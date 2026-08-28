pub mod boot;
pub mod bootloader;
pub mod deploy;
pub mod host_storage;
pub mod image_access;
pub mod import;
pub mod initrd;
pub mod kernel_options;
pub mod os_release;
pub mod pull;
pub mod rollback;
pub mod seal;
pub mod target_compat;
pub mod var_layout;

pub use boot::migrate_bootloader_standalone;
pub use boot::phase5_setup_bootloader;
pub use deploy::phase4_stage_deploy;
pub use import::phase1_import_objects;
pub use pull::{PulledImage, phase2_pull_image};
pub use rollback::run_rollback;
pub use seal::phase3_create_image;

pub use boot::find_esp_or_mount;

pub(crate) use seal::{build_origin_content, patch_boot_digest_in_content};

use crate::VerityDigest;
use crate::preflight::PreflightReport;
use crate::registry::{extract_files_via_registry, extract_subtree_via_registry};
use crate::xattr;
use anyhow::{Context, Result, anyhow};
use kernel_options::get_kernel_options;
use os_release::{bls_entry_filename, bls_entry_title, read_os_release};
use rustix::fs::{FlockOperation, flock};
use rustix::io::Errno;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

// ---- Lock file ----

const LOCK_PATH: &str = "/var/run/bootc-migrate.lock";

fn acquire_lock() -> Result<File> {
    let lock = File::create(LOCK_PATH).context("failed to create lock file")?;
    // Non-blocking exclusive advisory lock, released when this fd is closed
    // (i.e. on process exit). Guards against concurrent migration runs.
    match flock(&lock, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {}
        Err(Errno::WOULDBLOCK | Errno::ACCESS) => {
            return Err(anyhow!(
                "Another instance of bootc-migrate is already running (lock held at {}).",
                LOCK_PATH
            ));
        }
        Err(e) => return Err(e).context("failed to acquire lock"),
    }
    // Write PID so admins can inspect.
    let _ = writeln!(&lock, "{}", std::process::id());
    Ok(lock)
}

// ---- Mount guard (Optional: safe cleanup of TempDir-backed mounts) ----

pub(crate) struct MountGuard {
    mount_path: PathBuf,
}

impl MountGuard {
    fn new(mount_path: &Path) -> Self {
        MountGuard {
            mount_path: mount_path.to_path_buf(),
        }
    }
}

impl Drop for MountGuard {
    fn drop(&mut self) {
        let status = Command::new("umount").arg(&self.mount_path).status();
        match status {
            Ok(s) if s.success() => {}
            _ => eprintln!(
                "Warning: failed to unmount {} — a stale mount may remain. Use 'umount {}' manually.",
                self.mount_path.display(),
                self.mount_path.display(),
            ),
        }
    }
}

/// Inhibits system sleep/suspend during migration using systemd-inhibit if available (issue #27).
#[derive(Debug)]
pub struct SleepGuard {
    child: Option<std::process::Child>,
}

impl SleepGuard {
    pub fn new(why: &str) -> Self {
        let child = Command::new("systemd-inhibit")
            .args([
                "--what=sleep",
                &format!("--why={why}"),
                "--mode=block",
                "sleep",
                "infinity",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok();

        if child.is_some() {
            println!("Acquired systemd sleep inhibitor lock.");
        } else {
            eprintln!("Note: systemd-inhibit unavailable; sleep inhibitor lock was not acquired.");
        }

        SleepGuard { child }
    }
}

impl Drop for SleepGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
            println!("Released systemd sleep inhibitor lock.");
        }
    }
}

/// RAII guard around `podman image mount`. Mounts a locally-cached OCI image and
/// exposes its merged rootfs at `path`, unmounting on drop. Used as the Phase 5
/// fallback when the composefs overlay mount yields no usable content (bootc
/// mounts in a private namespace that does not persist to our process). Because
/// Phase 2 also `podman pull`s the image, this needs no network.
pub(crate) struct PodmanImageMount {
    image: String,
    path: PathBuf,
}

impl PodmanImageMount {
    fn new(image: &str) -> Result<Self> {
        let out = Command::new("podman")
            .args(["image", "mount", image])
            .output()
            .context("failed to execute podman image mount")?;
        if !out.status.success() {
            return Err(anyhow!(
                "podman image mount {} failed: {}",
                image,
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
        if !path.is_dir() {
            return Err(anyhow!(
                "podman image mount returned non-directory path: {}",
                path.display()
            ));
        }
        Ok(PodmanImageMount {
            image: image.to_string(),
            path,
        })
    }
}

impl Drop for PodmanImageMount {
    fn drop(&mut self) {
        let _ = Command::new("podman")
            .args(["image", "unmount", &self.image])
            .status();
    }
}

// ---- Public API ----

/// Main migration entry point. Orchestrates all 5 phases.
///
/// `etc_overrides` carries the interactive Config Drift Review's per-path
/// decisions (issue #15's "Phase 0.5"), if the caller ran that review; pass
/// `None` to fall back to the default 3-way `/etc` merge behavior
/// unconditionally.
pub fn run_migration(
    report: &PreflightReport,
    target_image: &str,
    dry_run: bool,
    skip_import: bool,
    bootloader: &str,
    force: bool,
    etc_overrides: Option<&crate::mergetc::EtcDriftManifest>,
) -> Result<()> {
    // Acquire exclusive lock.
    let _lock = if !dry_run {
        Some(acquire_lock()?)
    } else {
        None
    };

    // Acquire systemd sleep inhibitor lock (issue #27).
    let _sleep_guard = if !dry_run {
        Some(SleepGuard::new("OSTree to ComposeFS migration in progress"))
    } else {
        None
    };

    if dry_run {
        println!("[DRY RUN] Would execute migration phases without making changes.");
    }

    // Mount /sysroot and /boot read-write.
    if !dry_run {
        let sysroot_status = Command::new("/usr/bin/mount")
            .args(["-o", "remount,rw", "/sysroot"])
            .status()
            .context("failed to execute mount remount,rw /sysroot")?;
        if !sysroot_status.success() {
            return Err(anyhow!(
                "failed to remount /sysroot read-write — cannot proceed with migration"
            ));
        }
        let boot_status = Command::new("/usr/bin/mount")
            .args(["-o", "remount,rw", "/boot"])
            .status()
            .context("failed to execute mount remount,rw /boot")?;
        if !boot_status.success() {
            return Err(anyhow!(
                "failed to remount /boot read-write — cannot proceed with migration"
            ));
        }
    } else {
        println!("[DRY RUN] Would remount /sysroot and /boot read-write.");
    }

    // ---- Phase 0: preflight free-space check ----
    println!("=== Phase 0: Free-space check ===");
    if !dry_run {
        host_storage::check_free_space(report.supports_reflink)?;
    } else {
        println!("[DRY RUN] Would check free space on /sysroot/composefs.");
    }

    // ---- XFS workaround: ensure composefs store supports fs-verity ----
    let _loopback_guard: Option<MountGuard> = if !dry_run {
        host_storage::prepare_composefs_storage(report)?
    } else {
        let fs_type = report.fs_type.as_deref().unwrap_or("unknown");
        if fs_type == "xfs" {
            println!("[DRY RUN] Would set up ext4 loopback at /sysroot/composefs for fs-verity.");
        }
        None
    };

    // ---- Phase 1: Import OSTree objects (optional / deletable) ----
    // Ensure composefs repository directory exists before any phase touches it.
    if !dry_run {
        fs::create_dir_all("/sysroot/composefs")
            .context("failed to create composefs repository directory")?;
    }

    if !skip_import {
        phase1_import_objects(report, dry_run)?;
    } else {
        println!("=== Phase 1: Skipped (--skip-import) ===");
    }

    // ---- Phase 2: Pull OCI image ----
    // The target bootc reads this store after reboot, so it chooses the
    // writer generation. Modern targets use composefs-rs directly; legacy
    // targets retain the CLI path. Keep the feature-off fallback for minimal
    // downstream builds that intentionally opt out of the native backend.
    #[cfg(feature = "composefs-native")]
    let store = crate::composefs::TargetStore::new(target_image);
    #[cfg(not(feature = "composefs-native"))]
    let store = crate::composefs::BootcCliStore::default();
    let pulled_image = phase2_pull_image(&store, target_image, dry_run)?;

    // ---- Phase 3: Create and seal EROFS image ----
    let (verity, sealed_config) =
        phase3_create_image(&store, target_image, &pulled_image.config_digest, dry_run)?;

    // ---- Phase 4: Stage deployment state ----
    let _deploy_dir = phase4_stage_deploy(
        &verity,
        target_image,
        &pulled_image,
        &sealed_config,
        dry_run,
        force,
        etc_overrides,
    )?;

    // ---- Phase 5: Setup bootloader ----
    phase5_setup_bootloader(
        report,
        &verity,
        &pulled_image.image_reference,
        &sealed_config,
        dry_run,
        bootloader,
        force,
    )?;

    println!("\n=== MIGRATION COMPLETED ===");
    println!("Staged ComposeFS deployment: {}", verity.as_hex());
    let use_systemd_boot = bootloader != "grub2" && report.is_uefi && report.nvram_writable;
    if use_systemd_boot {
        println!("Primary bootloader: systemd-boot");
    } else {
        println!("Primary bootloader: GRUB2 (BLS Type 1)");
    }
    println!("Please reboot the system to finalize the transition.");
    println!("After successful boot, run 'bootc-migrate commit' to make composefs permanent.");
    if !dry_run {
        // Best-effort: a login reminder is a courtesy, not a migration
        // requirement — don't fail an otherwise-successful migration over it.
        if let Err(e) = crate::motd::write_migration_reminder(verity.as_hex()) {
            eprintln!("Warning: failed to write login reminder: {e:#}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sleep_guard_creation_and_drop() {
        let guard = SleepGuard::new("unit test migration");
        drop(guard);
    }
}
