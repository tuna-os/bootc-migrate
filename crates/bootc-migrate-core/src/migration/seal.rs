//! Phase 3: create and seal the composefs image; `.origin` file content.

use super::*;

// ---- Phase 3 ----

/// Returns `(rootfs_verity, sealed_config_digest)`. The rootfs verity is the
/// composefs image object ID used in `.origin`/BLS for the boot-time root
/// mount; the sealed config digest (`sha256:…`) is the *manifest stream*
/// identifier that `bootc … cfs oci mount` requires (it prepends
/// `oci-config-` and looks up `streams/oci-config-<digest>`). These are
/// distinct: passing the rootfs verity to `mount` looks up a nonexistent
/// `oci-config-<verity>` stream and forces the zero-filling raw-EROFS fallback.
pub fn phase3_create_image(
    store: &dyn crate::composefs::ComposefsStore,
    target_image: &str,
    config_digest: &str,
    dry_run: bool,
) -> Result<(VerityDigest, String)> {
    println!("=== Phase 3: Creating ComposeFS EROFS Image ===");

    if dry_run {
        println!(
            "[DRY RUN] Would create and seal composefs image for config: {}",
            config_digest
        );
        return Ok((
            // "dryrun..." isn't valid hex (r/y/u/n) — from_hex asserts hex-only,
            // so a placeholder digest must actually be hex. deadbeef is the
            // traditional obviously-fake stand-in.
            VerityDigest::from_hex(
                "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
            ),
            config_digest.to_string(),
        ));
    }

    // Real idempotency — check if the image already exists AND is sealed.
    // We first need the verity hash to check, so we still call create_image (which
    // is typically a no-op if objects already exist), then skip seal if already done.
    let sha512_verity_str = store
        .create_image(target_image, config_digest)
        .context("failed to create composefs image")?;

    let verity = VerityDigest::from_prefixed_or_hex(&sha512_verity_str);
    println!(
        "ComposeFS EROFS image created. Verity digest: {}",
        verity.as_hex()
    );

    // `bootc … cfs oci seal` clones the manifest with the embedded verity digest
    // and prints the *sealed* manifest's `config <sha256:…>` line. That sealed
    // config digest — NOT the rootfs verity above — is what `cfs oci mount`
    // needs; without using it, mount fails and bootc falls back to a raw kernel
    // EROFS mount which zero-fills files above the inline threshold (causing
    // missing unit files like dbus.service and cascading boot failures).
    // Always seal — idempotency is handled inside bootc.
    println!("Sealing composefs image...");
    let seal_out = store
        .seal_image(target_image, config_digest)
        .context("failed to seal composefs image")?;
    let sealed_config = seal_out
        .lines()
        .find_map(|l| l.trim().strip_prefix("config "))
        .map(|s| s.trim().to_string())
        .ok_or_else(|| anyhow!("seal output missing 'config <digest>' line; got:\n{seal_out}"))?;
    println!("Image sealed successfully (sealed config: {sealed_config}).");

    // #425 — read every finished object after the writer's structural fsck.
    // FS_IOC_MEASURE_VERITY returns sealed metadata without reading object
    // contents; the explicit read pass makes backing-store corruption fail
    // during migration instead of later as EIO from the composefs overlay.
    store
        .verify_store_integrity(target_image)
        .context("composefs store integrity verification failed")?;
    println!("Verified: composefs store integrity and target readability.");

    Ok((verity, sealed_config))
}

// ---- Phase 4 ----

/// Build the `.origin` file content that bootc parses to identify a composefs
/// deployment. Uses `tini::Ini` for byte-compatible output with bootc's parser.
pub(crate) fn build_origin_content(
    target_image: &str,
    verity: &VerityDigest,
    manifest_digest: &str,
) -> String {
    // Schema must match bootc's canonical layout (crates/lib/src/composefs_consts.rs):
    //   [origin] container-image-reference = ...
    //   [boot]   boot_type = bls
    //   [boot]   digest = <verity hex>           # NB: key is "digest", not "boot_digest"
    //   [image]  manifest_digest = sha256:...
    // bootc's status code reads from [image]/manifest_digest and [boot]/digest;
    // wrong section or key names produce "No manifest_digest in origin and no
    // legacy .imginfo file" or "Could not find boot digest for deployment".
    tini::Ini::new()
        .section("origin")
        .item(
            "container-image-reference",
            format!("ostree-unverified-image:docker://{}", target_image),
        )
        .section("boot")
        .item("boot_type", "bls")
        .item("digest", verity.as_hex())
        .section("image")
        .item("manifest_digest", manifest_digest)
        .to_string()
}

/// Patch the `digest` entry in `[boot]` with a real sha256(vmlinuz || initrd).
/// Pure function so we can test it without filesystem access.
pub(crate) fn patch_boot_digest_in_content(content: &str, new_digest: &str) -> Result<String> {
    let ini = tini::Ini::from_string(content)
        .map_err(|e| anyhow!("parsing origin file: {e}"))?
        .section("boot")
        .item("digest", new_digest);
    Ok(ini.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_content_roundtrips_through_tini() {
        let verity = VerityDigest::from_hex(
            "9af734da164df0edb34a200a55bf4a6426afbc80f66e5fb7c73ecfdd17b19dbd",
        );
        let content = build_origin_content(
            "ghcr.io/projectbluefin/dakota:stable",
            &verity,
            "sha256:abc123",
        );
        // Must parse back successfully
        let parsed = tini::Ini::from_string(&content).expect("origin content must be valid INI");
        assert_eq!(
            parsed
                .get::<String>("origin", "container-image-reference")
                .as_deref(),
            Some("ostree-unverified-image:docker://ghcr.io/projectbluefin/dakota:stable")
        );
        assert_eq!(
            parsed.get::<String>("boot", "boot_type").as_deref(),
            Some("bls")
        );
        assert_eq!(
            parsed.get::<String>("boot", "digest").as_deref(),
            Some("9af734da164df0edb34a200a55bf4a6426afbc80f66e5fb7c73ecfdd17b19dbd"),
            "[boot] digest must match bootc's ORIGIN_KEY_BOOT_DIGEST constant"
        );
        assert_eq!(
            parsed.get::<String>("image", "manifest_digest").as_deref(),
            Some("sha256:abc123"),
            "manifest_digest must be under [image], not [boot]"
        );
    }

    #[test]
    fn origin_content_is_stable_across_rebuilds() {
        let verity = VerityDigest::from_hex(
            "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
        );
        let a = build_origin_content("img:latest", &verity, "sha256:foo");
        let b = build_origin_content("img:latest", &verity, "sha256:foo");
        assert_eq!(a, b, "origin content must be deterministic");
    }

    #[test]
    fn patch_boot_digest_replaces_placeholder() {
        let verity = VerityDigest::from_hex(
            "9af734da164df0edb34a200a55bf4a6426afbc80f66e5fb7c73ecfdd17b19dbd",
        );
        let original = build_origin_content("img:latest", &verity, "sha256:disc");
        let patched = patch_boot_digest_in_content(&original, "abcdef1234567890").unwrap();

        let parsed = tini::Ini::from_string(&patched).unwrap();
        assert_eq!(
            parsed.get::<String>("boot", "digest").as_deref(),
            Some("abcdef1234567890"),
            "[boot] digest must be replaced with real sha256(vmlinuz||initrd)"
        );
    }

    #[test]
    fn patch_boot_digest_preserves_all_other_keys() {
        let verity = VerityDigest::from_hex(
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        );
        let original =
            build_origin_content("ghcr.io/example/target:v1", &verity, "sha256:manifest123");
        let patched = patch_boot_digest_in_content(&original, "newdigest111").unwrap();

        let parsed = tini::Ini::from_string(&patched).unwrap();
        assert_eq!(
            parsed
                .get::<String>("origin", "container-image-reference")
                .as_deref(),
            Some("ostree-unverified-image:docker://ghcr.io/example/target:v1")
        );
        assert_eq!(
            parsed.get::<String>("boot", "boot_type").as_deref(),
            Some("bls")
        );
        assert_eq!(
            parsed.get::<String>("image", "manifest_digest").as_deref(),
            Some("sha256:manifest123")
        );
        assert_eq!(
            parsed.get::<String>("boot", "digest").as_deref(),
            Some("newdigest111")
        );
    }

    #[test]
    fn patch_boot_digest_fails_on_garbage_input() {
        let result = patch_boot_digest_in_content("not a valid INI file\n[garbage", "foo");
        assert!(result.is_err(), "must reject malformed INI");
    }
}
