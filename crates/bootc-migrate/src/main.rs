use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process;

use bootc_migrate_core::{migration, preflight, runlog, transaction};

mod drift_review;
mod tui;

#[derive(Parser, Debug)]
#[command(name = "bootc-migrate")]
#[command(about = "In-place migration utility from OSTree backend to ComposeFS backend", long_about = None)]
#[command(version = env!("BUILD_GIT_HASH"))]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// Target bootable container image to migrate to (e.g., ghcr.io/projectbluefin/dakota:stable)
    #[arg(short, long)]
    target_image: Option<String>,

    /// Skip preflight validation checks (unrecommended, use with caution)
    #[arg(long)]
    skip_preflight: bool,

    /// Force migration even if warnings are encountered
    #[arg(short, long)]
    force: bool,

    /// Bootloader to use: "systemd-boot" (default, when UEFI), "grub2", or "auto"
    #[arg(long, default_value = "systemd-boot")]
    bootloader: String,

    /// Dry-run: print every action without executing
    #[arg(long)]
    dry_run: bool,

    /// Skip Phase 1 (OSTree object import)
    #[arg(long)]
    skip_import: bool,

    /// Interactively review /etc config drift before migration begins
    /// ("Phase 0.5", issue #15). Checked entries keep the user's live
    /// version (today's default); unchecked entries take the target's new
    /// default. Mutually exclusive with --etc-drift-manifest.
    #[arg(long)]
    review_drift: bool,

    /// Load a previously-saved Config Drift Review manifest (see
    /// `etc-drift --interactive --output <path>`) instead of prompting
    /// interactively. Mutually exclusive with --review-drift.
    #[arg(long)]
    etc_drift_manifest: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Commit the composefs deployment as the permanent default (after successful boot).
    ///
    /// Permanently deletes the OSTree-Bluefin deployment from disk: removes
    /// /sysroot/ostree (object store + deploys + leaked /var copy), drops
    /// stale /boot/loader/entries/ostree-*.conf, removes GRUB2 bits when
    /// migrated to systemd-boot, refreshes /sysroot/.bootc-aleph.json.
    /// The composefs system becomes byte-shape identical to a fresh
    /// `bootc install` of the target image.
    #[command(name = "commit")]
    Commit {
        /// Preview deletions and reclaimed bytes; touch nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Undo a partial or failed migration — remove composefs boot artifacts
    /// and staged deployments while preserving the composefs object store.
    ///
    /// Removes staged deployments, boot artifacts, BLS entries from ESP.
    /// Does NOT touch the composefs object store or loopback image — those
    /// are expensive to rebuild and survive across retries. Use --full for
    /// complete cleanup including the object store.
    #[command(name = "undo")]
    Undo {
        /// Preview what would be removed; touch nothing.
        #[arg(long)]
        dry_run: bool,
        /// Full cleanup: also remove composefs object store and loopback image.
        #[arg(long)]
        full: bool,
    },
    /// Return to the previous OSTree deployment (re-order UEFI BootOrder to OSTree/GRUB).
    #[command(name = "rollback")]
    Rollback {
        /// Reboot immediately after re-ordering UEFI BootOrder
        #[arg(long)]
        reboot: bool,
        /// Preview changes without modifying UEFI BootOrder or rebooting
        #[arg(long)]
        dry_run: bool,
    },
    /// Launch the interactive TUI wizard.
    #[command(name = "tui")]
    Tui,
    /// Show config drift between the OSTree factory default /etc and live
    /// /etc — the "Config Drift Review" step (issue #15). Read-only;
    /// independent of any migration target. Reports the
    /// Added/Modified/Removed/TypeChanged categorization that also drives
    /// the interactive review (`--interactive`) and Phase 4's `/etc` merge.
    #[command(name = "etc-drift")]
    EtcDrift {
        /// Output as machine-readable JSON instead of a table.
        #[arg(long)]
        json: bool,
        /// Launch the interactive checklist instead of printing a static
        /// report, and write the resulting decisions manifest to --output.
        #[arg(long)]
        interactive: bool,
        /// Where to write the decisions manifest when --interactive is
        /// used. Consumed later via `--etc-drift-manifest` on a migration
        /// run.
        #[arg(long, default_value = "/var/tmp/bootc-migrate-etc-drift.json")]
        output: PathBuf,
    },
    /// Move a system Steam library and user settings into Flatpak Steam.
    ///
    /// Run as the desktop user, with Steam fully stopped. This moves only
    /// Steam's game library, user data, and configuration using filesystem
    /// renames; it never copies game trees. Existing Flatpak state is kept in
    /// a rollback directory below ~/.var/app/com.valvesoftware.Steam.
    #[command(name = "system-to-flatpak-steam")]
    SystemToFlatpakSteam {
        /// Preview the validated moves without changing Steam data.
        #[arg(long)]
        dry_run: bool,
    },
    /// Convert the bootloader between GRUB2 and systemd-boot without
    /// touching the rootfs backend (issue #65).
    ///
    /// Copies kernel/initrd for every existing deployment (OSTree or
    /// composefs) onto the ESP (systemd-boot) or /boot (GRUB2), writes
    /// BLS entries, and registers the bootloader in UEFI NVRAM. GRUB is
    /// retained as a fallback entry.
    #[command(name = "migrate-bootloader")]
    MigrateBootloader {
        /// Target bootloader: "systemd-boot" or "grub2".
        #[arg(long, default_value = "systemd-boot")]
        to: String,
        /// Preview actions without executing.
        #[arg(long)]
        dry_run: bool,
        /// Force migration even if readiness checks fail.
        #[arg(short, long)]
        force: bool,
        /// OCI image reference to extract systemd-boot from (when the
        /// running system does not ship it). Streamed layer-by-layer from
        /// the registry without pulling the full image.
        #[arg(long)]
        from_image: Option<String>,
    },
}

fn check_root_privilege() -> Result<()> {
    if !rustix::process::getuid().is_root() {
        return Err(anyhow!(
            "This command must be run as root (e.g., using sudo)."
        ));
    }
    Ok(())
}

fn main() {
    let args = Args::parse();

    // Tee stdout+stderr to the persistent log via a pipe so output is visible
    // both on the terminal (over SSH for E2E) and in the log, which is the
    // only copy that survives the reboot the migration asks for.
    let mut tee_guard = runlog::start(
        "/var/log/bootc-migrate.log",
        "bootc-migrate",
        env!("BUILD_GIT_HASH"),
    );

    // Drain the tee thread (flushing all buffered output to terminal + log)
    // then exit. process::exit() skips Rust destructors, so without this
    // the last few lines of output (including the error message) are lost.
    macro_rules! exit_flushed {
        ($code:expr) => {{
            use std::io::Write;
            let _ = std::io::stdout().flush();
            let _ = std::io::stderr().flush();
            if let Some(g) = tee_guard.take() {
                g.finish();
            }
            process::exit($code);
        }};
    }

    // Handle --commit subcommand
    if let Some(Command::Commit { dry_run }) = args.command {
        let result = run_commit(dry_run);
        if let Err(e) = result {
            eprintln!("Error: {}", e);
            exit_flushed!(1);
        }
        if let Some(g) = tee_guard.take() {
            g.finish();
        }
        return;
    }

    // Handle --undo subcommand
    if let Some(Command::Undo { dry_run, full }) = args.command {
        let result = run_undo(dry_run, full);
        if let Err(e) = result {
            eprintln!("Error: {}", e);
            exit_flushed!(1);
        }
        if let Some(g) = tee_guard.take() {
            g.finish();
        }
        return;
    }

    // Handle --rollback subcommand
    if let Some(Command::Rollback { reboot, dry_run }) = args.command {
        let result = run_rollback(reboot, dry_run);
        if let Err(e) = result {
            eprintln!("Error: {}", e);
            exit_flushed!(1);
        }
        if let Some(g) = tee_guard.take() {
            g.finish();
        }
        return;
    }

    // Handle `migrate-bootloader` subcommand
    if let Some(Command::MigrateBootloader {
        to,
        dry_run,
        force,
        from_image,
    }) = args.command
    {
        let result = run_migrate_bootloader(&to, dry_run, force, from_image.as_deref());
        if let Err(e) = result {
            eprintln!("Error: {:#}", e);
            exit_flushed!(1);
        }
        if let Some(g) = tee_guard.take() {
            g.finish();
        }
        return;
    }

    // Handle `etc-drift` subcommand
    if let Some(Command::EtcDrift {
        json,
        interactive,
        output,
    }) = args.command
    {
        let result = if interactive {
            run_etc_drift_interactive(&output)
        } else {
            run_etc_drift(json)
        };
        if let Err(e) = result {
            eprintln!("Error: {}", e);
            exit_flushed!(1);
        }
        if let Some(g) = tee_guard.take() {
            g.finish();
        }
        return;
    }

    // Handle the per-user system Steam -> Flatpak Steam conversion.
    if let Some(Command::SystemToFlatpakSteam { dry_run }) = args.command {
        let result = run_system_to_flatpak_steam(dry_run);
        if let Err(e) = result {
            eprintln!("Error: {:#}", e);
            exit_flushed!(1);
        }
        if let Some(g) = tee_guard.take() {
            g.finish();
        }
        return;
    }

    // Handle explicit `tui` subcommand, or fall into the wizard automatically
    // when no target image was given on the command line. Root isn't required
    // just to browse the wizard — the migration subprocess it spawns on Run
    // enforces that itself.
    if matches!(args.command, Some(Command::Tui)) || args.target_image.is_none() {
        let result = tui::run_tui();
        if let Err(e) = result {
            eprintln!("Error: {}", e);
            exit_flushed!(1);
        }
        if let Some(g) = tee_guard.take() {
            g.finish();
        }
        return;
    }

    if let Err(e) = check_root_privilege() {
        eprintln!("Error: {}", e);
        exit_flushed!(1);
    }

    let target_image = match args.target_image {
        Some(t) => t,
        None => {
            eprintln!("Error: --target-image is required for migration");
            exit_flushed!(1);
        }
    };

    // Validate target_image to prevent INI injection in the .origin file.
    if target_image.contains('\n') || target_image.contains('\r') || target_image.contains('\0') {
        eprintln!("Error: --target-image contains invalid characters (newlines, nulls).");
        exit_flushed!(1);
    }

    let version = env!("BUILD_GIT_HASH");
    println!("=== OSTree to ComposeFS Migration Utility v{} ===", version);
    if args.dry_run {
        println!("*** DRY RUN MODE — no changes will be made ***");
    }
    println!("Checking system state...");

    let report = match preflight::run_preflight_checks() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Preflight failure: {}", e);
            if !args.skip_preflight {
                exit_flushed!(1);
            }
            preflight::PreflightReport {
                is_bootc_ostree: true,
                pending_transaction: preflight::PendingTransactionStatus::Clean,
                is_uefi: true,
                nvram_writable: true,
                esp_path: Some("/boot/efi".to_string()),
                esp_free_space_bytes: 500 * 1024 * 1024,
                esp_fs_type: Some("vfat".to_string()),
                supports_reflink: true,
                is_btrfs: true,
                fs_type: Some("btrfs".to_string()),
                ostree_repo_size_bytes: 0,
                composefs_free_bytes: 0,
                // Permissive like every other field here: preflight failed and
                // the user passed --skip-preflight, so nothing was measured.
                // Warning about space we never looked at would be noise.
                container_storage_free_bytes: u64::MAX,
                container_storage_path: "/var/lib/containers/storage".to_string(),
                var_is_separate_mount: false,
                esp_ready_for_systemd_boot: true,
                systemd_boot_binaries_present: false,
                grub_tools_available: true,
                esp_detected: false,
                sysroot_was_ro: false,
            }
        }
    };

    preflight::readiness::print_report(&report);
    preflight::readiness::print_readiness(&report);

    match preflight::readiness::gate(&report, args.force, args.skip_preflight) {
        preflight::readiness::MigrationGate::Proceed => {}
        preflight::readiness::MigrationGate::Refuse(reason) => {
            eprintln!("Error: {}", reason);
            exit_flushed!(1);
        }
        preflight::readiness::MigrationGate::ConfirmFullCopy => {
            println!(
                "Warning: Reflink support not detected on /sysroot. Migration will perform a full copy of repository objects, which will require significant disk space."
            );
            print!("Do you want to proceed anyway? (y/N): ");
            let mut input = String::new();
            if std::io::stdin().read_line(&mut input).is_ok() {
                let input = input.trim().to_lowercase();
                if input != "y" && input != "yes" {
                    println!("Migration aborted.");
                    exit_flushed!(0);
                }
            } else {
                println!("Migration aborted.");
                exit_flushed!(0);
            }
        }
    }

    // ---- Phase 0.5: Config Drift Review (issue #15) ----
    // Read-only and before any mutation below this point: preflight above
    // only inspects the system, and run_migration is the first thing that
    // remounts /sysroot and /boot read-write.
    if args.review_drift && args.etc_drift_manifest.is_some() {
        eprintln!("Error: --review-drift and --etc-drift-manifest are mutually exclusive.");
        exit_flushed!(1);
    }

    let mut etc_overrides: Option<bootc_migrate_core::mergetc::EtcDriftManifest> = None;

    if args.review_drift {
        println!("=== Phase 0.5: Config Drift Review ===");
        match migration::etc_transition::compute_etc_drift() {
            Ok(drift) => match drift_review::run_review(drift) {
                Ok(Some(manifest)) => etc_overrides = Some(manifest),
                Ok(None) => {
                    println!("Config Drift Review cancelled; aborting migration.");
                    exit_flushed!(0);
                }
                Err(e) => {
                    eprintln!("Error running Config Drift Review: {:#}", e);
                    exit_flushed!(1);
                }
            },
            Err(e) => {
                eprintln!(
                    "Warning: failed to compute /etc config drift ({:#}); skipping review.",
                    e
                );
            }
        }
    } else if let Some(path) = &args.etc_drift_manifest {
        match read_etc_drift_manifest(path) {
            Ok(manifest) => etc_overrides = Some(manifest),
            Err(e) => {
                eprintln!("Error: {:#}", e);
                exit_flushed!(1);
            }
        }
    }

    println!("Starting migration to OCI image: {}...", target_image);
    if let Err(e) = migration::run_migration(
        &report,
        &target_image,
        args.dry_run,
        args.skip_import,
        &args.bootloader,
        args.force,
        etc_overrides.as_ref(),
    ) {
        eprintln!("\nMigration Failed: {:#}", e);
        exit_flushed!(1);
    }
}

/// Commit the composefs deployment as the permanent default.
fn run_commit(dry_run: bool) -> Result<()> {
    check_root_privilege()?;
    transaction::commit(dry_run)
}

fn run_undo(dry_run: bool, full: bool) -> Result<()> {
    check_root_privilege()?;
    transaction::undo(dry_run, full)
}

fn run_rollback(reboot: bool, dry_run: bool) -> Result<()> {
    check_root_privilege()?;
    migration::run_rollback(reboot, dry_run)
}

/// Show config drift between the OSTree factory default /etc and live /etc
/// (issue #15). Read-only; does not require root (only reads /proc/cmdline,
/// /sysroot/ostree/deploy/.../usr/etc, and /etc).
fn run_etc_drift(json: bool) -> Result<()> {
    let drift = migration::etc_transition::compute_etc_drift()?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&drift).expect("drift entries always serialize")
        );
        return Ok(());
    }
    if drift.is_empty() {
        println!("No /etc config drift from the OSTree factory default.");
        return Ok(());
    }
    println!("=== /etc Config Drift ({} change(s)) ===", drift.len());
    for entry in &drift {
        let kind = match entry.kind {
            bootc_migrate_core::mergetc::DriftKind::Added => "Added",
            bootc_migrate_core::mergetc::DriftKind::Modified => "Modified",
            bootc_migrate_core::mergetc::DriftKind::Removed => "Removed",
            bootc_migrate_core::mergetc::DriftKind::TypeChanged => "TypeChanged",
        };
        println!("  {:<50} [{}]", format!("/etc/{}", entry.path), kind);
    }
    Ok(())
}

/// Run the interactive Config Drift Review checklist and write the
/// resulting manifest to `output` (issue #15).
fn run_etc_drift_interactive(output: &std::path::Path) -> Result<()> {
    let drift = migration::etc_transition::compute_etc_drift()?;
    match drift_review::run_review(drift)? {
        Some(manifest) => {
            write_etc_drift_manifest(output, &manifest)?;
            println!(
                "Wrote Config Drift Review manifest ({} decision(s)) to {}",
                manifest.decisions.len(),
                output.display()
            );
            Ok(())
        }
        None => {
            println!("Config Drift Review cancelled; no manifest written.");
            Ok(())
        }
    }
}

/// Serialize `manifest` and write it to `path` (I/O; parsing itself is
/// `EtcDriftManifest::to_json`, kept pure and unit-tested in
/// `bootc-migrate-core`).
fn write_etc_drift_manifest(
    path: &std::path::Path,
    manifest: &bootc_migrate_core::mergetc::EtcDriftManifest,
) -> Result<()> {
    let json = manifest.to_json()?;
    std::fs::write(path, json)
        .with_context(|| format!("failed to write etc-drift manifest to {}", path.display()))?;
    Ok(())
}

/// Read and parse a previously-saved Config Drift Review manifest (I/O;
/// parsing itself is `EtcDriftManifest::parse`, kept pure and unit-tested in
/// `bootc-migrate-core`).
fn read_etc_drift_manifest(
    path: &std::path::Path,
) -> Result<bootc_migrate_core::mergetc::EtcDriftManifest> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read etc-drift manifest {}", path.display()))?;
    bootc_migrate_core::mergetc::EtcDriftManifest::parse(&contents)
}

fn run_migrate_bootloader(
    to: &str,
    dry_run: bool,
    force: bool,
    from_image: Option<&str>,
) -> Result<()> {
    check_root_privilege()?;
    migration::migrate_bootloader_standalone(to, dry_run, force, from_image)
}

fn run_system_to_flatpak_steam(dry_run: bool) -> Result<()> {
    if rustix::process::getuid().is_root() {
        bail!("system-to-flatpak-steam must be run as the desktop user, not via sudo");
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set; run as the desktop user")?;
    if !home.is_absolute() {
        bail!("HOME must be an absolute path, got {}", home.display());
    }

    if !dry_run {
        bootc_migrate_core::steam_flatpak::ensure_steam_is_stopped()?;
    }
    let outcome = bootc_migrate_core::steam_flatpak::migrate(&home, dry_run)?;
    let plan = outcome.plan;

    println!("=== System Steam to Flatpak Steam ===");
    println!("System Steam:  {}", plan.native_root.display());
    println!("Flatpak Steam: {}", plan.flatpak_root.display());
    println!(
        "Library path:  {} -> {}",
        plan.native_library_path, plan.flatpak_library_path
    );
    for directory in &plan.portable_directories {
        println!(
            "{} {} -> {}",
            if dry_run {
                "[dry-run] would move"
            } else {
                "Moved"
            },
            plan.native_root.join(directory).display(),
            plan.flatpak_root.join(directory).display()
        );
    }
    if let Some(backup) = outcome.backup_dir {
        println!("Preserved previous Flatpak state in {}", backup.display());
        println!(
            "Start Flatpak Steam and validate the library before removing any native Steam runtime files."
        );
    } else {
        println!("No changes made.");
    }
    Ok(())
}
