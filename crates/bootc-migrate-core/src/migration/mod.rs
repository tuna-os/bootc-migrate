pub mod boot;
pub mod bootloader;
pub mod deploy;
pub mod deploy_layout;
pub mod etc_transition;
pub mod host_storage;
pub mod image_access;
pub mod import;
pub mod initrd;
pub mod kernel_options;
pub mod lifecycle;
pub mod mount;
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
// Public API: bootc-rebase constructs this as `migration::SleepGuard`.
pub use lifecycle::SleepGuard;

pub(crate) use lifecycle::MigrationLifecycle;
pub(crate) use mount::{MountGuard, PodmanImageMount};
pub(crate) use seal::{build_origin_content, patch_boot_digest_in_content};

use crate::VerityDigest;
use crate::preflight::PreflightReport;
use crate::registry::extract_files_via_registry;
use anyhow::{Context, Result, anyhow};
use kernel_options::get_kernel_options;
use os_release::{bls_entry_filename, bls_entry_title, read_os_release};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

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
    // Hold the mutation guards for the whole run; a dry run holds none.
    let _lifecycle = MigrationLifecycle::acquire(dry_run)?;

    // ---- Phase 0: preflight free-space check ----
    println!("=== Phase 0: Free-space check ===");
    if !dry_run {
        host_storage::check_free_space(report.supports_reflink)?;
    } else {
        println!("[DRY RUN] Would check free space on /sysroot/composefs.");
    }

    // ---- XFS workaround: ensure composefs store supports fs-verity ----
    let _loopback_guard = host_storage::prepare_composefs_storage(report, dry_run)?;

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
