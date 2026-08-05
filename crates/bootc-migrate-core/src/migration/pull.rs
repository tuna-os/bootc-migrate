//! Phase 2: pull the target OCI image into the composefs repository.

use super::*;

// ---- Phase 2 ----

/// A target image resolved to the platform-specific manifest that Phase 2
/// pulled. `image_reference` is immutable and is used for all content reads
/// during this migration; `target_image` remains the mutable origin reference
/// recorded for future bootc upgrades.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PulledImage {
    pub image_reference: String,
    pub manifest_digest: String,
    pub config_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedTargetImage {
    image_reference: String,
    manifest_digest: String,
}

pub fn phase2_pull_image(
    store: &dyn crate::composefs::ComposefsStore,
    target_image: &str,
    dry_run: bool,
) -> Result<PulledImage> {
    phase2_pull_image_with_resolver(store, target_image, dry_run, refresh_target_image)
}

fn phase2_pull_image_with_resolver(
    store: &dyn crate::composefs::ComposefsStore,
    target_image: &str,
    dry_run: bool,
    resolve_target: impl FnOnce(&str) -> Result<ResolvedTargetImage>,
) -> Result<PulledImage> {
    println!("=== Phase 2: Pulling OCI image ===");

    if dry_run {
        println!("[DRY RUN] Would pull image: {}", target_image);
        return Ok(PulledImage {
            image_reference: target_image.to_string(),
            manifest_digest: "dry-run-manifest".into(),
            config_digest: "dry-run-config".into(),
        });
    }

    // The delegated legacy cfs path imports from containers-storage:. Refresh
    // first, then pin it to the selected platform manifest so that the cfs
    // rootfs, /etc merge, and boot artifacts cannot come from different tag
    // generations.
    let resolved = resolve_target(target_image)
        .context("failed to refresh and resolve the target image before ComposeFS import")?;
    println!("Pulling target image: {}...", resolved.image_reference);
    let pull_output = store
        .pull_image(target_image, &resolved.image_reference)
        .context("failed to pull OCI image")?;

    let (manifest_opt, config_opt) = parse_pull_digests(&pull_output);
    if let Some(manifest_digest) = manifest_opt
        && manifest_digest != resolved.manifest_digest
    {
        anyhow::bail!(
            "ComposeFS pulled manifest {manifest_digest}, but podman resolved \
             {} to {}. Refusing to mix image generations",
            target_image,
            resolved.manifest_digest
        );
    }

    let config_digest = config_opt.unwrap_or_default();
    println!(
        "Target image pulled. Manifest: {}, Config: {}",
        resolved.manifest_digest, config_digest
    );
    Ok(PulledImage {
        image_reference: resolved.image_reference,
        manifest_digest: resolved.manifest_digest,
        config_digest,
    })
}

/// Pull the mutable target tag with an explicit always policy, then resolve the
/// exact platform manifest that podman selected. Failing here is intentional:
/// using an older local tag after a failed refresh can create a rootfs from one
/// image generation and boot artifacts from another.
fn refresh_target_image(target_image: &str) -> Result<ResolvedTargetImage> {
    let podman_image = target_image
        .strip_prefix("docker://")
        .unwrap_or(target_image);
    let output = Command::new("podman")
        .args(["pull", "--policy", "always", podman_image])
        .output()
        .context("failed to execute podman pull")?;
    if !output.status.success() {
        anyhow::bail!(
            "podman could not refresh {podman_image}: {}. Refusing to use a \
             potentially stale local image",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let manifest_digest = podman_manifest_digest(podman_image).ok_or_else(|| {
        anyhow!(
            "podman refreshed {podman_image}, but did not report a valid platform manifest digest"
        )
    })?;
    let image_reference = pin_image_reference(podman_image, &manifest_digest)?;
    println!("Image refreshed in podman storage: {image_reference}");
    Ok(ResolvedTargetImage {
        image_reference,
        manifest_digest,
    })
}

/// Convert a tag or existing digest reference into an immutable registry
/// reference. The result deliberately has no transport prefix so it can be
/// used by both podman and `containers-storage:`.
fn pin_image_reference(image: &str, manifest_digest: &str) -> Result<String> {
    if !is_sha256_digest(manifest_digest) {
        anyhow::bail!("invalid OCI manifest digest {manifest_digest:?}");
    }
    let image = image.strip_prefix("docker://").unwrap_or(image);
    if image.contains("://") {
        anyhow::bail!("unsupported image transport in {image:?}");
    }
    let name = image.split_once('@').map_or(image, |(name, _)| name);
    let without_tag = match (name.rfind(':'), name.rfind('/')) {
        (Some(colon), Some(slash)) if colon > slash => &name[..colon],
        (Some(colon), None) => &name[..colon],
        _ => name,
    };
    if without_tag.is_empty() {
        anyhow::bail!("invalid image reference {image:?}");
    }
    Ok(format!("{without_tag}@{manifest_digest}"))
}

fn is_sha256_digest(digest: &str) -> bool {
    let Some(hex) = digest.strip_prefix("sha256:") else {
        return false;
    };
    hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Parse `bootc internals cfs oci pull` stdout into `(manifest_digest, config_digest)`.
///
/// Handles the 1.13.0 format that prints only `config <sha256>` + `verity <hash>`
/// (no `manifest` line). Critically, never yields a multi-line `manifest_digest` —
/// a newline in that value corrupts the deployment `.origin` ini and breaks
/// `bootc status`. The config digest falls back to the first `sha256:` token.
pub(crate) fn parse_pull_digests(pull_output: &str) -> (Option<String>, Option<String>) {
    let mut manifest = None;
    let mut config = None;
    for line in pull_output.lines() {
        let t = line.trim();
        if let Some(r) = t.strip_prefix("manifest ") {
            manifest = Some(r.trim().to_string());
        } else if let Some(r) = t.strip_prefix("config ") {
            config = Some(r.trim().to_string());
        }
    }
    // A valid digest is a single non-empty token; reject anything else.
    let manifest = manifest.filter(|m| !m.is_empty() && !m.contains(char::is_whitespace));
    let config = config
        .filter(|c| !c.is_empty() && !c.contains(char::is_whitespace))
        .or_else(|| {
            pull_output
                .split_whitespace()
                .find(|x| x.starts_with("sha256:"))
                .map(String::from)
        });
    (manifest, config)
}

/// Read the OCI manifest digest (`sha256:…`) of a locally-cached image via
/// `podman image inspect`. Returns None if podman/the image is unavailable.
fn podman_manifest_digest(image: &str) -> Option<String> {
    let out = Command::new("podman")
        .args(["image", "inspect", "--format", "{{.Digest}}", image])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let d = String::from_utf8_lossy(&out.stdout).trim().to_string();
    is_sha256_digest(&d).then_some(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pull_digests_kanpur_format_no_manifest_line() {
        // bootc 1.13.0: config + verity, no "manifest" line. The old code used the
        // whole multi-line output as manifest_digest, corrupting the .origin ini.
        let out = "config sha256:39f5731c23efd9\nverity b0e7a7dabb84cb9d";
        let (manifest, config) = parse_pull_digests(out);
        // No usable manifest digest from this output (caller falls back to podman).
        assert_eq!(manifest, None);
        // Config digest is parsed clean and single-line.
        assert_eq!(config.as_deref(), Some("sha256:39f5731c23efd9"));
    }

    #[test]
    fn parse_pull_digests_with_manifest_line() {
        let out = "manifest sha256:aaa\nconfig sha256:bbb\nverity ccc";
        let (manifest, config) = parse_pull_digests(out);
        assert_eq!(manifest.as_deref(), Some("sha256:aaa"));
        assert_eq!(config.as_deref(), Some("sha256:bbb"));
    }

    #[test]
    fn parse_pull_digests_single_line_fallback() {
        // Single bare digest line → config via the sha256: token fallback.
        let (manifest, config) = parse_pull_digests("sha256:deadbeef");
        assert_eq!(manifest, None);
        assert_eq!(config.as_deref(), Some("sha256:deadbeef"));
    }

    #[test]
    fn parse_pull_digests_never_returns_multiline() {
        // Even a malformed multi-token "manifest" line must be rejected, never
        // passed through to corrupt the .origin ini.
        let (manifest, _) = parse_pull_digests("manifest sha256:x extra junk");
        assert_eq!(manifest, None);
    }

    #[test]
    fn pin_image_reference_replaces_a_tag_but_preserves_a_registry_port() {
        let digest = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert_eq!(
            pin_image_reference("ghcr.io/projectbluefin/dakota-nvidia:testing", digest).unwrap(),
            format!("ghcr.io/projectbluefin/dakota-nvidia@{digest}")
        );
        assert_eq!(
            pin_image_reference("docker://localhost:5000/ns/image:testing", digest).unwrap(),
            format!("localhost:5000/ns/image@{digest}")
        );
        assert_eq!(
            pin_image_reference(
                "ghcr.io/projectbluefin/dakota-nvidia@sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
                digest
            )
            .unwrap(),
            format!("ghcr.io/projectbluefin/dakota-nvidia@{digest}")
        );
    }

    #[test]
    fn phase2_imports_the_resolved_manifest_reference() {
        use crate::composefs::MockComposefsStore;
        let store = MockComposefsStore::default();
        let manifest = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
        let result =
            phase2_pull_image_with_resolver(&store, "example.invalid/mock:latest", false, |_| {
                Ok(ResolvedTargetImage {
                    image_reference: format!("example.invalid/mock@{manifest}"),
                    manifest_digest: manifest.into(),
                })
            })
            .unwrap();
        assert_eq!(result.manifest_digest, manifest);
        assert!(result.config_digest.starts_with("sha256:1111"));
        let calls = store.calls.lock().unwrap();
        assert_eq!(
            calls.as_slice(),
            [format!("pull_image example.invalid/mock@{manifest}")]
        );
    }

    #[test]
    fn phase2_dry_run_does_not_resolve_or_pull() {
        use crate::composefs::MockComposefsStore;
        let store = MockComposefsStore::default();
        let result =
            phase2_pull_image_with_resolver(&store, "example.invalid/mock:latest", true, |_| {
                panic!("dry run must not resolve the target image")
            })
            .unwrap();
        assert_eq!(result.image_reference, "example.invalid/mock:latest");
        assert!(store.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn phase2_rejects_a_store_manifest_from_another_generation() {
        use crate::composefs::MockComposefsStore;
        let store = MockComposefsStore::default();
        let result = phase2_pull_image_with_resolver(
            &store,
            "example.invalid/mock:latest",
            false,
            |_| {
                Ok(ResolvedTargetImage {
                    image_reference: "example.invalid/mock@sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".into(),
                    manifest_digest: "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".into(),
                })
            },
        );
        assert!(result.is_err());
    }

    #[test]
    fn phase2_does_not_pull_when_resolution_fails() {
        use crate::composefs::MockComposefsStore;
        let store = MockComposefsStore::default();
        let result =
            phase2_pull_image_with_resolver(&store, "example.invalid/mock:latest", false, |_| {
                Err(anyhow!("test resolver failure"))
            });
        assert!(result.is_err());
        assert!(store.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn sha256_digest_validation_requires_full_hex_digest() {
        assert!(is_sha256_digest(
            "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        ));
        assert!(!is_sha256_digest("sha256:deadbeef"));
        assert!(!is_sha256_digest(
            "sha512:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        ));
    }
}
