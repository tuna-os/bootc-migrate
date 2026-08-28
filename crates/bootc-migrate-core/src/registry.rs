//! Disk-bounded extraction of files from OCI registry images.
//!
//! Downloads image layers one at a time — fetch → extract needed files →
//! delete blob → repeat — so peak disk usage is bounded by the largest
//! single layer instead of the whole image (see docs/architecture.md §1–2).
//! This avoids `podman pull`-ing multi-GB images when only a few files
//! (kernel, initrd, bootloader binaries, kernel modules) are needed, and
//! works around composefs EROFS mounts zero-filling large file content.
//!
//! Base- and direction-agnostic: operates on any OCI image reference.

use anyhow::{Context, Result, anyhow};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Where layer blobs are staged while they are being unpacked. `/var/tmp` is
/// disk-backed (unlike `/tmp`, which is a tmpfs on bootc systems and far too
/// small for a multi-hundred-MB layer).
const LAYER_SCRATCH_DIR: &str = "/var/tmp";

pub fn extract_files_via_registry(image_ref: &str, files: &[(&Path, &Path)]) -> Result<()> {
    let endpoint = RegistryEndpoint::resolve(image_ref)?;
    let manifest_json = endpoint.fetch_manifest(&endpoint.reference)?;

    // Manifest list / OCI index → resolve to current-arch manifest.
    let layers_manifest = if endpoint.is_manifest_index(&manifest_json) {
        let arch = match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            other => other,
        };
        let entries = manifest_json
            .get("manifests")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("manifest index has no manifests array"))?;
        let pick = entries
            .iter()
            .find(|m| {
                m.get("platform")
                    .and_then(|p| p.get("architecture"))
                    .and_then(|a| a.as_str())
                    == Some(arch)
            })
            .ok_or_else(|| anyhow!("manifest index has no entry for arch {}", arch))?;
        let digest = pick
            .get("digest")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("manifest index entry has no digest"))?;
        endpoint.fetch_manifest(digest)?
    } else {
        manifest_json
    };

    let layers = layers_manifest
        .get("layers")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("image manifest has no layers array"))?;

    let scratch = tempfile::Builder::new()
        .prefix("bootc-migrate-extract-")
        .tempdir_in(LAYER_SCRATCH_DIR)
        .context("failed to create /var/tmp scratch dir for layer streaming")?;

    let mut remaining: Vec<(&Path, &Path)> = files.to_vec();
    for layer in layers.iter().rev() {
        if remaining.is_empty() {
            break;
        }
        let digest = layer
            .get("digest")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("layer entry has no digest"))?;

        // Download just this one layer, extract from it, drop it.
        let blob_path = scratch.path().join("layer.blob");
        endpoint
            .download_blob(digest, &blob_path)
            .with_context(|| format!("failed to fetch layer {}", digest))?;

        let mut still_needed: Vec<(&Path, &Path)> = Vec::new();
        for (src, dst) in remaining.into_iter() {
            if extract_one_from_layer(&blob_path, src, dst)? {
                // satisfied
            } else {
                still_needed.push((src, dst));
            }
        }
        remaining = still_needed;
        let _ = fs::remove_file(&blob_path);
    }

    if !remaining.is_empty() {
        let missing: Vec<String> = remaining
            .iter()
            .map(|(s, _)| s.display().to_string())
            .collect();
        return Err(anyhow!(
            "target image is missing files: {}",
            missing.join(", ")
        ));
    }
    Ok(())
}

pub fn extract_subtree_via_registry(image_ref: &str, subtree: &str, dst_dir: &Path) -> Result<()> {
    let endpoint = RegistryEndpoint::resolve(image_ref)?;
    let manifest_json = endpoint.fetch_manifest(&endpoint.reference)?;
    let layers_manifest = if endpoint.is_manifest_index(&manifest_json) {
        let arch = match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            other => other,
        };
        let entries = manifest_json
            .get("manifests")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("manifest index has no manifests array"))?;
        let pick = entries
            .iter()
            .find(|m| {
                m.get("platform")
                    .and_then(|p| p.get("architecture"))
                    .and_then(|a| a.as_str())
                    == Some(arch)
            })
            .ok_or_else(|| anyhow!("manifest index has no entry for arch {}", arch))?;
        let digest = pick
            .get("digest")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("manifest index entry has no digest"))?;
        endpoint.fetch_manifest(digest)?
    } else {
        manifest_json
    };
    let layers = layers_manifest
        .get("layers")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("image manifest has no layers array"))?;

    let scratch = tempfile::Builder::new()
        .prefix("bootc-migrate-subtree-")
        .tempdir_in(LAYER_SCRATCH_DIR)
        .context("failed to create /var/tmp scratch dir for subtree streaming")?;

    fs::create_dir_all(dst_dir)
        .with_context(|| format!("failed to create subtree destination {}", dst_dir.display()))?;

    // Iterate oldest → newest so later writes win.
    for layer in layers.iter() {
        let digest = layer
            .get("digest")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("layer entry has no digest"))?;

        let blob_path = scratch.path().join("layer.blob");
        endpoint
            .download_blob(digest, &blob_path)
            .with_context(|| format!("failed to fetch layer {}", digest))?;

        // tar will silently produce no output if the prefix is absent in this layer.
        // --strip-components=1 drops the leading directory we asked for so the
        // contents land directly under dst_dir (we want dst_dir to be the merged
        // /etc, not dst_dir/etc).
        let normalized = subtree.trim_end_matches('/');
        for candidate in [format!("./{}", normalized), normalized.to_string()] {
            let _ = Command::new("tar")
                .args([
                    "-xaf",
                    blob_path
                        .to_str()
                        .ok_or_else(|| anyhow!("invalid blob path"))?,
                    "-C",
                    dst_dir
                        .to_str()
                        .ok_or_else(|| anyhow!("invalid dst path"))?,
                    "--overwrite",
                    "--no-same-owner",
                    "--strip-components=1",
                    &candidate,
                ])
                .stderr(std::process::Stdio::null())
                .status();
        }
        let _ = fs::remove_file(&blob_path);
    }
    Ok(())
}

/// Stream `image_ref`'s layers oldest → newest, extracting each of `paths`
/// (archive-relative, e.g. `usr/share/xsessions` — a file or a whole
/// directory) into `dst_root` with its full path preserved, so a caller
/// finds it back at `dst_root/<path>`.
///
/// Later layers overwrite earlier ones (`--overwrite`), matching the overlay
/// semantics of an OCI image. A path that is absent from a layer is not an
/// error — tar simply produces nothing — and a layer whose blob cannot be
/// fetched is skipped with a warning rather than aborting: this is a
/// best-effort inventory of a possibly-large image, and one unreachable blob
/// should degrade the result, not sink the whole probe. The caller decides
/// what a missing path means.
pub fn extract_paths_into_dir(image_ref: &str, paths: &[&str], dst_root: &Path) -> Result<()> {
    let endpoint = RegistryEndpoint::resolve(image_ref)?;
    let manifest_json = endpoint.fetch_manifest(&endpoint.reference)?;
    let layers_manifest = endpoint.arch_layers_manifest(manifest_json)?;
    let layers = layers_manifest
        .get("layers")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("image manifest has no layers array"))?;

    fs::create_dir_all(dst_root).with_context(|| {
        format!(
            "failed to create extract destination {}",
            dst_root.display()
        )
    })?;
    let scratch = tempfile::Builder::new()
        .prefix("bootc-migrate-paths-")
        .tempdir_in(LAYER_SCRATCH_DIR)
        .context("failed to create /var/tmp scratch dir for layer streaming")?;
    let blob_path = scratch.path().join("layer.blob");

    for layer in layers.iter() {
        let Some(digest) = layer.get("digest").and_then(|v| v.as_str()) else {
            eprintln!("Warning: skipping a layer of {image_ref} with no digest in its manifest");
            continue;
        };
        if let Err(e) = endpoint.download_blob(digest, &blob_path) {
            eprintln!("Warning: skipping layer {digest} of {image_ref}: {e:#}");
            continue;
        }

        for path in paths {
            // OCI layer tarballs store paths with or without a leading `./`;
            // try both. tar exits non-zero when the member is absent from
            // this layer, which is the common case, so its status is
            // deliberately not checked here.
            for candidate in [format!("./{path}"), (*path).to_string()] {
                let _ = Command::new("tar")
                    .arg("-xaf")
                    .arg(&blob_path)
                    .arg("-C")
                    .arg(dst_root)
                    .args(["--overwrite", "--no-same-owner"])
                    .arg(&candidate)
                    .stderr(std::process::Stdio::null())
                    .status();
            }
        }
        let _ = fs::remove_file(&blob_path);
    }
    Ok(())
}

/// Probe paths [`fetch_probe_files_via_registry`] pulls out of a target image.
const PROBE_PATHS: &[&str] = &[
    "usr/lib/os-release",
    "etc/os-release",
    "usr/lib/ostree/prepare-root.conf",
    "usr/lib/sysusers.d",
    "usr/share/xsessions",
    "usr/share/wayland-sessions",
    "usr/lib/systemd/boot/efi/systemd-bootx64.efi",
    "usr/bin/bootc",
    "usr/lib/bootc",
    "usr/lib/dracut/modules.d",
    "usr/lib/bootc/install",
];

/// Stream probe files for the target image from the registry without pulling full layers.
pub fn fetch_probe_files_via_registry(image_ref: &str) -> Result<crate::scan::ProbeFiles> {
    let scratch = tempfile::Builder::new()
        .prefix("bootc-migrate-scan-")
        .tempdir_in(LAYER_SCRATCH_DIR)
        .context("failed to create /var/tmp scratch dir for registry scan")?;
    extract_paths_into_dir(image_ref, PROBE_PATHS, scratch.path())?;

    let mut probe = crate::scan::ProbeFiles::default();

    let os_release_usr = scratch.path().join("usr/lib/os-release");
    let os_release_etc = scratch.path().join("etc/os-release");
    if os_release_usr.exists() {
        probe.os_release = fs::read_to_string(&os_release_usr).ok();
    } else if os_release_etc.exists() {
        probe.os_release = fs::read_to_string(&os_release_etc).ok();
    }

    let prep_path = scratch.path().join("usr/lib/ostree/prepare-root.conf");
    if prep_path.exists() {
        probe.prepare_root = fs::read_to_string(&prep_path).ok();
    }

    let sysusers_dir = scratch.path().join("usr/lib/sysusers.d");
    if sysusers_dir.is_dir()
        && let Ok(entries) = fs::read_dir(&sysusers_dir)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("conf")
                && let Ok(content) = fs::read_to_string(&path)
            {
                probe.sysusers.push(content);
            }
        }
    }

    let mut sessions = Vec::new();
    for sess_dir_name in ["usr/share/xsessions", "usr/share/wayland-sessions"] {
        let sess_dir = scratch.path().join(sess_dir_name);
        if sess_dir.is_dir()
            && let Ok(entries) = fs::read_dir(&sess_dir)
        {
            for entry in entries.flatten() {
                if let Some(fname) = entry.file_name().to_str() {
                    sessions.push(fname.to_string());
                }
            }
        }
    }
    probe.session_files = sessions;

    probe.has_systemd_boot_payload = scratch
        .path()
        .join("usr/lib/systemd/boot/efi/systemd-bootx64.efi")
        .exists();
    probe.has_bootc = scratch.path().join("usr/bin/bootc").exists()
        || scratch.path().join("usr/lib/bootc").exists();

    let dracut_modules_dir = scratch.path().join("usr/lib/dracut/modules.d");
    if dracut_modules_dir.is_dir()
        && let Ok(entries) = fs::read_dir(&dracut_modules_dir)
    {
        probe.dracut_module_dir_names = entries
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .collect();
    }

    let install_dir = scratch.path().join("usr/lib/bootc/install");
    if install_dir.is_dir()
        && let Ok(entries) = fs::read_dir(&install_dir)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("toml")
                && let Ok(content) = fs::read_to_string(&path)
            {
                probe.bootc_install_config = Some(content);
                break;
            }
        }
    }

    Ok(probe)
}

/// Extract the target image's kernel modules for `kver` from the registry into a
/// fresh /var/tmp directory, returning the tempdir guard plus the path to the
/// extracted `usr/lib/modules/<kver>` tree.
///
/// The composefs cfs mount used elsewhere in Phase 5 can fall back to a raw
/// EROFS mount, where files past the inline threshold (xfs.ko, modules.dep.bin,
/// …) read back as zeros — so dracut cannot rebuild an initrd from that mount.
/// The registry layer stream returns real bytes, so we source the modules tree
/// from there instead (the same mechanism that extracts vmlinuz + initrd).
pub fn extract_kernel_modules_via_registry(
    image_ref: &str,
    kver: &str,
) -> Result<(tempfile::TempDir, PathBuf)> {
    let endpoint = RegistryEndpoint::resolve(image_ref)?;
    let manifest_json = endpoint.fetch_manifest(&endpoint.reference)?;
    let layers_manifest = endpoint.arch_layers_manifest(manifest_json)?;
    let layers = layers_manifest
        .get("layers")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("image manifest has no layers array"))?;

    let scratch = tempfile::Builder::new()
        .prefix("bootc-migrate-kmods-")
        .tempdir_in(LAYER_SCRATCH_DIR)
        .context("failed to create /var/tmp scratch dir for kernel module streaming")?;
    let blob_path = scratch.path().join("layer.blob");
    let want = format!("usr/lib/modules/{kver}");

    // Newest → oldest with --skip-old-files so the newest copy of each file
    // wins (overlay semantics). The module tree is split across layers — bootc
    // images regenerate modules.dep.bin in a later layer than the kernel's .ko
    // files — so we can't stop at the first layer; we keep going until the
    // filesystem drivers the composefs+LUKS+XFS initrd actually needs (xfs,
    // erofs, overlay — shipped together in the kernel-modules layer) are present.
    // Full paths (no --strip-components) land the tree deterministically at
    // <scratch>/usr/lib/modules/<kver> regardless of the layer's leading `./`.
    let mods = scratch.path().join(&want);
    let needed_kos = [
        mods.join("kernel/fs/xfs/xfs.ko"),
        mods.join("kernel/fs/erofs/erofs.ko"),
        mods.join("kernel/fs/overlayfs/overlay.ko"),
    ];
    for layer in layers.iter().rev() {
        let digest = layer
            .get("digest")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("layer entry has no digest"))?;
        endpoint
            .download_blob(digest, &blob_path)
            .with_context(|| format!("failed to fetch layer {}", digest))?;
        for candidate in [format!("./{want}"), want.clone()] {
            let _ = Command::new("tar")
                .arg("-xaf")
                .arg(&blob_path)
                .arg("-C")
                .arg(scratch.path())
                .args(["--skip-old-files", "--no-same-owner"])
                .arg(&candidate)
                .stderr(std::process::Stdio::null())
                .status();
        }
        let _ = fs::remove_file(&blob_path);
        if needed_kos.iter().all(|p| p.exists()) {
            break;
        }
    }

    let missing: Vec<String> = needed_kos
        .iter()
        .filter(|p| !p.exists())
        .map(|p| p.display().to_string())
        .collect();
    if !mods.join("modules.dep.bin").exists() || !missing.is_empty() {
        return Err(anyhow!(
            "incomplete kernel modules for {kver} from {image_ref} via registry \
             (missing: {})",
            if missing.is_empty() {
                "modules.dep.bin".to_string()
            } else {
                missing.join(", ")
            }
        ));
    }
    Ok((scratch, mods))
}

/// Resolved registry endpoint: base URL (scheme + host), repository, reference, and
/// optional Bearer token. Built once per image and reused for the manifest + every
/// blob fetch.
struct RegistryEndpoint {
    base_url: String,
    repo: String,
    reference: String,
    bearer: Option<String>,
}

impl RegistryEndpoint {
    fn resolve(image_ref: &str) -> Result<Self> {
        let (host, repo, reference) = parse_image_ref(image_ref)?;

        // Pick http for plain non-standard ports (local dev registries), https otherwise.
        // We probe /v2/ to confirm and to discover any bearer challenge.
        let candidates: &[&str] = if host_is_plain_http(&host) {
            &["http"]
        } else {
            &["https", "http"]
        };

        // Keep every scheme's failure. Discarding them (this used to be
        // `Err(_) => continue`) collapsed curl-not-installed, DNS failure, a
        // TLS rejection, a proxy's 403, and a 401 with no challenge into one
        // indistinguishable "could not reach registry" — which is precisely
        // the information needed to fix any of them. See bootc-migrate#187:
        // the cross-base E2E cell could not be diagnosed because the guest's
        // real failure was never reported.
        let mut failures: Vec<(String, String)> = Vec::new();
        for scheme in candidates {
            let base = format!("{}://{}", scheme, host);
            match probe_v2(&base, &repo) {
                Ok(bearer) => {
                    return Ok(RegistryEndpoint {
                        base_url: base,
                        repo,
                        reference,
                        bearer,
                    });
                }
                Err(e) => failures.push(((*scheme).to_string(), format!("{e:#}"))),
            }
        }
        Err(anyhow!(unreachable_registry_message(&host, &failures)))
    }

    fn fetch_manifest(&self, reference: &str) -> Result<serde_json::Value> {
        let url = format!("{}/v2/{}/manifests/{}", self.base_url, self.repo, reference);
        let mut args: Vec<String> = vec![
            "-sSL".into(),
            "--fail".into(),
            "-H".into(),
            "Accept: application/vnd.oci.image.manifest.v1+json, application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.v2+json, application/vnd.docker.distribution.manifest.list.v2+json".into(),
        ];
        if let Some(token) = &self.bearer {
            args.push("-H".into());
            args.push(format!("Authorization: Bearer {}", token));
        }
        args.push(url);
        let out = Command::new("curl")
            .args(&args)
            .output()
            .context("failed to invoke curl for manifest fetch")?;
        if !out.status.success() {
            return Err(anyhow!(
                "curl manifest fetch failed: {}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        serde_json::from_slice(&out.stdout).context("failed to parse manifest JSON")
    }

    fn is_manifest_index(&self, m: &serde_json::Value) -> bool {
        match m.get("mediaType").and_then(|v| v.as_str()) {
            Some(mt) => mt.contains("manifest.list") || mt.contains("image.index"),
            None => m.get("manifests").is_some(),
        }
    }

    fn download_blob(&self, digest: &str, dst: &Path) -> Result<()> {
        let url = format!("{}/v2/{}/blobs/{}", self.base_url, self.repo, digest);
        let mut args: Vec<String> = vec![
            "-sSL".into(),
            "--fail".into(),
            "-o".into(),
            dst.to_string_lossy().into_owned(),
        ];
        if let Some(token) = &self.bearer {
            args.push("-H".into());
            args.push(format!("Authorization: Bearer {}", token));
        }
        args.push(url);
        let status = Command::new("curl")
            .args(&args)
            .status()
            .context("failed to invoke curl for blob fetch")?;
        if !status.success() {
            return Err(anyhow!("curl blob fetch failed for {}", digest));
        }
        Ok(())
    }

    /// Resolve a (possibly multi-arch) manifest to the concrete image manifest
    /// for the current architecture.
    fn arch_layers_manifest(&self, manifest_json: serde_json::Value) -> Result<serde_json::Value> {
        if !self.is_manifest_index(&manifest_json) {
            return Ok(manifest_json);
        }
        let arch = match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            other => other,
        };
        let entries = manifest_json
            .get("manifests")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("manifest index has no manifests array"))?;
        let pick = entries
            .iter()
            .find(|m| {
                m.get("platform")
                    .and_then(|p| p.get("architecture"))
                    .and_then(|a| a.as_str())
                    == Some(arch)
            })
            .ok_or_else(|| anyhow!("manifest index has no entry for arch {}", arch))?;
        let digest = pick
            .get("digest")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("manifest index entry has no digest"))?;
        self.fetch_manifest(digest)
    }
}

/// Hosts that should always use plain HTTP: bare IPv4 with a port, or `localhost`.
fn host_is_plain_http(host: &str) -> bool {
    if host.starts_with("localhost") {
        return true;
    }
    // IPv4-with-port like 10.0.2.2:5000
    let host_only = host.split(':').next().unwrap_or(host);
    host_only
        .split('.')
        .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        && host_only.split('.').count() == 4
}

/// Build the failure message for a registry we could not reach, naming what
/// each scheme actually did.
///
/// Pure so the shape is testable without a network: the value of this message
/// is that it distinguishes causes, and a regression to a single opaque line
/// would be invisible to any test that only checked for "could not reach".
fn unreachable_registry_message(host: &str, failures: &[(String, String)]) -> String {
    if failures.is_empty() {
        return format!("could not reach registry {host}: no scheme was attempted");
    }
    let detail = failures
        .iter()
        .map(|(scheme, err)| format!("{scheme}: {err}"))
        .collect::<Vec<_>>()
        .join("; ");
    format!("could not reach registry {host} ({detail})")
}

/// Probe `/v2/` (or `/v2/<repo>/tags/list`) to determine if the registry is reachable
/// and whether it requires a Bearer token. Returns Ok(Some(token)) if a Bearer
/// challenge was issued and we obtained a token, Ok(None) for anonymous access, Err
/// on transport failure.
fn probe_v2(base_url: &str, repo: &str) -> Result<Option<String>> {
    let url = format!("{}/v2/", base_url);
    let out = Command::new("curl")
        .args([
            "-sS",
            "-o",
            "/dev/null",
            "-D",
            "-",
            "--max-time",
            "10",
            &url,
        ])
        .output()
        .with_context(|| {
            format!("could not execute `curl` to probe {url} (is curl installed in this image?)")
        })?;
    if !out.status.success() {
        return Err(anyhow!(
            "curl probe to {} failed: {}",
            url,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let headers = String::from_utf8_lossy(&out.stdout);
    // First line: HTTP/1.1 <code> ...
    let status_code = headers
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("");
    if status_code.starts_with("2") {
        return Ok(None);
    }
    if status_code == "401" {
        // Parse Www-Authenticate: Bearer realm="...",service="...",scope="..."
        let challenge = headers
            .lines()
            .find(|l| l.to_lowercase().starts_with("www-authenticate:"))
            .ok_or_else(|| anyhow!("registry returned 401 with no Www-Authenticate header"))?;
        let token = fetch_bearer_token(challenge, repo)?;
        return Ok(Some(token));
    }
    Err(anyhow!("unexpected status from {}: {}", url, status_code))
}

/// Parse a `Www-Authenticate: Bearer realm="...",service="...",scope="..."` line and
/// fetch an anonymous token. If the challenge didn't include a scope, build one for
/// pull access to `repo`.
fn fetch_bearer_token(challenge: &str, repo: &str) -> Result<String> {
    let bearer_part = challenge
        .split_once(':')
        .map(|x| x.1)
        .map(|s| s.trim())
        .unwrap_or("");
    let bearer_part = bearer_part
        .strip_prefix("Bearer ")
        .ok_or_else(|| anyhow!("Www-Authenticate is not a Bearer challenge: {}", challenge))?;

    let mut realm: Option<String> = None;
    let mut service: Option<String> = None;
    let mut scope: Option<String> = None;
    for kv in bearer_part.split(',') {
        let mut it = kv.splitn(2, '=');
        let k = it.next().unwrap_or("").trim();
        let v = it.next().unwrap_or("").trim().trim_matches('"');
        match k {
            "realm" => realm = Some(v.to_string()),
            "service" => service = Some(v.to_string()),
            "scope" => scope = Some(v.to_string()),
            _ => {}
        }
    }
    let realm = realm.ok_or_else(|| anyhow!("Bearer challenge missing realm"))?;
    let scope = scope.unwrap_or_else(|| format!("repository:{}:pull", repo));

    let mut url = format!("{}?scope={}", realm, urlencode(&scope));
    if let Some(svc) = service {
        url.push_str(&format!("&service={}", urlencode(&svc)));
    }

    let out = Command::new("curl")
        .args(["-sSL", "--fail", &url])
        .output()
        .context("curl token fetch failed")?;
    if !out.status.success() {
        return Err(anyhow!(
            "token fetch failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let body: serde_json::Value =
        serde_json::from_slice(&out.stdout).context("token endpoint did not return JSON")?;
    let token = body
        .get("token")
        .or_else(|| body.get("access_token"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("token endpoint response has no token field"))?;
    Ok(token.to_string())
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// Parse `host[:port]/repo[:tag|@digest]` into (host, repo, reference).
/// Reference is the digest if `@` was present, otherwise the tag (default `latest`).
fn parse_image_ref(image_ref: &str) -> Result<(String, String, String)> {
    let trimmed = image_ref
        .strip_prefix("docker://")
        .unwrap_or(image_ref)
        .trim_start_matches('/');
    let (host, rest) = trimmed
        .split_once('/')
        .ok_or_else(|| anyhow!("image ref {} has no repository component", image_ref))?;

    // Split reference. `@` (digest) takes priority over `:` (tag) since digest contains `:`.
    let (repo, reference) = if let Some((r, d)) = rest.split_once('@') {
        (r.to_string(), d.to_string())
    } else if let Some((r, t)) = rest.rsplit_once(':') {
        (r.to_string(), t.to_string())
    } else {
        (rest.to_string(), "latest".to_string())
    };
    Ok((host.to_string(), repo, reference))
}

/// Try to extract a single file from one OCI layer blob to `dst`. Returns Ok(true)
/// if found, Ok(false) if the path wasn't in this layer (caller continues to the
/// next layer), Err on unexpected tar/IO failure.
///
/// OCI layer tarballs are gzip- or zstd-compressed and may store paths with or
/// without a leading `./`, so we try both forms. `tar -xaf` autodetects compression.
fn extract_one_from_layer(blob: &Path, src: &Path, dst: &Path) -> Result<bool> {
    let src_no_leading = src
        .strip_prefix("/")
        .unwrap_or(src)
        .to_string_lossy()
        .into_owned();
    let candidates = [format!("./{}", src_no_leading), src_no_leading.clone()];

    for candidate in &candidates {
        // Stream directly to disk — initrds can be ~200 MB, no reason to buffer.
        let dst_file = fs::File::create(dst).with_context(|| {
            format!(
                "failed to open destination {} for tar extract",
                dst.display()
            )
        })?;
        let status = Command::new("tar")
            .args([
                "-xaf",
                blob.to_str().ok_or_else(|| anyhow!("invalid blob path"))?,
                "-O",
                candidate,
            ])
            .stdout(dst_file)
            .stderr(std::process::Stdio::null())
            .status()
            .context("failed to invoke tar for layer extraction")?;
        if status.success() {
            // tar emitted to stdout — verify we got actual bytes (some tar versions
            // exit 0 even when the path isn't in the archive, just producing empty).
            if let Ok(meta) = fs::metadata(dst)
                && meta.len() > 0
            {
                return Ok(true);
            }
        }
        // Clean the empty destination so the next attempt starts fresh.
        let _ = fs::remove_file(dst);
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of this message is that it separates causes. #187
    /// stalled because every failure mode collapsed into one opaque
    /// "could not reach registry", so a guest that simply lacked `curl` was
    /// indistinguishable from one with no route to the registry.
    #[test]
    fn unreachable_registry_message_names_each_scheme_failure() {
        let failures = vec![
            (
                "https".to_string(),
                "could not execute `curl` to probe https://ghcr.io/v2/ (is curl installed in this image?): No such file or directory (os error 2)".to_string(),
            ),
            (
                "http".to_string(),
                "curl probe to http://ghcr.io/v2/ failed: Could not resolve host".to_string(),
            ),
        ];
        let msg = unreachable_registry_message("ghcr.io", &failures);

        assert!(msg.contains("ghcr.io"), "must name the host: {msg}");
        // Both attempts must survive, and be attributable to their scheme.
        assert!(
            msg.contains("https:"),
            "must attribute the https failure: {msg}"
        );
        assert!(
            msg.contains("http:"),
            "must attribute the http failure: {msg}"
        );
        assert!(
            msg.contains("is curl installed"),
            "a missing curl must stay legible rather than reading as a network fault: {msg}"
        );
        assert!(
            msg.contains("Could not resolve host"),
            "the second scheme's distinct cause must not be dropped: {msg}"
        );
    }

    /// Defensive: an empty failure list must not render as a bare, causeless
    /// "could not reach registry" that looks like the old opaque message.
    #[test]
    fn unreachable_registry_message_is_explicit_when_nothing_was_tried() {
        let msg = unreachable_registry_message("example.test", &[]);
        assert!(msg.contains("no scheme was attempted"), "{msg}");
    }

    #[test]
    fn image_ref_with_tag() {
        let (host, repo, r) = parse_image_ref("ghcr.io/projectbluefin/dakota:stable").unwrap();
        assert_eq!(host, "ghcr.io");
        assert_eq!(repo, "projectbluefin/dakota");
        assert_eq!(r, "stable");
    }

    #[test]
    fn image_ref_docker_prefix_and_default_tag() {
        let (host, repo, r) = parse_image_ref("docker://quay.io/fedora/fedora-bootc").unwrap();
        assert_eq!(host, "quay.io");
        assert_eq!(repo, "fedora/fedora-bootc");
        assert_eq!(r, "latest");
    }

    #[test]
    fn image_ref_digest_wins_over_colon() {
        // The digest itself contains ':' — '@' must take priority.
        let (_, repo, r) = parse_image_ref("ghcr.io/org/img@sha256:abcdef0123456789").unwrap();
        assert_eq!(repo, "org/img");
        assert_eq!(r, "sha256:abcdef0123456789");
    }

    #[test]
    fn image_ref_with_registry_port() {
        // rsplit_once(':') must pick the TAG colon, not the port colon.
        let (host, repo, r) = parse_image_ref("127.0.0.1:5000/bluefin:stable").unwrap();
        assert_eq!(host, "127.0.0.1:5000");
        assert_eq!(repo, "bluefin");
        assert_eq!(r, "stable");
    }

    #[test]
    fn image_ref_without_repo_component_errors() {
        assert!(parse_image_ref("just-a-name").is_err());
    }

    #[test]
    fn plain_http_hosts() {
        assert!(host_is_plain_http("localhost"));
        assert!(host_is_plain_http("localhost:5000"));
        assert!(host_is_plain_http("10.0.2.2:5000"));
        assert!(host_is_plain_http("127.0.0.1"));
        assert!(!host_is_plain_http("ghcr.io"));
        assert!(!host_is_plain_http("quay.io:443"));
        // Not a full dotted quad — must stay HTTPS.
        assert!(!host_is_plain_http("10.0.2"));
    }

    #[test]
    fn urlencode_reserved_and_unreserved() {
        assert_eq!(urlencode("repo/pull:read"), "repo%2Fpull%3Aread");
        assert_eq!(urlencode("abc-XYZ_0.9~"), "abc-XYZ_0.9~");
    }

    #[test]
    fn manifest_index_detection() {
        let ep = RegistryEndpoint {
            base_url: "https://example.invalid".into(),
            repo: "r".into(),
            reference: "t".into(),
            bearer: None,
        };
        let index: serde_json::Value = serde_json::json!({
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": []
        });
        let list: serde_json::Value = serde_json::json!({
            "mediaType": "application/vnd.docker.distribution.manifest.list.v2+json"
        });
        let image: serde_json::Value = serde_json::json!({
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "layers": []
        });
        // Media-type-less docker registries: presence of `manifests` decides.
        let bare: serde_json::Value = serde_json::json!({ "manifests": [] });
        assert!(ep.is_manifest_index(&index));
        assert!(ep.is_manifest_index(&list));
        assert!(!ep.is_manifest_index(&image));
        assert!(ep.is_manifest_index(&bare));
    }

    #[test]
    fn arch_manifest_passthrough_for_plain_manifest() {
        let ep = RegistryEndpoint {
            base_url: "https://example.invalid".into(),
            repo: "r".into(),
            reference: "t".into(),
            bearer: None,
        };
        let image: serde_json::Value = serde_json::json!({
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "layers": [{"digest": "sha256:aaa"}]
        });
        // A non-index manifest must pass through untouched (no network).
        let out = ep.arch_layers_manifest(image.clone()).unwrap();
        assert_eq!(out, image);
    }
}
