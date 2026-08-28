//! Phase 4 deployment layout: the on-disk shape of a staged deployment.
//!
//! Everything that decides *where* deployment state lives and *what files*
//! describe it — the deployment directory, the `.origin` and `.imginfo`
//! descriptors, the `/var` state staging, and the runtime ComposeFS mount
//! unit. [`super::deploy`] coordinates these steps; none of their policy is
//! inline there.

use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

use crate::VerityDigest;
use crate::migration::build_origin_content;
use crate::migration::image_access;
use crate::migration::var_layout;
use crate::xattr;

/// Root of the composefs state tree that deployments are staged under.
const DEPLOY_ROOT: &str = "/sysroot/state/deploy";
/// The ext4 loopback holding the composefs store on XFS roots.
const COMPOSEFS_LOOPBACK: &str = "/sysroot/composefs-loopback.ext4";

/// The on-disk layout of one staged deployment, keyed by its rootfs verity.
pub(crate) struct DeploymentLayout {
    pub(crate) deploy_dir: PathBuf,
    pub(crate) etc_dir: PathBuf,
    pub(crate) origin_path: PathBuf,
    pub(crate) imginfo_path: PathBuf,
    /// A deployment directory with a valid `.origin` was already present.
    pub(crate) already_staged: bool,
}

impl DeploymentLayout {
    /// Resolve the layout for `verity` and record whether it is already staged.
    ///
    /// bootc expects the descriptor filenames as `<bare-hex-verity>.origin`
    /// (no `sha512:` prefix); using `as_prefixed()` here would cause
    /// `bootc status` to fail with "Opening origin file: No such file or
    /// directory" and break the post-reboot validation.
    pub(crate) fn for_verity(verity: &VerityDigest) -> Self {
        let hex = verity.as_hex();
        let deploy_dir = Path::new(DEPLOY_ROOT).join(hex);
        let origin_path = deploy_dir.join(format!("{hex}.origin"));
        let imginfo_path = deploy_dir.join(format!("{hex}.imginfo"));
        let already_staged = deploy_dir.exists() && origin_path.exists();
        Self {
            etc_dir: deploy_dir.join("etc"),
            deploy_dir,
            origin_path,
            imginfo_path,
            already_staged,
        }
    }

    /// Create the deployment directory and its `/etc`.
    pub(crate) fn create_dirs(&self) -> Result<()> {
        fs::create_dir_all(&self.deploy_dir).context("failed to create deployment directory")?;
        fs::create_dir_all(&self.etc_dir).context("failed to create deployment etc directory")?;
        Ok(())
    }

    /// Point the deployment's `var` at the shared stateroot `/var`.
    pub(crate) fn stage_var_symlink(&self) -> Result<()> {
        let var_symlink = self.deploy_dir.join("var");
        if var_symlink.exists() || var_symlink.is_symlink() {
            fs::remove_file(&var_symlink).context("failed to remove existing var entry")?;
        }
        std::os::unix::fs::symlink("../../os/default/var", &var_symlink)
            .context("failed to create /var symlink")
    }

    /// Write the `.origin` descriptor using bootc's expected schema.
    ///
    /// The placeholder boot_digest gets patched in Phase 5 with
    /// sha256(vmlinuz || initrd) once those files are on the ESP.
    pub(crate) fn write_origin(
        &self,
        target_image: &str,
        verity: &VerityDigest,
        manifest_digest: &str,
    ) -> Result<()> {
        let origin_content = build_origin_content(target_image, verity, manifest_digest);
        fs::write(&self.origin_path, &origin_content).context("failed to write .origin file")
    }

    /// Write the `.imginfo` descriptor. Best-effort: a missing or unwritable
    /// `.imginfo` does not fail the migration, since bootc can fall back to
    /// the manifest digest recorded in `.origin`.
    pub(crate) fn write_imginfo(&self, config_digest: &str) {
        println!("Writing .imginfo file...");
        if let Ok(config_json) = image_access::inspect_image(config_digest)
            && let Err(e) = fs::write(&self.imginfo_path, &config_json)
        {
            eprintln!(
                "Warning: failed to write .imginfo file ({}): {}",
                self.imginfo_path.display(),
                e
            );
        }
    }

    /// On XFS roots the composefs repo lives in an ext4 loopback file, and the
    /// booted system must mount it at /sysroot/composefs so `bootc status` and
    /// day-2 updates can reach the repo. No-op when there is no loopback.
    ///
    /// Best-effort: usually the initrd's mount survives switch-root and this
    /// unit merely goes active over it.
    pub(crate) fn install_runtime_composefs_mount(&self) {
        if !Path::new(COMPOSEFS_LOOPBACK).exists() {
            return;
        }
        if let Err(e) = write_runtime_composefs_loopback_mount(&self.etc_dir) {
            eprintln!("[phase4] Warning: failed to install runtime composefs mount: {e:#}");
        }
    }
}

/// Install a systemd mount unit into the deployment's /etc so the booted system
/// loop-mounts the composefs ext4 store at /sysroot/composefs.
///
/// This is a fallback, not the mount the running system normally uses. The
/// initrd unit from `migration::prepare_composefs_loopback_include` mounts the
/// same source at the same target under /sysroot, which *becomes* / at
/// switch-root, so that mount survives — and systemd then treats this unit's
/// already-mounted target as active without issuing a second mount. Options
/// set here therefore do not override the initrd's; they apply only if the
/// initrd mount is somehow absent.
///
/// Kept in sync at **rw** with the initrd unit for exactly that case: day-2
/// updates write to the store, and a `ro` store makes `bootc upgrade` fail
/// with "Repository is not writable: read-only file system". See that
/// function's docs for the full account.
fn write_runtime_composefs_loopback_mount(etc_dir: &Path) -> Result<()> {
    let unit_dir = etc_dir.join("systemd/system");
    fs::create_dir_all(&unit_dir)?;
    fs::write(
        unit_dir.join("sysroot-composefs.mount"),
        "[Unit]\n\
         Description=ComposeFS Loopback Store (runtime)\n\
         DefaultDependencies=no\n\
         After=sysroot.mount\n\
         Before=local-fs.target\n\
         \n\
         [Mount]\n\
         What=/sysroot/composefs-loopback.ext4\n\
         Where=/sysroot/composefs\n\
         Type=ext4\n\
         Options=loop,rw\n\
         \n\
         [Install]\n\
         WantedBy=local-fs.target\n",
    )?;
    let wants_dir = unit_dir.join("local-fs.target.wants");
    fs::create_dir_all(&wants_dir)?;
    let link = wants_dir.join("sysroot-composefs.mount");
    let _ = fs::remove_file(&link);
    std::os::unix::fs::symlink("../sysroot-composefs.mount", &link)
        .context("failed to enable runtime sysroot-composefs.mount")?;
    Ok(())
}

/// Migrate `/var` into the composefs stateroot.
///
/// A dedicated `/var` filesystem stays in place; otherwise the live tree is
/// copied once, and only the completion marker proves the copy finished.
pub(crate) fn migrate_var_state() -> Result<()> {
    println!("=== Migrating /var data to ComposeFS state ===");
    let target_var = Path::new("/sysroot/state/os/default/var");
    let completion_marker =
        Path::new("/sysroot/state/os/default/.bootc-migrate-composefs-var-complete");

    // A dedicated filesystem or direct Btrfs subvolume is mounted at this
    // stateroot path by the initrd (Phase 5), so its existing data remains in
    // place. Copying it would be both wasteful and lossy for special files.
    if let Some(var) = var_layout::detect_separate_var()? {
        println!(
            "[phase4] preserving /var in place ({}, UUID={}, options={}); Phase 5 will mount it at the composefs stateroot",
            var.fstype, var.uuid, var.options
        );
        return Ok(());
    }

    // A target /var can contain a directory skeleton before migration. Only our
    // completion marker proves the live data copy finished successfully.
    if completion_marker.exists() {
        println!(
            "/var migration already completed at {}. Skipping.",
            target_var.display()
        );
        return Ok(());
    }

    fs::create_dir_all(target_var)?;

    // Copy the mounted live tree. The old OSTree deployment path may exist but
    // is not necessarily the source backing the active /var mount.
    println!("Migrating /var data from /var to ComposeFS state...");
    xattr::copy_dir_all_with_xattrs("/var", target_var)
        .context("failed to migrate /var data to ComposeFS state")?;
    fs::write(
        completion_marker,
        "The live /var tree was copied by bootc-migrate-composefs.\n",
    )
    .context("failed to write /var migration completion marker")?;
    println!("/var data migrated successfully.");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The composefs store must be mounted rw: day-2 updates write into it,
    /// and a `ro` store makes `bootc upgrade` fail with "Repository is not
    /// writable". This unit and the initrd unit in
    /// `migration::prepare_composefs_loopback_include` must agree — see the
    /// matching test there for why the initrd one is the binding copy.
    #[test]
    fn runtime_composefs_loopback_mount_is_writable() {
        let tmp = tempfile::tempdir().unwrap();
        write_runtime_composefs_loopback_mount(tmp.path()).unwrap();

        let unit = tmp.path().join("systemd/system/sysroot-composefs.mount");
        let body = std::fs::read_to_string(&unit).unwrap();
        let opts = body
            .lines()
            .find_map(|l| l.trim().strip_prefix("Options="))
            .expect("mount unit has no Options= line");
        assert!(
            opts.split(',').any(|o| o == "rw"),
            "runtime composefs loopback mount must be rw, got Options={opts}"
        );
        assert!(body.contains("Where=/sysroot/composefs"));
        assert!(body.contains("What=/sysroot/composefs-loopback.ext4"));

        // Writing the unit is not enough; it has to be enabled.
        let link = tmp
            .path()
            .join("systemd/system/local-fs.target.wants/sysroot-composefs.mount");
        assert!(link.is_symlink(), "mount unit was written but not enabled");
    }

    /// bootc reads `<bare-hex>.origin`; a `sha512:`-prefixed name makes
    /// `bootc status` fail with "Opening origin file: No such file or
    /// directory" and breaks post-reboot validation.
    #[test]
    fn descriptor_filenames_use_bare_hex_verity() {
        let verity = VerityDigest::from_hex("abc123");
        let layout = DeploymentLayout::for_verity(&verity);

        assert_eq!(
            layout.origin_path.file_name().unwrap(),
            "abc123.origin",
            "origin filename must be bare hex, not prefixed"
        );
        assert_eq!(
            layout.imginfo_path.file_name().unwrap(),
            "abc123.imginfo",
            "imginfo filename must be bare hex, not prefixed"
        );
        assert!(layout.deploy_dir.ends_with("abc123"));
        assert!(layout.etc_dir.ends_with("abc123/etc"));
    }
}
