//! Target-image access: inspecting the image, and getting a usable rootfs
//! mounted with the fallback ladder Phase 4 and Phase 5 both need.
//!
//! Split out of `migration::mod` (#175) so both phases request a mounted
//! target and receive **one owned guard** instead of each running the same
//! ladder and picking between two guard types inline.
//!
//! The ladder exists because a zero exit from `bootc internals cfs oci mount`
//! is not proof of a usable mount: bootc can mount into its own private
//! namespace (`MS_REC|MS_PRIVATE`), which is torn down the instant the
//! subprocess exits, leaving an empty directory behind. So every mount is
//! verified by looking for content the caller knows must be there, and a
//! `podman image mount` of the already-cached image is the fallback — local,
//! real content, and no dependency on reaching the registry mid-migration.

use anyhow::{Context, Result, anyhow};
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{MountGuard, PodmanImageMount};

pub fn inspect_image(image_id: &str) -> Result<String> {
    let output = Command::new("bootc")
        .args(["internals", "cfs", "--system", "oci", "inspect", image_id])
        .output()
        .context("failed to execute bootc internals cfs oci inspect")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("inspect failed: {}", stderr));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Generation-aware composefs overlay mount for phases 4/5.
///
/// On a **legacy-CLI host** this is exactly [`mount_image`] with the sealed
/// config digest — byte-identical to historical behavior.
///
/// On a **new-generation host** (no `create-image`/`seal` — issue #72) the
/// sealed-config identifier resolves nothing (`oci mount` now takes a tag or
/// manifest digest), and a legacy-delegate-written store additionally lacks
/// the config-splitstream EROFS named ref that new-gen resolution requires.
/// Both are fixed by one free operation, verified empirically (see
/// docs/cfs-cli-generations.md): re-pull the image from `containers-storage:`
/// — 0 new objects, deduped, rewrites config+manifest splitstreams with the
/// EROFS ref, and the EROFS id is deterministic so existing BLS/`.origin`
/// digests stay valid — then mount by the pulled ref. Any failure falls back
/// to the legacy path (and its raw-EROFS + caller-side podman fallbacks).
pub fn mount_image_for(target_image: &str, sealed_config: &str, mount_path: &Path) -> Result<()> {
    if !crate::composefs::host_cfs_is_legacy() {
        let cs_ref = format!("containers-storage:{target_image}");
        let pulled = Command::new("bootc")
            .args(["internals", "cfs", "--system", "oci", "pull", &cs_ref])
            .output();
        match pulled {
            Ok(o) if o.status.success() => {
                if let Some(mount_str) = mount_path.to_str() {
                    let mnt = Command::new("bootc")
                        .args([
                            "internals",
                            "cfs",
                            "--system",
                            "oci",
                            "mount",
                            &cs_ref,
                            mount_str,
                        ])
                        .output();
                    match mnt {
                        Ok(m) if m.status.success() => return Ok(()),
                        Ok(m) => eprintln!(
                            "[mount] new-gen mount by ref failed ({}); trying legacy identifiers",
                            String::from_utf8_lossy(&m.stderr).trim()
                        ),
                        Err(e) => eprintln!(
                            "[mount] new-gen mount by ref failed ({e}); trying legacy identifiers"
                        ),
                    }
                }
            }
            Ok(o) => eprintln!(
                "[mount] new-gen containers-storage re-pull failed ({}); \
                 trying legacy identifiers",
                String::from_utf8_lossy(&o.stderr).trim()
            ),
            Err(e) => eprintln!(
                "[mount] new-gen containers-storage re-pull failed ({e}); \
                 trying legacy identifiers"
            ),
        }
    }
    mount_image(sealed_config, mount_path)
}

pub fn mount_image(image_id: &str, mount_path: &Path) -> Result<()> {
    let mount_str = mount_path
        .to_str()
        .ok_or_else(|| anyhow!("invalid mount path"))?;

    // Always prefer the bootc composefs overlay mount: it stacks the EROFS
    // metadata layer on top of the content-addressed object tree at
    // /sysroot/composefs/objects so files read back with their actual content.
    // A bare `mount -t erofs` returns metadata-only views (sizes look right but
    // file contents are zero-filled), which silently corrupts every artifact
    // Phase 5 copies out of the mount (kernel, initrd, systemd-bootx64.efi…).
    let output = Command::new("bootc")
        .args([
            "internals",
            "cfs",
            "--system",
            "oci",
            "mount",
            image_id,
            mount_str,
        ])
        .output()
        .context("failed to execute bootc internals cfs oci mount")?;
    if output.status.success() {
        return Ok(());
    }

    // Last-resort fallback: raw EROFS mount. This works only if every file
    // copied out of the mount happens to be inline (small enough to live in
    // the EROFS metadata). Reserved for environments where bootc is missing.
    let bootc_err = String::from_utf8_lossy(&output.stderr).into_owned();
    let image_path = Path::new("/sysroot/composefs/images").join(image_id);
    if image_path.exists() {
        let fallback = Command::new("/usr/bin/mount")
            .args([
                "-t",
                "erofs",
                "-o",
                "ro,loop",
                image_path.to_str().unwrap_or(""),
                mount_str,
            ])
            .output()
            .context("failed to mount erofs image (bootc cfs fallback)")?;
        if fallback.status.success() {
            eprintln!(
                "Warning: bootc cfs mount failed ({}), fell back to raw EROFS — \
                 file content beyond the inline threshold will read as zeros.",
                bootc_err.trim()
            );
            return Ok(());
        }
    }
    Err(anyhow!("mount failed: {}", bootc_err))
}

/// A mounted target image plus whichever guard owns its cleanup.
///
/// Holding one value rather than an `Option<MountGuard>` and an
/// `Option<PodmanImageMount>` side by side is the point: the two are mutually
/// exclusive, unmounting is guaranteed on drop, and no caller can get the
/// pairing wrong.
pub(crate) struct TargetMount {
    path: PathBuf,
    _cfs: Option<MountGuard>,
    _podman: Option<PodmanImageMount>,
}

impl TargetMount {
    /// The usable rootfs — the composefs overlay, or the podman mount it fell
    /// back to.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

/// Mount `target_image` and return a guard owning the cleanup.
///
/// `verify_subpath` is content the caller knows the image must contain
/// (`etc` for the /etc merge, `usr/lib/modules` for boot artifacts); its
/// absence means the composefs mount did not survive into our namespace, and
/// the podman fallback is taken. `phase` prefixes the diagnostics.
pub(crate) fn open_target(
    target_image: &str,
    sealed_config: &str,
    preferred_mount: &Path,
    verify_subpath: &str,
    phase: &str,
) -> Result<TargetMount> {
    let composefs_mounted = match mount_image_for(target_image, sealed_config, preferred_mount) {
        Ok(()) if preferred_mount.join(verify_subpath).is_dir() => true,
        Ok(()) => {
            eprintln!(
                "[{phase}] composefs mount reported success but exposed no content \
                 (bootc mounted in a private namespace that did not persist); \
                 falling back to podman image mount"
            );
            false
        }
        Err(e) => {
            eprintln!(
                "[{phase}] composefs overlay mount failed ({e}); \
                 falling back to podman image mount"
            );
            false
        }
    };

    if composefs_mounted {
        return Ok(TargetMount {
            path: preferred_mount.to_path_buf(),
            // Guard only a mount that actually persisted into our namespace;
            // otherwise umount would warn about a mount that isn't ours.
            _cfs: Some(MountGuard::new(preferred_mount)),
            _podman: None,
        });
    }

    let pm = PodmanImageMount::new(target_image)
        .context("composefs mount unavailable and podman image mount fallback also failed")?;
    println!(
        "[{phase}] using podman image mount at {}",
        pm.path.display()
    );
    Ok(TargetMount {
        path: pm.path.clone(),
        _cfs: None,
        _podman: Some(pm),
    })
}
