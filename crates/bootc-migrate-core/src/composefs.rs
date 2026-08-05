use anyhow::{Context, Result, anyhow};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::OnceLock;

/// The system composefs store. During migration this is writable: on btrfs it is
/// a plain directory on `/sysroot`; on XFS it is the ext4-verity loopback mounted
/// here by `setup_composefs_loopback_if_needed`.
const STORE: &str = "/sysroot/composefs";

/// Operations on the system composefs store, abstracted behind a trait so
/// library consumers can swap implementations: the real `bootc internals cfs`
/// CLI ([`BootcCliStore`]), an in-memory mock for tests/dry-runs
/// ([`MockComposefsStore`]), or eventually a native composefs-rs backend (#13).
///
/// Methods are the *logical* store operations, not a mirror of the bootc CLI
/// surface, so alternative backends can implement them without shelling out.
pub trait ComposefsStore: std::fmt::Debug {
    /// Pull `image_ref` into the store; returns the pull output (digests).
    fn pull_image(&self, target_image: &str, image_ref: &str) -> Result<String>;
    /// Create the composefs EROFS image for `image_id`; returns its fs-verity digest.
    fn create_image(&self, target_image: &str, image_id: &str) -> Result<String>;
    /// Seal `image_id`; returns the seal output (sealed config digest).
    fn seal_image(&self, target_image: &str, image_id: &str) -> Result<String>;
    /// Confirm the finished store is readable by the target image's bootc.
    fn verify_store_target_readable(&self, target_image: &str) -> Result<()>;
}

/// The real implementation: drives `bootc internals cfs` — natively when the
/// host bootc still ships the legacy cfs CLI, otherwise via a legacy-CLI bootc
/// run out of a container image: the target image when possible, else a pinned
/// legacy builder (see [`ComposefsStore::pull_image`] and issue #72).
#[derive(Debug, Default)]
pub struct BootcCliStore {
    /// The bootc that wrote the store, so `create_image`/`seal_image` use the
    /// same one. `None` = the host bootc; `Some(image)` = a legacy-CLI bootc
    /// run out of `image` via podman (the target image when its bootc is
    /// legacy, otherwise a pinned legacy builder — see issue #72). Set by
    /// `pull_image`.
    delegate_image: OnceLock<Option<String>>,
}

/// In-memory store for unit tests and dry-run pipelines: records every call
/// and returns canned digests without touching the system.
#[derive(Debug, Default)]
pub struct MockComposefsStore {
    /// Call log: one entry per method invocation.
    pub calls: std::sync::Mutex<Vec<String>>,
}

impl ComposefsStore for MockComposefsStore {
    fn pull_image(&self, _target_image: &str, image_ref: &str) -> Result<String> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("pull_image {image_ref}"));
        Ok(
            "manifest sha256:0000000000000000000000000000000000000000000000000000000000000000
            config sha256:1111111111111111111111111111111111111111111111111111111111111111
"
            .to_string(),
        )
    }
    fn create_image(&self, _target_image: &str, image_id: &str) -> Result<String> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("create_image {image_id}"));
        Ok("2222222222222222222222222222222222222222222222222222222222222222            2222222222222222222222222222222222222222222222222222222222222222"
            .to_string())
    }
    fn seal_image(&self, _target_image: &str, image_id: &str) -> Result<String> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("seal_image {image_id}"));
        Ok(
            "sha256:3333333333333333333333333333333333333333333333333333333333333333
"
            .to_string(),
        )
    }
    fn verify_store_target_readable(&self, _target_image: &str) -> Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push("verify_store_target_readable".to_string());
        Ok(())
    }
}

/// Run `bootc internals cfs --system oci <args>` with the host (source) bootc.
/// This is the fast path: it runs natively, so on btrfs it reflinks the image's
/// blobs into the store (near-zero extra disk) with no container overhead.
fn host_cfs_oci(args: &[&str]) -> Result<Output> {
    Command::new("bootc")
        .args(["internals", "cfs", "--system", "oci"])
        .args(args)
        .output()
        .context("failed to execute host bootc internals cfs oci")
}

/// Run `bootc internals cfs --repo <store> <args>` with the bootc shipped in
/// `bootc_image` (via podman). Used when the host bootc cannot write a store
/// the migrated system can consume — either too old (no oci-manifest stream)
/// or too new (cfs CLI without create-image/seal, issue #72). The host
/// container storage is bind-mounted so target layers come from the
/// manifest-pinned local cache that Phase 2 refreshed rather than being
/// re-downloaded.
fn image_cfs(bootc_image: &str, args: &[&str]) -> Result<Output> {
    Command::new("podman")
        .args([
            "run",
            "--rm",
            "--privileged",
            "--net=host",
            "--security-opt",
            "label=disable",
            "-v",
        ])
        .arg(format!("{STORE}:{STORE}"))
        .arg("-v")
        .arg("/var/lib/containers/storage:/var/lib/containers/storage")
        .arg(bootc_image)
        .args(["bootc", "internals", "cfs", "--repo", STORE])
        .args(args)
        .output()
        .context("failed to execute target-image bootc via podman")
}

/// True if the store has at least one `oci-manifest-*` stream — the marker that a
/// pull produced a deployment the target bootc can read (`bootc status`/`upgrade`
/// open `streams/oci-manifest-<digest>`). Older bootc (≤1.13) writes config+layer
/// streams but no manifest stream.
fn manifest_stream_present() -> bool {
    std::fs::read_dir(format!("{STORE}/streams"))
        .map(|d| {
            d.filter_map(|e| e.ok())
                .any(|e| e.file_name().to_string_lossy().starts_with("oci-manifest-"))
        })
        .unwrap_or(false)
}

/// Remove the store's content so it can be rebuilt cleanly with the target bootc.
/// (The target bootc can't reconcile a store an older bootc already wrote —
/// "Expected exactly 1 external object in splitstream" — so a partial reuse fails.)
fn wipe_store() -> Result<()> {
    for sub in ["objects", "streams", "images"] {
        let p = format!("{STORE}/{sub}");
        if Path::new(&p).exists() {
            std::fs::remove_dir_all(&p).with_context(|| format!("wiping {p}"))?;
        }
    }
    // Drop the (host-written) meta.json so the target bootc init writes its own.
    let _ = std::fs::remove_file(format!("{STORE}/meta.json"));
    Ok(())
}

/// Whether a `bootc internals cfs … oci --help` output belongs to the legacy
/// CLI generation (has `create-image`/`seal`). Newer composefs-rs removed both
/// (creation folded into `pull --bootable`/`prepare-boot`, sealing implicit) —
/// see issue #72. Malformed/failed probes count as legacy so behavior on old
/// hosts is unchanged.
fn help_is_legacy_cli(help: &std::process::Output) -> bool {
    if !help.status.success() {
        return true;
    }
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&help.stdout),
        String::from_utf8_lossy(&help.stderr)
    );
    text.contains("create-image")
}

/// Probe the HOST bootc's cfs CLI generation.
pub(crate) fn host_cfs_is_legacy() -> bool {
    match host_cfs_oci(&["--help"]) {
        Ok(out) => help_is_legacy_cli(&out),
        Err(_) => true,
    }
}

/// Probe the cfs CLI generation of the bootc shipped in `bootc_image`.
/// Unlike the host probe, a *failed* probe here counts as NOT legacy: the
/// delegation path is only entered deliberately, and running the legacy
/// sequence against an unprobeable image produces confusing mid-phase errors.
fn image_cfs_is_legacy(bootc_image: &str) -> bool {
    match image_cfs(bootc_image, &["oci", "--help"]) {
        Ok(out) => out.status.success() && help_is_legacy_cli(&out),
        Err(_) => false,
    }
}

/// Pinned image whose bootc still ships the legacy cfs CLI, used as the store
/// builder when neither the host nor the target has it (issue #72). New bootc
/// reads legacy-format stores (proven by LTS→dakota E2E), so any legacy writer
/// produces a store the migrated system can consume. Override with
/// `BMC_CFS_BUILDER` when this pin ages out.
const DEFAULT_LEGACY_BUILDER: &str = "quay.io/fedora/fedora-bootc:42";

fn legacy_builder_image() -> String {
    std::env::var("BMC_CFS_BUILDER").unwrap_or_else(|_| DEFAULT_LEGACY_BUILDER.to_string())
}

/// Normalize an image reference to a `docker://` (or already-prefixed) transport.
fn with_transport(image_ref: &str) -> String {
    if image_ref.contains("://") {
        image_ref.to_string()
    } else {
        format!("docker://{}", image_ref)
    }
}

fn is_digest_pinned_image_ref(image_ref: &str) -> bool {
    let image_ref = image_ref.strip_prefix("docker://").unwrap_or(image_ref);
    image_ref
        .split_once('@')
        .is_some_and(|(_, digest)| digest.starts_with("sha256:"))
}

impl ComposefsStore for BootcCliStore {
    fn pull_image(&self, target_image: &str, image_ref: &str) -> Result<String> {
        let image_ref = image_ref.strip_prefix("docker://").unwrap_or(image_ref);
        let docker_ref = with_transport(image_ref);

        // The host bootc is only usable when it still speaks the legacy cfs
        // CLI (`create-image`/`seal`); newer composefs-rs removed both (#72),
        // so a new-generation host must delegate the whole sequence to the
        // target image's bootc below.
        let host_legacy = host_cfs_is_legacy();
        if !host_legacy {
            eprintln!(
                "[cfs] host bootc ships the new cfs CLI (no create-image/seal — \
                 see issue #72); building the store with the target image's bootc"
            );
        }

        // Fast path: pull with the host (source) bootc. On a current source this
        // reflinks blobs into the store and produces a target-readable deployment.
        if host_legacy {
            let host_out = host_cfs_oci(&["pull", &docker_ref])?;
            if host_out.status.success() && manifest_stream_present() {
                let _ = self.delegate_image.set(None);
                return Ok(String::from_utf8_lossy(&host_out.stdout).to_string());
            }

            // The host bootc is too old: its pull either failed or produced no
            // oci-manifest stream, which leaves `bootc status`/`upgrade` broken
            // after an otherwise-successful migration. Rebuild the store with
            // the target image's own bootc so it's written in the format the
            // migrated system expects.
            if host_out.status.success() {
                eprintln!(
                    "[cfs] host bootc produced no oci-manifest stream (too old for the \
                     target); rebuilding the store with the target image's bootc"
                );
            } else {
                eprintln!(
                    "[cfs] host bootc pull failed ({}); rebuilding the store with the \
                     target image's bootc",
                    String::from_utf8_lossy(&host_out.stderr).trim()
                );
            }
        }

        // Everything below needs a bootc that still speaks the legacy CLI.
        // Prefer the target image's own bootc (store written by its runtime
        // reader); otherwise fall back to a pinned legacy builder — new bootc
        // reads legacy stores, so any legacy writer is correct (#72).
        let delegate = if image_cfs_is_legacy(image_ref) {
            image_ref.to_string()
        } else {
            let builder = legacy_builder_image();
            eprintln!(
                "[cfs] target image's bootc also ships the new cfs CLI; \
                 building the store with legacy builder {builder}"
            );
            if !image_cfs_is_legacy(&builder) {
                return Err(anyhow!(
                    "no legacy-CLI bootc available: host, target ({target_image}), and \
                     builder ({builder}) all lack `oci create-image`/`seal`. Set \
                     BMC_CFS_BUILDER to an image whose bootc still ships the legacy cfs \
                     CLI, or wait for the native composefs-rs backend — see \
                     https://github.com/tuna-os/bootc-migrate/issues/72"
                ));
            }
            builder
        };

        wipe_store()?;
        // cfsctl has no `init` subcommand (verified empirically: `bootc
        // internals cfs help` lists no such command, and running it errors
        // "unrecognized subcommand 'init'" even on the legacy generation —
        // this call was never valid and broke every run through the
        // rebuild path, i.e. every new-gen-host migration). The repo is
        // auto-initialized on first write by `oci pull` as long as the
        // directory exists; `wipe_store` clears its contents but not the
        // directory itself, so this is just a defensive ensure.
        std::fs::create_dir_all(STORE)
            .with_context(|| format!("creating composefs store directory {STORE}"))?;

        // Only import from local storage when Phase 2 provided an immutable
        // manifest reference. A mutable tag here can silently select an older
        // cached image, which would make the ComposeFS root differ from the
        // deployment metadata and boot artifacts.
        let out = if is_digest_pinned_image_ref(image_ref) {
            let cs_ref = format!("containers-storage:{image_ref}");
            let local_out = image_cfs(&delegate, &["oci", "pull", &cs_ref])?;
            if local_out.status.success() {
                local_out
            } else {
                eprintln!(
                    "[cfs] containers-storage pull failed ({}); retrying via registry",
                    String::from_utf8_lossy(&local_out.stderr).trim()
                );
                image_cfs(&delegate, &["oci", "pull", &docker_ref])?
            }
        } else {
            image_cfs(&delegate, &["oci", "pull", &docker_ref])?
        };
        if !out.status.success() {
            return Err(anyhow!(
                "pull failed: {}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        let _ = self.delegate_image.set(Some(delegate));
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    fn create_image(&self, _target_image: &str, image_id: &str) -> Result<String> {
        let output = match self.delegate_image.get().and_then(|d| d.as_deref()) {
            Some(delegate) => image_cfs(delegate, &["oci", "create-image", image_id])?,
            None => host_cfs_oci(&["create-image", image_id])?,
        };
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!("create-image failed: {}", stderr));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Post-build verification (#3): confirm the finished store is readable by the
    /// TARGET image's bootc — the binary that reads it at runtime — so a bootc
    /// format skew can never silently ship a migration that boots but breaks
    /// `bootc status`/`bootc upgrade`.
    ///
    /// How we know it's readable:
    /// - In the rebuild path (`use_target()`), the **target bootc itself** wrote the
    ///   store (its pull/create/seal succeeded), so it is target-readable by
    ///   construction.
    /// - In the host path, we assert the `oci-manifest-*` stream is present — the
    ///   exact artifact the target's status/upgrade open
    ///   (`streams/oci-manifest-<digest>`). Its absence is the known symptom of an
    ///   older source bootc and *guarantees* breakage.
    ///
    /// Either way, a missing manifest stream here is a hard failure with a clear
    /// message — we refuse to declare success on a store the target can't read.
    /// This also future-proofs us: if a later bootc changes the format such that
    /// neither path produces the stream, the migration fails loudly instead of
    /// silently regressing updates.
    fn verify_store_target_readable(&self, target_image: &str) -> Result<()> {
        if manifest_stream_present() {
            return Ok(());
        }
        Err(anyhow!(
            "post-build verification failed: the composefs store has no oci-manifest \
         stream, so the target image's bootc ({target_image}) cannot read this \
         deployment — `bootc status`/`bootc upgrade` would break after reboot. \
         This is a bootc format incompatibility between the source bootc and the \
         target that was not resolved by rebuilding the store. Refusing to ship a \
         silently-broken migration; update the source system's bootc and retry."
        ))
    }

    fn seal_image(&self, _target_image: &str, image_id: &str) -> Result<String> {
        let output = match self.delegate_image.get().and_then(|d| d.as_deref()) {
            Some(delegate) => image_cfs(delegate, &["oci", "seal", image_id])?,
            None => host_cfs_oci(&["seal", image_id])?,
        };
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!("seal failed: {}", stderr));
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use std::process::{ExitStatus, Output};

    fn fake_output(code: i32, stdout: &str) -> Output {
        Output {
            status: ExitStatus::from_raw(code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    #[test]
    fn legacy_cli_detected_by_create_image() {
        let help = fake_output(0, "Commands:\n  pull\n  create-image\n  seal\n  mount\n");
        assert!(help_is_legacy_cli(&help));
    }

    #[test]
    fn new_cli_detected_by_missing_create_image() {
        let help = fake_output(
            0,
            "Commands:\n  pull\n  compute-id\n  prepare-boot\n  mount\n  fsck\n",
        );
        assert!(!help_is_legacy_cli(&help));
    }

    #[test]
    fn failed_probe_counts_as_legacy() {
        // A failed --help (old bootc without the subcommand, sandboxing, etc.)
        // must not change behavior on old hosts.
        let help = fake_output(256, "");
        assert!(help_is_legacy_cli(&help));
    }

    #[test]
    fn only_digest_pinned_references_may_use_local_storage() {
        assert!(is_digest_pinned_image_ref(
            "ghcr.io/projectbluefin/dakota@sha256:0123456789abcdef"
        ));
        assert!(is_digest_pinned_image_ref(
            "docker://ghcr.io/projectbluefin/dakota@sha256:0123456789abcdef"
        ));
        assert!(!is_digest_pinned_image_ref(
            "ghcr.io/projectbluefin/dakota:testing"
        ));
    }
}
