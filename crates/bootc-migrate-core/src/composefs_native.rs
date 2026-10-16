//! Native composefs-rs store backend (issue #13), behind the
//! `composefs-native` feature.
//!
//! [`NativeStore`] implements [`ComposefsStore`] with the `composefs` /
//! `composefs-oci` crates directly instead of shelling out to `bootc
//! internals cfs`. This is the permanent answer to the upstream CLI drift in
//! issue #72 (the `oci create-image` / `oci seal` subcommands were removed
//! from new-generation bootc): no CLI to drift against, and digests come back
//! as typed values instead of scraped stdout.
//!
//! ## Generation matrix
//!
//! The store format is defined by the bootc that *reads* it at boot — the
//! target image's. This backend writes the new-generation (composefs-rs
//! ≥ 0.7) store model, so it is the right writer when the target ships a
//! new-generation bootc. Legacy-generation targets keep using
//! [`BootcCliStore`](crate::composefs::BootcCliStore) delegation. Selection
//! is by the target-generation probe introduced for issue #72.
//!
//! ## String contracts
//!
//! The trait predates typed digests, so methods return the same line formats
//! the CLI printed and the phase code parses:
//! - `pull_image` → `manifest <sha256:…>\nconfig <sha256:…>`
//! - `create_image` → the EROFS image's fs-verity digest (bare hex)
//! - `seal_image` → `config <sha256:…>` (the sealed config digest)

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use composefs::fsverity::{FsVerityHashValue, Sha512HashValue};
use composefs::repository::{Repository, RepositoryConfig};
use composefs::tree::FileSystem;
use composefs_oci::{PullOptions, image::create_filesystem, open_config, pull};

use crate::composefs::ComposefsStore;

/// The fs-verity flavour of the store: **sha512**, matching what bootc's cfs
/// CLI writes — `VerityDigest` carries a `sha512:` prefix in `.origin` /
/// `.imginfo`, and `composefs=` takes the bare sha512 hex. A sha256 repo here
/// would produce object IDs the rest of the pipeline (and the target's bootc
/// at boot) cannot resolve.
type ObjectId = Sha512HashValue;

/// Config label that marks an OCI image as sealed, holding the EROFS image's
/// fs-verity object ID. This is what new-generation `oci fsck` checks (the
/// composefs-oci sealing model — there is no discrete `seal` subcommand).
const SEAL_LABEL: &str = "containers.composefs.fsverity";

/// A [`ComposefsStore`] that writes the store with composefs-rs natively.
#[derive(Debug)]
pub struct NativeStore {
    /// Filesystem path of the composefs repository (`/sysroot/composefs` on a
    /// real migration).
    repo_path: PathBuf,
}

impl NativeStore {
    /// A store at an explicit repository path (tests, staging roots).
    pub fn new(repo_path: impl Into<PathBuf>) -> Self {
        Self {
            repo_path: repo_path.into(),
        }
    }

    /// The store at the system location the migration pipeline uses.
    pub fn system() -> Self {
        Self::new("/sysroot/composefs")
    }

    /// Open the repository, creating it if absent and upgrading
    /// pre-`meta.json` repositories in place (the non-destructive path for
    /// stores written by a legacy bootc CLI).
    ///
    /// Discriminates on [`repo_exists`], not bare path existence: on a real
    /// migration `/sysroot/composefs` is typically a pre-created (or
    /// loopback-mounted, on XFS) **empty** directory before store init, and
    /// an empty dir must take the init path, not the upgrade path.
    fn repo(&self) -> Result<Arc<Repository<ObjectId>>> {
        let cwd = rustix::fs::CWD;
        let repo = if repo_exists(&self.repo_path) {
            let (repo, _upgraded) = Repository::open_upgrade(cwd, &self.repo_path)
                .with_context(|| format!("opening composefs repo at {:?}", self.repo_path))?;
            repo
        } else {
            let algorithm = composefs::fsverity::Algorithm::for_hash::<ObjectId>();
            match Repository::init_path(cwd, &self.repo_path, RepositoryConfig::new(algorithm)) {
                Ok((repo, _created)) => repo,
                // The filesystem may not support fs-verity (test tmpdirs, or
                // hosts without the feature). The repo still works in insecure
                // mode — objects are unverified at read time, matching what a
                // non-verity legacy store gave us. Boot-time sealing strength
                // is a target-image concern, not a store-write concern.
                Err(verity_err) => {
                    let config = RepositoryConfig::new(algorithm).set_insecure();
                    let (repo, _created) = Repository::init_path(cwd, &self.repo_path, config)
                        .with_context(|| {
                            format!(
                                "initializing composefs repo at {:?} \
                                 (fs-verity init also failed: {verity_err:#})",
                                self.repo_path
                            )
                        })?;
                    repo
                }
            }
        };
        Ok(Arc::new(repo))
    }

    /// Build the EROFS filesystem for a pulled config and commit it to the
    /// repo, returning its fs-verity object ID. Shared by `create_image` and
    /// the build-if-missing path in `seal_image`.
    fn commit_erofs(
        &self,
        repo: &Arc<Repository<ObjectId>>,
        config_digest: &composefs_oci::OciDigest,
    ) -> Result<ObjectId> {
        // Default transform options = the historical allowlist-only xattr
        // filtering, i.e. the digest composefs-oci 0.7 produced.
        let fs: FileSystem<ObjectId> = create_filesystem(
            repo,
            config_digest,
            None,
            &composefs_oci::OciTransformOptions::default(),
        )
        .context("building composefs filesystem from OCI layers")?;
        fs.commit_image(repo, None)
            .context("committing EROFS image to the repo")
    }
}

/// Normalize an image reference to a skopeo transport (`docker://` unless a
/// transport is already present). `containers-storage:` is the one common
/// transport spelled without `//` — it triggers the crate's native zero-copy
/// local import path, so it must pass through untouched.
fn with_transport(image_ref: &str) -> String {
    if image_ref.contains("://") || image_ref.starts_with("containers-storage:") {
        image_ref.to_string()
    } else {
        format!("docker://{image_ref}")
    }
}

/// Parse a `sha256:…` string into a typed OCI digest.
fn oci_digest(s: &str) -> Result<composefs_oci::OciDigest> {
    s.parse()
        .map_err(|e| anyhow::anyhow!("invalid OCI digest {s:?}: {e}"))
}

/// A small single-threaded runtime for the async composefs-oci entry points
/// (`pull`, fsck). The pipeline itself is synchronous.
fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building tokio runtime for composefs-oci")
}

impl ComposefsStore for NativeStore {
    fn pull_image(&self, _target_image: &str, image_ref: &str) -> Result<String> {
        let repo = self.repo()?;
        let transport_ref = with_transport(image_ref);
        let result = runtime()?
            .block_on(pull(
                &repo,
                &transport_ref,
                Some(image_ref),
                PullOptions::default(),
            ))
            .with_context(|| format!("pulling {transport_ref} into the composefs repo"))?;
        // Same line format the legacy CLI printed; parse_pull_digests reads it.
        Ok(format!(
            "manifest {}\nconfig {}",
            result.manifest_digest, result.config_digest
        ))
    }

    fn create_image(&self, _target_image: &str, image_id: &str) -> Result<String> {
        let repo = self.repo()?;
        let config_digest = oci_digest(image_id)?;

        // Build the EROFS *and link it* into the config+manifest splitstreams
        // (`upgrade_repo` covers every tagged image; `pull_image` always
        // tags). The linkage is what new-generation `oci mount` resolves —
        // a bare committed EROFS mounts fine by sealed-config digest on a
        // legacy host but fails on a new-gen host with "No composefs EROFS
        // image linked" (verified empirically against fedora-bootc:44).
        composefs_oci::upgrade_repo(&repo)
            .context("linking EROFS images into the OCI splitstreams")?;

        let oc = open_config(&repo, &config_digest, None)
            .with_context(|| format!("opening pulled config {image_id}"))?;
        match oc.image_ref.or(oc.image_ref_v1) {
            Some(erofs) => Ok(erofs.to_hex()),
            // Untagged image (not reachable via pull_image, but the trait
            // allows it): commit the EROFS directly. Legacy-identifier mounts
            // still work; only new-gen tag/manifest mounts need the linkage.
            None => Ok(self.commit_erofs(&repo, &config_digest)?.to_hex()),
        }
    }

    fn seal_image(&self, _target_image: &str, image_id: &str) -> Result<String> {
        let repo = self.repo()?;
        let config_digest = oci_digest(image_id)?;
        let oc = open_config(&repo, &config_digest, None)
            .with_context(|| format!("opening pulled config {image_id}"))?;

        // The EROFS image to embed: reuse the one linked to the config if
        // create_image already ran, else build it now.
        let erofs = match oc.image_ref.clone().or_else(|| oc.image_ref_v1.clone()) {
            Some(id) => id,
            None => self.commit_erofs(&repo, &config_digest)?,
        };

        // Seal = clone the config with the fs-verity label and write it back
        // as a new config splitstream carrying the EROFS named ref. The
        // returned digest is the *sealed* config identifier the pipeline
        // records for boot-time lookup — distinct from the pulled config.
        let mut sealed = oc.config;
        let mut inner = sealed.config().clone().unwrap_or_default();
        let mut labels = inner.labels().clone().unwrap_or_default();
        labels.insert(SEAL_LABEL.to_string(), erofs.to_id());
        inner.set_labels(Some(labels));
        sealed.set_config(Some(inner));

        let (sealed_digest, _sealed_verity) = composefs_oci::write_config(
            &repo,
            &sealed,
            oc.layer_refs,
            Some(&erofs),
            None,
            // Keep any cached boot EROFS refs; an empty map would drop them.
            &oc.boot_image_refs,
        )
        .context("writing sealed config splitstream")?;

        Ok(format!("config {sealed_digest}"))
    }

    fn verify_store_target_readable(&self, _target_image: &str) -> Result<()> {
        // Native analog of the CLI store's check: the repository must reopen
        // cleanly (meta.json parses, format is compatible) and its OCI refs
        // must enumerate. A full byte-level fsck would re-hash every object of
        // a multi-GB store — far too slow for a migration phase. Cross-checking
        // against the *target's* bootc binary stays meaningful only for the
        // CLI store; for a new-generation target this store IS the target's
        // native format (see the generation matrix in the module docs).
        let repo = self.repo()?;
        composefs_oci::oci_image::list_refs(&repo).context("enumerating OCI refs in the store")?;
        Ok(())
    }
}

/// True if `repo_path` looks like an initialized composefs repository. Used
/// by store selection to distinguish "fresh migration" from "resume".
pub fn repo_exists(repo_path: &Path) -> bool {
    repo_path.join("objects").is_dir() || repo_path.join("meta.json").is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `sha256:<hex>` for `bytes`, the OCI digest form.
    fn sha256_ref(bytes: &[u8]) -> String {
        use sha2::Digest as _;
        let hex: String = sha2::Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        format!("sha256:{hex}")
    }

    /// A minimal POSIX ustar archive: `usr/` and one file under it (the
    /// filesystem builder copies root metadata from `/usr`, so the layer must
    /// contain it). Hand-rolled so the test needs no tar dependency.
    fn minimal_tar() -> Vec<u8> {
        fn header(name: &str, size: usize, typeflag: u8) -> [u8; 512] {
            let mut h = [0u8; 512];
            h[..name.len()].copy_from_slice(name.as_bytes());
            h[100..108].copy_from_slice(b"0000755\0");
            h[108..116].copy_from_slice(b"0000000\0");
            h[116..124].copy_from_slice(b"0000000\0");
            h[124..136].copy_from_slice(format!("{size:011o}\0").as_bytes());
            h[136..148].copy_from_slice(b"00000000000\0");
            h[156] = typeflag;
            h[257..263].copy_from_slice(b"ustar\0");
            h[263..265].copy_from_slice(b"00");
            // Checksum: field counts as spaces while summing.
            h[148..156].copy_from_slice(b"        ");
            let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
            h[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
            h[155] = b' ';
            h
        }
        let content = b"hello from the native store test\n";
        let mut tar = Vec::new();
        tar.extend_from_slice(&header("usr/", 0, b'5'));
        tar.extend_from_slice(&header("usr/hello.txt", content.len(), b'0'));
        tar.extend_from_slice(content);
        tar.resize(tar.len().next_multiple_of(512), 0);
        tar.resize(tar.len() + 1024, 0); // end-of-archive blocks
        tar
    }

    /// Full create+seal integration against a real repository: fabricate a
    /// one-layer OCI image via the crate's own import/write APIs, then drive
    /// the trait methods the way phase 3 does.
    #[test]
    fn create_and_seal_against_a_real_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = NativeStore::new(dir.path().join("repo"));
        let repo = store.repo().unwrap();

        // Layer: import the tar under its diff_id (sha256 of the bytes).
        let tar = minimal_tar();
        let diff_id = sha256_ref(&tar);
        let (layer_verity, _stats) = runtime()
            .unwrap()
            .block_on(composefs_oci::import_layer(
                &repo,
                &oci_digest(&diff_id).unwrap(),
                None,
                tar.as_slice(),
            ))
            .unwrap();

        // Config: minimal OCI image configuration referencing that layer.
        let config_json = format!(
            r#"{{"architecture":"amd64","os":"linux","config":{{}},"rootfs":{{"type":"layers","diff_ids":["{diff_id}"]}}}}"#
        );
        let mut refs = std::collections::HashMap::new();
        refs.insert(diff_id.clone().into_boxed_str(), layer_verity);
        let (config_digest, config_verity) = composefs_oci::write_config_raw(
            &repo,
            config_json.as_bytes(),
            refs,
            None,
            None,
            &std::collections::HashMap::new(),
        )
        .unwrap();
        let config_digest = config_digest.to_string();

        // Manifest + tag: fabricated through the public low-level stream API
        // (the typed `write_manifest` wants oci-spec types this crate doesn't
        // depend on). Tagging matters — `create_image` links EROFS images via
        // `upgrade_repo`, which walks tagged images.
        let manifest_json = format!(
            r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{config_digest}","size":{config_size}}},"layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":"{diff_id}","size":{tar_size}}}]}}"#,
            config_size = config_json.len(),
            tar_size = tar.len(),
        );
        let manifest_digest = oci_digest(&sha256_ref(manifest_json.as_bytes())).unwrap();
        // The crate-private content-type tag for manifest splitstreams.
        let manifest_content_type: u64 = u64::from_le_bytes(*b"ocimanif");
        let mut stream = repo.create_stream(manifest_content_type).unwrap();
        stream.add_named_stream_ref(&format!("config:{config_digest}"), &config_verity);
        stream.write_external(manifest_json.as_bytes()).unwrap();
        repo.write_stream(
            stream,
            &composefs_oci::oci_image::manifest_identifier(&manifest_digest),
            None,
        )
        .unwrap();
        composefs_oci::oci_image::tag_image(&repo, &manifest_digest, "xgen-test").unwrap();

        // Phase-3 shape: create_image returns the EROFS fs-verity digest as
        // bare sha512 hex (what VerityDigest/`composefs=` expect).
        let verity_hex = store.create_image("unused", &config_digest).unwrap();

        // The EROFS must be *linked* into the original config splitstream —
        // the exact thing new-generation `oci mount` resolves (its absence is
        // the "No composefs EROFS image linked" refusal seen with
        // legacy-CLI-written stores).
        let oc =
            composefs_oci::open_config(&repo, &oci_digest(&config_digest).unwrap(), None).unwrap();
        assert!(
            oc.image_ref.is_some() || oc.image_ref_v1.is_some(),
            "EROFS not linked into the config splitstream"
        );
        assert_eq!(verity_hex.len(), 128, "sha512 fs-verity digest expected");
        assert!(verity_hex.chars().all(|c| c.is_ascii_hexdigit()));
        // Idempotent: a second run re-derives the same object.
        assert_eq!(
            store.create_image("unused", &config_digest).unwrap(),
            verity_hex
        );

        // seal returns the *sealed* config digest line — a different digest
        // than the pulled config (it gained the fs-verity label).
        let seal_out = store.seal_image("unused", &config_digest).unwrap();
        let sealed = seal_out.strip_prefix("config ").unwrap();
        assert!(sealed.starts_with("sha256:"), "sealed digest: {sealed}");
        assert_ne!(sealed, config_digest);

        // And the store still verifies clean.
        store.verify_store_target_readable("unused").unwrap();
    }

    #[test]
    fn with_transport_adds_docker_prefix() {
        assert_eq!(
            with_transport("ghcr.io/projectbluefin/dakota:stable"),
            "docker://ghcr.io/projectbluefin/dakota:stable"
        );
        assert_eq!(
            with_transport("containers-storage:localhost/x"),
            "containers-storage:localhost/x"
        );
    }

    #[test]
    fn oci_digest_rejects_garbage() {
        assert!(oci_digest("not-a-digest").is_err());
        assert!(
            oci_digest("sha256:0000000000000000000000000000000000000000000000000000000000000000")
                .is_ok()
        );
    }

    #[test]
    fn store_initializes_a_fresh_repo() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repo");
        let store = NativeStore::new(&path);
        assert!(!repo_exists(&path));
        // repo() creates the repository directory + meta.json.
        store.repo().unwrap();
        assert!(repo_exists(&path));
        // And a second open takes the open_upgrade path cleanly.
        store.repo().unwrap();
    }

    #[test]
    fn store_initializes_when_dir_exists_but_is_empty() {
        // /sysroot/composefs is typically a pre-created (or loopback-mounted)
        // empty directory before store init — this must init, not try to
        // "upgrade" a repo that was never there.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repo");
        std::fs::create_dir(&path).unwrap();
        let store = NativeStore::new(&path);
        store.repo().unwrap();
        assert!(repo_exists(&path));
    }

    #[test]
    fn verify_on_empty_store_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let store = NativeStore::new(dir.path().join("repo"));
        store.verify_store_target_readable("unused").unwrap();
    }

    #[test]
    fn create_image_on_unknown_config_errors_cleanly() {
        // Phase 3 hands us a config digest from Phase 2; if the store lost it
        // (wiped repo, wrong path) this must be a contextual error, not a
        // panic or a bogus digest.
        let dir = tempfile::tempdir().unwrap();
        let store = NativeStore::new(dir.path().join("repo"));
        let err = store
            .create_image(
                "unused",
                "sha256:1111111111111111111111111111111111111111111111111111111111111111",
            )
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("sha256:1111"),
            "error should name the missing config: {err:#}"
        );
    }

    #[test]
    fn seal_image_on_unknown_config_errors_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let store = NativeStore::new(dir.path().join("repo"));
        let err = store
            .seal_image(
                "unused",
                "sha256:2222222222222222222222222222222222222222222222222222222222222222",
            )
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("sha256:2222"),
            "error should name the missing config: {err:#}"
        );
    }

    #[test]
    fn create_image_rejects_a_bare_verity_hex() {
        // Guard against the verity-vs-config-digest confusion the glossary
        // warns about: a bare 64-hex verity digest is not an OCI config
        // digest and must be rejected at the boundary, silently looking up
        // nothing.
        let dir = tempfile::tempdir().unwrap();
        let store = NativeStore::new(dir.path().join("repo"));
        assert!(
            store
                .create_image(
                    "unused",
                    "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
                )
                .is_err()
        );
    }
}
