//! `bootc-rebase` — universal bootc re-base engine.
//!
//! Consumes `bootc-migrate-core` to re-base a bootc system between backends,
//! bootloaders, and images. Today the OSTree → ComposeFS route drives the
//! core pipeline directly; the routing table in [`routing`] tracks what else
//! is planned. See issues #30 and #45 in tuna-os/bootc-migrate for
//! the roadmap.

use anyhow::{Context, Result, bail};
use bootc_migrate_core::migration;
use bootc_migrate_core::preflight::{self, readiness};
use bootc_migrate_core::{registry, remap, scan};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

mod routing;

use routing::{Backend, Strategy, route};

#[derive(Parser, Debug)]
#[command(name = "bootc-rebase")]
#[command(about = "Re-base a bootc system between backends, bootloaders, and images", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    #[command(flatten)]
    rebase_args: Args,
}

#[derive(Subcommand, Debug, Clone)]
enum Commands {
    /// Inspect target container image capabilities via registry streaming
    Scan(ScanArgs),
    /// Re-base system
    Rebase(Args),
    /// Return to the previous OSTree deployment (re-order UEFI BootOrder to OSTree/GRUB)
    Rollback(RollbackArgs),
    /// GRUB2 -> systemd-boot bootloader migration (issue #65). NOT YET
    /// IMPLEMENTED: the ESP/NVRAM mutation and the kernel-install resync
    /// hook (without which a flipped system would silently boot stale
    /// kernels after the next update) don't exist yet. This subcommand
    /// exists so the CLI shape and pure BLS-entry/karg-carry-over/
    /// entry-token logic (`bootc_migrate_core::migration::bootloader::systemd_boot`)
    /// can be reviewed ahead of the live mutation work.
    MigrateBootloader(MigrateBootloaderArgs),
    /// Enumerate and classify UEFI boot entries (issue #31). Read-only:
    /// reports which entries look dead/generic/duplicate/firmware-managed
    /// without removing or renaming anything. Interactive cleanup and
    /// branding-rename are not implemented yet.
    BootEntries(BootEntriesArgs),
    /// Stash or restore a user's DE config around a cross-DE re-base (issue
    /// #68). Off by default: only runs when this subcommand is invoked
    /// explicitly. NOT wired into `rebase`/`scan` yet — target-image DE
    /// detection needs the cross-base hardening this issue is gated behind,
    /// so callers currently pass `--from-de`/`--to-de` explicitly.
    DeMigrate(DeMigrateArgs),
}

#[derive(clap::Args, Debug, Clone)]
struct DeMigrateArgs {
    #[command(subcommand)]
    action: DeMigrateAction,
}

#[derive(Subcommand, Debug, Clone)]
enum DeMigrateAction {
    /// Move the outgoing DE's per-user config out of `$HOME` into the stash.
    Stash {
        /// Desktop environment being left ("gnome" or "kde")
        #[arg(long)]
        from_de: String,
        /// User home directory to stash config out of
        #[arg(long)]
        home: PathBuf,
        /// Root directory to stash config into
        #[arg(long, default_value = ".local/share/de-migrate")]
        stash_dir: PathBuf,
        /// Also run pre-switch.d hooks (with REBASE_* env vars set) after stashing
        #[arg(long)]
        run_hooks: bool,
        /// Print what would happen without touching the filesystem or running hooks
        #[arg(long)]
        dry_run: bool,
    },
    /// Move a previously stashed DE's config back into `$HOME`.
    Restore {
        /// Desktop environment being returned to ("gnome" or "kde")
        #[arg(long)]
        to_de: String,
        /// User home directory to restore config into
        #[arg(long)]
        home: PathBuf,
        /// Root directory config was stashed into
        #[arg(long, default_value = ".local/share/de-migrate")]
        stash_dir: PathBuf,
        /// Also run post-switch.d hooks (with REBASE_* env vars set) after restoring
        #[arg(long)]
        run_hooks: bool,
        /// Print what would happen without touching the filesystem or running hooks
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(clap::Args, Debug, Clone)]
struct BootEntriesArgs {
    /// Output as machine-readable JSON instead of a table.
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug, Clone)]
struct MigrateBootloaderArgs {
    /// Target bootloader (only "systemd-boot" is planned)
    #[arg(long, default_value = "systemd-boot")]
    to: String,

    /// Dry-run: print every action without executing
    #[arg(long)]
    dry_run: bool,

    /// Undo a previous migrate-bootloader run
    #[arg(long)]
    undo: bool,
}

#[derive(clap::Args, Debug, Clone)]
struct RollbackArgs {
    /// Reboot immediately after re-ordering UEFI BootOrder
    #[arg(long)]
    reboot: bool,

    /// Dry-run: print every action without executing
    #[arg(long)]
    dry_run: bool,
}

#[derive(clap::Args, Debug, Clone)]
struct ScanArgs {
    /// Target container image to scan (e.g. ghcr.io/projectbluefin/dakota:stable)
    image: String,

    /// Output capabilities as machine-readable JSON
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug, Clone)]
struct Args {
    /// Target bootable container image (e.g. ghcr.io/projectbluefin/dakota:stable)
    #[arg(short, long, default_value = "")]
    target_image: String,

    /// Source backend: "auto" (detect), "ostree", or "composefs"
    #[arg(long, default_value = "auto")]
    source_backend: String,

    /// Target backend: "ostree" or "composefs"
    #[arg(long, default_value = "composefs")]
    target_backend: String,

    /// Bootloader to use: "systemd-boot" (default, when UEFI), "grub2", or "auto"
    #[arg(long, default_value = "systemd-boot")]
    bootloader: String,

    /// Force the re-base even if readiness warnings are encountered
    #[arg(short, long)]
    force: bool,

    /// Skip preflight validation checks (unrecommended, use with caution)
    #[arg(long)]
    skip_preflight: bool,

    /// Skip OSTree object import (phase 1)
    #[arg(long)]
    skip_import: bool,

    /// Dry-run: print every action without executing
    #[arg(long)]
    dry_run: bool,

    /// Print the planned route and exit without touching the system
    #[arg(long)]
    plan: bool,

    /// Acknowledge a cross-base re-base (host and target disagree on
    /// ID/ID_LIKE) and proceed with its UID/GID remap (#67). Without this,
    /// a detected cross-base re-base is refused after printing the remap
    /// report so the blast radius is visible first.
    #[arg(long)]
    accept_cross_base: bool,
}

fn print_capabilities_table(image: &str, caps: &bootc_migrate_core::scan::Capabilities) {
    println!("=== Target image capabilities ===");
    println!("Image:                 {image}");
    println!(
        "Composefs:             {}",
        if caps.composefs_capable {
            "capable"
        } else {
            "not enabled in prepare-root.conf"
        }
    );
    println!(
        "OSTree capable:        {}",
        if caps.ostree_capable { "yes" } else { "no" }
    );
    println!(
        "Bootloader payload:    {}",
        if caps.systemd_boot_payload {
            "systemd-boot ✓"
        } else {
            "none"
        }
    );
    println!(
        "bootc present:         {}",
        if caps.bootc_present { "yes" } else { "no" }
    );
    println!(
        "Desktops:              {}",
        if caps.desktops.is_empty() {
            "none".to_string()
        } else {
            caps.desktops.join(", ")
        }
    );
    if let Some(base) = &caps.base {
        println!(
            "Base OS:               {} {}",
            base.id,
            base.version_id.as_deref().unwrap_or("")
        );
    } else {
        println!("Base OS:               unknown");
    }
    println!(
        "Sysusers:              {} static allocation(s)",
        caps.sysusers.len()
    );
    println!(
        "Transient root/etc:    {} / {}",
        if caps.root_transient { "yes" } else { "no" },
        if caps.etc_transient { "yes" } else { "no" }
    );
    println!(
        "fs-verity required:    {}",
        if caps.fs_verity_required { "yes" } else { "no" }
    );
    println!(
        "Initramfs composefs:   {}",
        if caps.initramfs_has_composefs_module {
            "module present"
        } else {
            "not present (may need regeneration for a composefs boot)"
        }
    );
    println!(
        "Filesystem expected:   {}",
        caps.filesystem_expectation.as_deref().unwrap_or("unknown")
    );
    let issues = bootc_migrate_core::scan::compatibility_issues(caps);
    println!(
        "Compatible:            {}",
        if issues.is_empty() { "YES" } else { "NO" }
    );
    for issue in &issues {
        println!("  - {issue}");
    }
}

/// Stub (issue #65): the pure BLS-entry/karg-carry-over/entry-token core
/// lives in `bootc_migrate_core::migration::bootloader::systemd_boot` and is
/// unit-tested, but the live ESP populate + NVRAM cutover + kernel-install
/// resync hook aren't implemented — see the doc comment on
/// `Commands::MigrateBootloader`. Refuses unconditionally so this can't be
/// mistaken for a working migration.
fn run_migrate_bootloader(_args: &MigrateBootloaderArgs) -> Result<()> {
    bail!(
        "migrate-bootloader is not implemented yet (issue #65): the ESP/NVRAM mutation and \
         kernel-install resync hook don't exist. See \
         https://github.com/tuna-os/bootc-migrate/issues/65"
    );
}

/// Read-only UEFI boot-entry audit (issue #31). Runs `efibootmgr -v`, parses
/// and classifies every entry against the ESP's actual contents, and prints
/// the result. Never removes or renames anything.
fn run_boot_entries_audit(args: &BootEntriesArgs) -> Result<()> {
    use bootc_migrate_core::boot_audit::{self, AuditFlag};

    let out = std::process::Command::new("efibootmgr")
        .arg("-v")
        .output()
        .map_err(|e| anyhow::anyhow!("failed to execute efibootmgr -v: {e}"))?;
    if !out.status.success() {
        bail!(
            "efibootmgr -v failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let entries = boot_audit::parse_efibootmgr_entries(&stdout);

    let esp_root = migration::boot::find_esp_or_mount()
        .context("failed to locate the ESP for boot-entry audit")?;
    let audited = boot_audit::audit_entries(&entries, Path::new(&esp_root));

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&audited).expect("audited entries always serialize")
        );
        return Ok(());
    }

    println!("=== UEFI boot-entry audit ({} entries) ===", audited.len());
    for a in &audited {
        let marker = if a.entry.active { "*" } else { " " };
        let flag_str = if a.flags.is_empty() {
            "ok".to_string()
        } else {
            a.flags
                .iter()
                .map(|f| match f {
                    AuditFlag::Dead => "DEAD",
                    AuditFlag::GenericLabel => "generic-label",
                    AuditFlag::DuplicateLoaderPath => "duplicate",
                    AuditFlag::FirmwareManaged => "firmware",
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        println!(
            "  Boot{}{} {:<28} [{}]",
            a.entry.id, marker, a.entry.label, flag_str
        );
    }
    let preselect_count = audited.iter().filter(|a| a.safe_to_preselect()).count();
    println!(
        "\n{preselect_count} entry(ies) would be pre-selected for removal (clearly dead, not firmware-managed)."
    );
    println!(
        "No entries were modified — this is a read-only audit (interactive cleanup: issue #31, not implemented yet)."
    );
    Ok(())
}

fn parse_de(name: &str) -> Result<bootc_migrate_core::de_migrate::DesktopEnvironment> {
    use bootc_migrate_core::de_migrate::DesktopEnvironment;
    match name.to_ascii_lowercase().as_str() {
        "gnome" => Ok(DesktopEnvironment::Gnome),
        "kde" => Ok(DesktopEnvironment::Kde),
        other => bail!("unknown desktop environment '{other}' (expected \"gnome\" or \"kde\")"),
    }
}

fn run_de_migrate(args: &DeMigrateArgs) -> Result<()> {
    use bootc_migrate_core::de_migrate;

    match &args.action {
        DeMigrateAction::Stash {
            from_de,
            home,
            stash_dir,
            run_hooks,
            dry_run,
        } => {
            let de = parse_de(from_de)?;
            let moved =
                de_migrate::stash(de, home, stash_dir, *dry_run).context("stashing DE config")?;
            if moved.is_empty() {
                println!("Nothing to stash for {from_de} under {}.", home.display());
            } else {
                println!("Stashed {} path(s) for {from_de}:", moved.len());
                for p in &moved {
                    println!("  {p}");
                }
            }
            if *run_hooks {
                let hooks = de_migrate::discover_hooks(Path::new(de_migrate::PRE_SWITCH_HOOK_DIR))
                    .context("discovering pre-switch hooks")?;
                // to_de is unknown at this point (the target DE isn't detected by
                // this subcommand yet — see #68's "Depends on" note), so the
                // env var is left empty rather than guessed.
                let env = [
                    ("REBASE_FROM_DE".to_string(), from_de.clone()),
                    ("REBASE_TO_DE".to_string(), String::new()),
                    (
                        "REBASE_STASH_DIR".to_string(),
                        stash_dir.display().to_string(),
                    ),
                    ("REBASE_HOME".to_string(), home.display().to_string()),
                ];
                let results = de_migrate::run_hooks(&hooks, &env, *dry_run)
                    .context("running pre-switch hooks")?;
                print_hook_results(&results);
            }
            Ok(())
        }
        DeMigrateAction::Restore {
            to_de,
            home,
            stash_dir,
            run_hooks,
            dry_run,
        } => {
            let de = parse_de(to_de)?;
            let moved = de_migrate::restore(de, home, stash_dir, *dry_run)
                .context("restoring DE config")?;
            if moved.is_empty() {
                println!("Nothing to restore for {to_de} into {}.", home.display());
            } else {
                println!("Restored {} path(s) for {to_de}:", moved.len());
                for p in &moved {
                    println!("  {p}");
                }
            }
            if *run_hooks {
                let hooks = de_migrate::discover_hooks(Path::new(de_migrate::POST_SWITCH_HOOK_DIR))
                    .context("discovering post-switch hooks")?;
                let env = [
                    ("REBASE_FROM_DE".to_string(), String::new()),
                    ("REBASE_TO_DE".to_string(), to_de.clone()),
                    (
                        "REBASE_STASH_DIR".to_string(),
                        stash_dir.display().to_string(),
                    ),
                    ("REBASE_HOME".to_string(), home.display().to_string()),
                ];
                let results = de_migrate::run_hooks(&hooks, &env, *dry_run)
                    .context("running post-switch hooks")?;
                print_hook_results(&results);
            }
            Ok(())
        }
    }
}

fn print_hook_results(results: &[bootc_migrate_core::de_migrate::HookResult]) {
    if results.is_empty() {
        println!("No hooks found.");
        return;
    }
    for r in results {
        let status = if r.success { "ok" } else { "FAILED" };
        println!("  hook {} [{status}]", r.path);
    }
}

fn run_scan(args: &ScanArgs) -> Result<()> {
    println!("Scanning target image {}...", args.image);
    let caps = bootc_migrate_core::scan::scan_target_image(&args.image)?;
    if args.json {
        println!("{}", caps.to_json());
    } else {
        print_capabilities_table(&args.image, &caps);
    }
    Ok(())
}

fn parse_backend(s: &str) -> Result<Backend> {
    match s {
        "ostree" => Ok(Backend::Ostree),
        "composefs" => Ok(Backend::Composefs),
        other => bail!("unknown backend '{other}' (expected 'ostree' or 'composefs')"),
    }
}

fn detect_source_backend() -> Result<Backend> {
    let sys = preflight::SystemInfo::gather()?;
    if sys.is_bootc_ostree {
        Ok(Backend::Ostree)
    } else {
        // Not OSTree-booted; assume composefs (a later capability scan will
        // verify this properly — see issue #24).
        Ok(Backend::Composefs)
    }
}

fn check_root_privilege() -> Result<()> {
    if !rustix::process::getuid().is_root() {
        bail!("This command must be run as root (e.g., using sudo).");
    }
    Ok(())
}

/// Drive the proven OSTree → ComposeFS pipeline from bootc-migrate-core:
/// preflight, readiness report, gating, then the phase 0–5 migration.
fn run_core_migration(args: &Args) -> Result<()> {
    check_root_privilege()?;

    // Validate target_image to prevent INI injection in the .origin file.
    if args.target_image.contains('\n')
        || args.target_image.contains('\r')
        || args.target_image.contains('\0')
    {
        bail!("--target-image contains invalid characters (newlines, nulls).");
    }

    if args.dry_run {
        println!("*** DRY RUN MODE — no changes will be made ***");
    }
    println!("Checking system state...");

    let report = preflight::run_preflight_checks()?;
    readiness::print_report(&report);
    readiness::print_readiness(&report);

    match readiness::gate(&report, args.force, args.skip_preflight) {
        readiness::MigrationGate::Proceed => {}
        readiness::MigrationGate::Refuse(reason) => bail!("{reason}"),
        readiness::MigrationGate::ConfirmFullCopy => {
            // bootc-rebase is non-interactive by design: no prompt, just a
            // clear instruction (the migrator binary offers the y/N prompt).
            bail!(
                "Reflink support not detected on /sysroot — the migration would perform a \
                 full copy of repository objects. Re-run with --force to accept the extra \
                 disk usage."
            );
        }
    }

    println!("Starting migration to OCI image: {}...", args.target_image);
    migration::run_migration(
        &report,
        &args.target_image,
        args.dry_run,
        args.skip_import,
        &args.bootloader,
        args.force,
    )
}

fn execute_rebase(args: &Args) -> Result<()> {
    let from = if args.source_backend == "auto" {
        detect_source_backend()?
    } else {
        parse_backend(&args.source_backend)?
    };
    let to = parse_backend(&args.target_backend)?;

    let Some(r) = route(from, to) else {
        bail!("no route from {from} to {to}");
    };

    println!(
        "Route: {from} -> {to} via {:?} ({})",
        r.strategy,
        if r.implemented {
            "implemented"
        } else {
            "planned, not yet implemented"
        }
    );

    if args.plan {
        return Ok(());
    }

    if !r.implemented {
        bail!(
            "the {from} -> {to} route is not implemented yet; \
             see https://github.com/tuna-os/bootc-migrate/issues/30"
        );
    }

    if to == Backend::Composefs
        && !args.skip_preflight
        && let Ok(caps) = bootc_migrate_core::scan::scan_target_image(&args.target_image)
        && !caps.composefs_capable
        && !args.force
    {
        bail!(
            "Target image {} is not composefs-capable (prepare-root.conf lacks composefs enabled). \
             Use --force to override.",
            args.target_image
        );
    }

    match r.strategy {
        Strategy::CoreMigration => run_core_migration(args),
        Strategy::OstreeDeploy => run_ostree_deploy(args),
        Strategy::ImageSwap => run_image_swap(args),
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Scan(ref scan_args)) => run_scan(scan_args),
        Some(Commands::Rollback(ref rollback_args)) => {
            check_root_privilege()?;
            bootc_migrate_core::migration::rollback::run_rollback(
                rollback_args.reboot,
                rollback_args.dry_run,
            )
        }
        Some(Commands::MigrateBootloader(ref args)) => run_migrate_bootloader(args),
        Some(Commands::BootEntries(ref args)) => run_boot_entries_audit(args),
        Some(Commands::DeMigrate(ref args)) => run_de_migrate(args),
        Some(Commands::Rebase(ref rebase_args)) => {
            if rebase_args.target_image.is_empty() {
                bail!("--target-image (-t) is required for re-base.");
            }
            execute_rebase(rebase_args)
        }
        None => {
            if cli.rebase_args.target_image.is_empty() {
                bail!(
                    "--target-image (-t) is required for re-base. Run `bootc-rebase --help` or `bootc-rebase scan <image>`."
                );
            }
            execute_rebase(&cli.rebase_args)
        }
    }
}

/// Reject target images whose characters would corrupt the `.origin` ini.
fn validate_target_image(target_image: &str) -> Result<()> {
    if target_image.contains('\n') || target_image.contains('\r') || target_image.contains('\0') {
        bail!("--target-image contains invalid characters (newlines, nulls).");
    }
    Ok(())
}

/// Stage `target_image` with `bootc switch` and verify via `bootc status
/// --json` that the staged deployment is exactly the requested image. Shared
/// by the OstreeDeploy and ImageSwap strategies — on both backends, `bootc
/// switch` performs the native staging (3-way /etc merge, shared /var) and
/// leaves the previous deployment as the rollback entry.
fn stage_via_bootc_switch(target_image: &str) -> Result<()> {
    println!("Staging deployment of {target_image} via `bootc switch`...");
    let status = std::process::Command::new("bootc")
        .args(["switch", target_image])
        .status()
        .map_err(|e| anyhow::anyhow!("failed to execute bootc switch: {e}"))?;
    if !status.success() {
        bail!("bootc switch {target_image} failed (exit {status})");
    }

    let out = std::process::Command::new("bootc")
        .args(["status", "--json"])
        .output()
        .map_err(|e| anyhow::anyhow!("failed to execute bootc status: {e}"))?;
    if !out.status.success() {
        bail!(
            "bootc status failed after switch: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let json: serde_json::Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| anyhow::anyhow!("parsing bootc status json: {e}"))?;
    match staged_image_from_status(&json) {
        Some(img) if staged_image_matches(target_image, img) => {
            println!("Staged deployment verified: {img}");
            Ok(())
        }
        Some(img) => {
            bail!("bootc switch staged '{img}' but the requested target was '{target_image}'")
        }
        None => bail!("no staged deployment found after bootc switch"),
    }
}

/// Scenario A' (issue #66): swap the image on a composefs-backed system —
/// no backend conversion. `bootc switch` stages the target natively; this
/// route is gating + switch + verification. The degenerate direct-store path
/// (for targets whose bootc cannot switch) is out of scope until the #13
/// store-selection work lands.
fn run_image_swap(args: &Args) -> Result<()> {
    check_root_privilege()?;
    validate_target_image(&args.target_image)?;

    if args.dry_run {
        println!("*** DRY RUN MODE — no changes will be made ***");
    }
    println!("Checking system state...");

    // The booted deployment must actually be composefs-backed: the router
    // may have been told --source-backend composefs explicitly, but staging
    // relies on the running bootc's composefs support.
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    if !cmdline.contains("composefs=") && !args.force {
        bail!(
            "System is not booted from a composefs deployment (/proc/cmdline has no \
             composefs= parameter). Use --force to override, or re-run with \
             --source-backend auto."
        );
    }

    if args.dry_run {
        println!("[DRY RUN] Would run: bootc switch {}", args.target_image);
        return Ok(());
    }

    let _sleep_guard = Some(bootc_migrate_core::migration::SleepGuard::new(
        "bootc image swap in progress",
    ));

    stage_via_bootc_switch(&args.target_image)?;

    println!(
        "Image swap staged. Reboot to enter the new deployment; the previous \
         deployment remains in the boot menu as rollback."
    );
    Ok(())
}

/// The staged deployment's image spec from `bootc status --json`, if any.
/// (Schema: `.status.staged.image.image.image` — ImageStatus → ImageReference
/// → image spec string; stable across bootc 1.x.)
fn staged_image_from_status(status: &serde_json::Value) -> Option<&str> {
    status
        .pointer("/status/staged/image/image/image")
        .and_then(|v| v.as_str())
}

/// Whether the image bootc reports as staged is the one the user asked for.
///
/// Compares by equality after stripping the transport prefix from both sides
/// (`docker://`, `ostree-unverified-registry:`, …) — bootc's status output
/// omits the transport the user may have typed. Deliberately NOT a substring
/// match: `bluefin:gts-testing` must not "verify" a request for
/// `bluefin:gts`.
fn staged_image_matches(requested: &str, staged: &str) -> bool {
    fn strip_transport(image: &str) -> &str {
        // `scheme://rest` transports first, then the `prefix:name` transports
        // whose remainder still contains a registry path (so a plain
        // `registry/image:tag` — whose only ':' precedes the tag — survives).
        if let Some((_, rest)) = image.split_once("://") {
            return rest;
        }
        for prefix in [
            "ostree-unverified-registry:",
            "ostree-image-signed:",
            "ostree-remote-registry:",
            "containers-storage:",
            "registry:",
        ] {
            if let Some(rest) = image.strip_prefix(prefix) {
                return rest;
            }
        }
        image
    }
    strip_transport(requested) == strip_transport(staged)
}

/// Build the cross-base UID/GID remap plan for `target_image` (issue #67
/// part 1), by comparing this host's base identity against the target
/// image's. `Ok(None)` means "not cross-base" (or identity couldn't be
/// established on either side, which is treated the same way — nothing to
/// gate on unknown information). A registry probe this early in a
/// freshly-booted system can race the guest's own network coming up (seen
/// in E2E: `bootc switch`'s own pull moments later succeeds against the
/// same registry), so a handful of retries absorb that before falling back
/// to the same "can't establish identity, don't gate" degradation used for
/// a target with no parseable os-release — printing a warning either way so
/// the degradation isn't silent.
/// Scan `target_image`'s capabilities, retrying on transient registry
/// failures — early-boot E2E runs have raced the guest's own network coming
/// up (`bootc switch`'s own pull moments later succeeds against the same
/// registry). Returns `None` (with a printed warning) rather than an error
/// so callers can degrade to "unknown information, don't gate on it."
fn scan_target_capabilities_with_retries(
    target_image: &str,
    purpose: &str,
) -> Option<scan::Capabilities> {
    const SCAN_ATTEMPTS: u32 = 3;
    const SCAN_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);
    let mut last_err = None;
    for attempt in 1..=SCAN_ATTEMPTS {
        match scan::scan_target_image(target_image) {
            Ok(c) => return Some(c),
            Err(e) => {
                if attempt < SCAN_ATTEMPTS {
                    std::thread::sleep(SCAN_RETRY_DELAY);
                }
                last_err = Some(e);
            }
        }
    }
    eprintln!(
        "Warning: could not scan target image for {purpose} after {SCAN_ATTEMPTS} attempt(s) \
         ({}); proceeding without it.",
        last_err.expect("None is returned only after at least one failed attempt")
    );
    None
}

fn build_cross_base_plan(target_image: &str) -> Result<Option<remap::RemapPlan>> {
    let Some(host_base) = scan::read_host_base_info() else {
        return Ok(None);
    };

    let Some(caps) = scan_target_capabilities_with_retries(target_image, "cross-base identity")
    else {
        return Ok(None);
    };
    let Some(target_base) = caps.base else {
        return Ok(None);
    };
    if !scan::is_cross_base(&host_base, &target_base) {
        return Ok(None);
    }

    let source_passwd =
        remap::parse_passwd(&std::fs::read_to_string("/etc/passwd").unwrap_or_default());
    let source_group =
        remap::parse_group(&std::fs::read_to_string("/etc/group").unwrap_or_default());

    let scratch = tempfile::Builder::new()
        .prefix("bootc-rebase-remap-")
        .tempdir_in("/var/tmp")
        .context("failed to create scratch dir for target identity DBs")?;
    let target_passwd_path = scratch.path().join("passwd");
    let target_group_path = scratch.path().join("group");
    registry::extract_files_via_registry(
        target_image,
        &[
            (Path::new("etc/passwd"), target_passwd_path.as_path()),
            (Path::new("etc/group"), target_group_path.as_path()),
        ],
    )
    .context("failed to fetch target identity DBs over the registry")?;
    let target_passwd =
        remap::parse_passwd(&std::fs::read_to_string(&target_passwd_path).unwrap_or_default());
    let target_group =
        remap::parse_group(&std::fs::read_to_string(&target_group_path).unwrap_or_default());

    Ok(Some(remap::plan_remap(
        &source_passwd,
        &source_group,
        &target_passwd,
        &target_group,
    )))
}

/// Print the remap report and, unless `accept_cross_base` (or `force`) was
/// passed, refuse with the blast radius already visible. Returns the plan
/// so the caller can apply it after staging succeeds — `None` when this
/// re-base isn't cross-base at all.
fn gate_cross_base(
    target_image: &str,
    accept_cross_base: bool,
    force: bool,
) -> Result<Option<remap::RemapPlan>> {
    let Some(plan) = build_cross_base_plan(target_image)? else {
        return Ok(None);
    };
    println!("{}", remap::render_report(&plan));
    if !accept_cross_base && !force {
        bail!(
            "Cross-base re-base detected (host and target disagree on ID/ID_LIKE). \
             Re-run with --accept-cross-base to proceed with the remap above."
        );
    }
    Ok(Some(plan))
}

/// #80: print (read-only, before staging) any system accounts the target
/// image's `sysusers.d` declares that this host's live identity DB lacks —
/// `bootc switch`'s native `/etc` merge has no key-level reconciliation for
/// them (see `remap::missing_target_sysusers`'s doc for the confirmed
/// mechanism). Advisory only; never refuses the re-base. Degrades silently
/// (via `scan_target_capabilities_with_retries`'s own warning) when the
/// target can't be scanned — this is a courtesy heads-up, not a gate.
fn warn_identity_merge_gap(target_image: &str) {
    let Some(caps) =
        scan_target_capabilities_with_retries(target_image, "identity-DB compatibility")
    else {
        return;
    };
    let host_passwd =
        remap::parse_passwd(&std::fs::read_to_string("/etc/passwd").unwrap_or_default());
    let host_group = remap::parse_group(&std::fs::read_to_string("/etc/group").unwrap_or_default());
    let missing = remap::missing_target_sysusers(&caps.sysusers, &host_passwd, &host_group);
    if missing.is_empty() {
        return;
    }
    println!(
        "Note (#80): the target image expects system account(s) not present in this \
         host's live /etc/passwd or /etc/group: {}. `bootc switch`'s native /etc merge \
         does not reconcile identity databases (unlike this tool's own composefs-conversion \
         merge) — if a locally-modified /etc/passwd is kept verbatim across the switch, \
         these accounts won't be added, and any service depending on them (e.g. dbus \
         needing a `messagebus` user) may fail to start after reboot. This is informational; \
         the re-base is not blocked.",
        missing.join(", ")
    );
}

/// The staged deployment's root directory under `/ostree/deploy/<stateroot>`,
/// found via `ostree admin status`: exactly two deployments exist right
/// after `bootc switch` stages a target (booted + staged), and the booted
/// one is marked with a leading `*` — so the other line is unambiguously the
/// staged deployment. Mirrors the parsing tests/run-e2e.sh's ostree-rebase
/// cell already relies on for its own post-merge fixture injection.
fn staged_deployment_root() -> Result<PathBuf> {
    let out = std::process::Command::new("ostree")
        .args(["admin", "status"])
        .output()
        .map_err(|e| anyhow::anyhow!("failed to execute ostree admin status: {e}"))?;
    if !out.status.success() {
        bail!(
            "ostree admin status failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    parse_staged_deployment_root(&String::from_utf8_lossy(&out.stdout))
}

/// Testable core of [`staged_deployment_root`]: find the non-booted
/// deployment line in `ostree admin status` output and build its path.
fn parse_staged_deployment_root(admin_status_stdout: &str) -> Result<PathBuf> {
    let deploy_line = admin_status_stdout
        .lines()
        .find(|l| !l.trim_start().starts_with('*') && !l.trim().is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("no staged (non-booted) deployment found in ostree admin status")
        })?;
    let mut fields = deploy_line.split_whitespace();
    let stateroot = fields
        .next()
        .ok_or_else(|| anyhow::anyhow!("malformed ostree admin status line: {deploy_line}"))?;
    let checksum_serial = fields
        .next()
        .ok_or_else(|| anyhow::anyhow!("malformed ostree admin status line: {deploy_line}"))?;
    Ok(PathBuf::from("/ostree/deploy")
        .join(stateroot)
        .join("deploy")
        .join(checksum_serial))
}

/// Apply the cross-base remap plan (chown /var + preserved /etc in the
/// staged deployment to the target's ids) after `bootc switch` has staged
/// it. No-op when `plan` is empty (same-base re-base, or no accounts
/// diverged even though the bases differ).
fn apply_cross_base_remap(plan: &remap::RemapPlan) -> Result<()> {
    if plan.is_empty() {
        return Ok(());
    }
    let staged_root = staged_deployment_root()
        .context("failed to locate staged deployment for cross-base remap")?;
    let changed = remap::apply_remap_plan(&staged_root, plan)
        .context("failed to apply cross-base UID/GID remap")?;
    println!(
        "Cross-base remap applied: {changed} file(s)/dir(s) rechowned under {}",
        staged_root.display()
    );
    Ok(())
}

/// Scenario A (issue #30): re-base to another image as a plain OSTree
/// deployment. `bootc switch` already does the heavy lifting on an
/// OSTree-backed system — staging the target with OSTree's native 3-way /etc
/// merge and shared /var — so this route is preflight + gating + `bootc
/// switch` + verification. The previous deployment stays as the rollback
/// entry, matching the engine's two-phase contract.
///
/// Bootloader: per the decision on issue #64, this route will migrate to
/// systemd-boot when the system is ready — wired in once #65's audited
/// bootloader entry point lands. Until then the current bootloader is kept.
fn run_ostree_deploy(args: &Args) -> Result<()> {
    check_root_privilege()?;
    validate_target_image(&args.target_image)?;

    if args.dry_run {
        println!("*** DRY RUN MODE — no changes will be made ***");
    }
    println!("Checking system state...");
    let report = preflight::run_preflight_checks()?;

    if !report.is_bootc_ostree && !args.force {
        bail!("System is not booted into an OSTree deployment. Cannot perform an ostree re-base.");
    }
    if report.pending_transaction != preflight::PendingTransactionStatus::Clean
        && !args.force
        && !args.skip_preflight
    {
        bail!(
            "Pending OSTree transaction detected: {}. Complete or undeploy it first \
             (see `ostree admin status`).",
            report.pending_transaction
        );
    }
    if report.esp_ready_for_systemd_boot && report.nvram_writable {
        println!(
            "Note: system is ready for systemd-boot; bootloader migration will be \
             integrated into this route via the migrate-bootloader work (#65). \
             Keeping the current bootloader for this re-base."
        );
    }

    // Cross-base gate (#67 part 1): always print the remap report before
    // anything is staged, and refuse without --accept-cross-base so the
    // blast radius is visible first — including in --dry-run.
    let cross_base_plan = gate_cross_base(&args.target_image, args.accept_cross_base, args.force)?;

    // #80: advisory identity-DB gap check, independent of cross-base status
    // (the motivating case — Bluefin GNOME → Aurora KDE — is same-base).
    warn_identity_merge_gap(&args.target_image);

    if args.dry_run {
        println!("[DRY RUN] Would run: bootc switch {}", args.target_image);
        return Ok(());
    }

    let _sleep_guard = Some(bootc_migrate_core::migration::SleepGuard::new(
        "bootc ostree re-base in progress",
    ));

    stage_via_bootc_switch(&args.target_image)?;

    if let Some(plan) = &cross_base_plan {
        apply_cross_base_remap(plan)?;
    }

    println!(
        "Re-base staged. Reboot to enter the new deployment; the previous \
         deployment remains in the boot menu as rollback."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staged_match_exact() {
        assert!(staged_image_matches(
            "ghcr.io/projectbluefin/bluefin:gts",
            "ghcr.io/projectbluefin/bluefin:gts"
        ));
    }

    #[test]
    fn staged_match_strips_requested_transport() {
        assert!(staged_image_matches(
            "docker://ghcr.io/projectbluefin/bluefin:gts",
            "ghcr.io/projectbluefin/bluefin:gts"
        ));
        assert!(staged_image_matches(
            "ostree-unverified-registry:ghcr.io/projectbluefin/bluefin:gts",
            "ghcr.io/projectbluefin/bluefin:gts"
        ));
    }

    #[test]
    fn staged_match_rejects_tag_extension() {
        // The old substring check accepted this: gts-testing contains gts.
        assert!(!staged_image_matches(
            "ghcr.io/projectbluefin/bluefin:gts",
            "ghcr.io/projectbluefin/bluefin:gts-testing"
        ));
        assert!(!staged_image_matches(
            "ghcr.io/projectbluefin/bluefin:gts-testing",
            "ghcr.io/projectbluefin/bluefin:gts"
        ));
    }

    #[test]
    fn staged_match_rejects_different_image() {
        assert!(!staged_image_matches(
            "ghcr.io/projectbluefin/bluefin:gts",
            "ghcr.io/projectbluefin/dakota:stable"
        ));
    }

    #[test]
    fn staged_match_plain_tag_colon_survives_transport_strip() {
        // A bare registry/image:tag has a ':' but no transport — it must not
        // get mangled by the prefix stripping.
        assert!(staged_image_matches(
            "quay.io/fedora/fedora-bootc:42",
            "quay.io/fedora/fedora-bootc:42"
        ));
    }

    #[test]
    fn staged_image_extracted_from_status_json() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{"status":{"staged":{"image":{"image":{"image":"ghcr.io/projectbluefin/bluefin:gts","transport":"registry"}}}}}"#,
        )
        .unwrap();
        assert_eq!(
            staged_image_from_status(&json),
            Some("ghcr.io/projectbluefin/bluefin:gts")
        );
    }

    #[test]
    fn staged_image_absent_when_nothing_staged() {
        let json: serde_json::Value =
            serde_json::from_str(r#"{"status":{"staged":null,"booted":{}}}"#).unwrap();
        assert_eq!(staged_image_from_status(&json), None);
    }

    #[test]
    fn staged_deployment_root_picks_non_starred_line() {
        // Real `ostree admin status` output: the booted deployment is
        // prefixed with '*', the staged one is not.
        let status = "* dakota abc123.0\nbluefin def456.1\n";
        let root = parse_staged_deployment_root(status).unwrap();
        assert_eq!(
            root,
            PathBuf::from("/ostree/deploy/bluefin/deploy/def456.1")
        );
    }

    #[test]
    fn staged_deployment_root_errors_when_only_booted_present() {
        let only_booted = "* dakota abc123.0\n";
        assert!(parse_staged_deployment_root(only_booted).is_err());
    }

    #[test]
    fn staged_deployment_root_errors_on_malformed_line() {
        let malformed = "* dakota abc123.0\nonly-one-field\n";
        assert!(parse_staged_deployment_root(malformed).is_err());
    }

    #[test]
    fn accept_cross_base_flag_parses() {
        let cli = Cli::parse_from([
            "bootc-rebase",
            "-t",
            "ghcr.io/tuna-os/centos-bootc:stream10",
            "--accept-cross-base",
        ]);
        assert!(cli.rebase_args.accept_cross_base);

        let cli = Cli::parse_from(["bootc-rebase", "-t", "ghcr.io/projectbluefin/dakota:stable"]);
        assert!(!cli.rebase_args.accept_cross_base);
    }

    #[test]
    fn parse_backend_accepts_known_and_rejects_unknown() {
        assert!(matches!(parse_backend("ostree"), Ok(Backend::Ostree)));
        assert!(matches!(parse_backend("composefs"), Ok(Backend::Composefs)));
        assert!(parse_backend("btrfs").is_err());
        assert!(parse_backend("").is_err());
    }

    #[test]
    fn test_scan_subcommand_parsing() {
        let cli = Cli::parse_from([
            "bootc-rebase",
            "scan",
            "ghcr.io/projectbluefin/dakota:stable",
            "--json",
        ]);
        match cli.command {
            Some(Commands::Scan(args)) => {
                assert_eq!(args.image, "ghcr.io/projectbluefin/dakota:stable");
                assert!(args.json);
            }
            _ => panic!("expected Commands::Scan"),
        }
    }

    #[test]
    fn test_rebase_subcommand_parsing() {
        let cli = Cli::parse_from([
            "bootc-rebase",
            "-t",
            "ghcr.io/projectbluefin/dakota:stable",
            "--plan",
        ]);
        assert!(cli.command.is_none());
        assert_eq!(
            cli.rebase_args.target_image,
            "ghcr.io/projectbluefin/dakota:stable"
        );
        assert!(cli.rebase_args.plan);

        let cli = Cli::parse_from([
            "bootc-rebase",
            "rebase",
            "-t",
            "ghcr.io/projectbluefin/dakota:stable",
        ]);
        match cli.command {
            Some(Commands::Rebase(args)) => {
                assert_eq!(args.target_image, "ghcr.io/projectbluefin/dakota:stable");
            }
            _ => panic!("expected Commands::Rebase"),
        }
    }

    #[test]
    fn test_rollback_subcommand_parsing() {
        let cli = Cli::parse_from(["bootc-rebase", "rollback", "--reboot", "--dry-run"]);
        match cli.command {
            Some(Commands::Rollback(args)) => {
                assert!(args.reboot);
                assert!(args.dry_run);
            }
            _ => panic!("expected Commands::Rollback"),
        }
    }

    #[test]
    fn test_migrate_bootloader_subcommand_parsing() {
        let cli = Cli::parse_from([
            "bootc-rebase",
            "migrate-bootloader",
            "--to",
            "systemd-boot",
            "--dry-run",
            "--undo",
        ]);
        match cli.command {
            Some(Commands::MigrateBootloader(args)) => {
                assert_eq!(args.to, "systemd-boot");
                assert!(args.dry_run);
                assert!(args.undo);
            }
            _ => panic!("expected Commands::MigrateBootloader"),
        }
    }

    #[test]
    fn test_boot_entries_subcommand_parsing() {
        let cli = Cli::parse_from(["bootc-rebase", "boot-entries", "--json"]);
        match cli.command {
            Some(Commands::BootEntries(args)) => {
                assert!(args.json);
            }
            _ => panic!("expected Commands::BootEntries"),
        }
    }

    #[test]
    fn test_de_migrate_stash_subcommand_parsing() {
        let cli = Cli::parse_from([
            "bootc-rebase",
            "de-migrate",
            "stash",
            "--from-de",
            "gnome",
            "--home",
            "/home/user",
            "--dry-run",
        ]);
        match cli.command {
            Some(Commands::DeMigrate(args)) => match args.action {
                DeMigrateAction::Stash {
                    from_de,
                    home,
                    dry_run,
                    run_hooks,
                    ..
                } => {
                    assert_eq!(from_de, "gnome");
                    assert_eq!(home, PathBuf::from("/home/user"));
                    assert!(dry_run);
                    assert!(!run_hooks);
                }
                _ => panic!("expected DeMigrateAction::Stash"),
            },
            _ => panic!("expected Commands::DeMigrate"),
        }
    }

    #[test]
    fn test_de_migrate_restore_subcommand_parsing() {
        let cli = Cli::parse_from([
            "bootc-rebase",
            "de-migrate",
            "restore",
            "--to-de",
            "kde",
            "--home",
            "/home/user",
            "--run-hooks",
        ]);
        match cli.command {
            Some(Commands::DeMigrate(args)) => match args.action {
                DeMigrateAction::Restore {
                    to_de, run_hooks, ..
                } => {
                    assert_eq!(to_de, "kde");
                    assert!(run_hooks);
                }
                _ => panic!("expected DeMigrateAction::Restore"),
            },
            _ => panic!("expected Commands::DeMigrate"),
        }
    }

    #[test]
    fn parse_de_rejects_unknown_names() {
        assert!(parse_de("xfce").is_err());
        assert!(parse_de("GNOME").is_ok());
    }

    #[test]
    fn migrate_bootloader_stub_always_refuses() {
        let args = MigrateBootloaderArgs {
            to: "systemd-boot".into(),
            dry_run: false,
            undo: false,
        };
        let err = run_migrate_bootloader(&args).unwrap_err();
        assert!(err.to_string().contains("not implemented"));
    }
}
