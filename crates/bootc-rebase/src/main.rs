//! `bootc-rebase` — universal bootc re-base engine.
//!
//! Consumes `bootc-migrate-core` to re-base a bootc system between backends,
//! bootloaders, and images. Today the OSTree → ComposeFS route drives the
//! core pipeline directly; the routing table in [`routing`] tracks what else
//! is planned. See issues #30 and #45 in tuna-os/bootc-migrate for
//! the roadmap.

use anyhow::{Result, bail};

use bootc_migrate_core::preflight;
use bootc_migrate_core::rebase_controller::{
    CoreMigrationConfig, ImageSwapConfig, OstreeDeployConfig,
};
use bootc_migrate_core::runlog;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod boot_entries;
mod boot_entry_review;
mod de_migrate_command;
mod scan_command;

use bootc_migrate_core::rebase_plan::{Backend, Strategy, bootloader_plan, plan, route};

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
    /// Audit and clean up UEFI boot entries (issue #31). A bare invocation
    /// is read-only: it reports which entries look
    /// dead/generic/duplicate/firmware-managed and which are protected.
    /// `--interactive` opens a checklist, `--rename-branding` proposes
    /// PRETTY_NAME renames, `--apply` writes the result to NVRAM after
    /// taking a restorable snapshot, and `--undo` restores from it.
    BootEntries(BootEntriesArgs),
    /// Stash or restore a user's DE config around a cross-DE re-base (issue
    /// #68), for one explicitly-named DE and one explicitly-named home. The
    /// `rebase` flow does this automatically for every human account when
    /// `--de-migrate` is passed and the target image ships a different DE;
    /// this subcommand is the manual escape hatch for the cases detection
    /// deliberately refuses to guess at (an image shipping several desktops,
    /// or one this tool does not recognize).
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
    /// Output as machine-readable JSON instead of a table. On its own this
    /// is the audit array; combined with a cleanup flag it becomes an
    /// object with `audit` and `plan` members.
    #[arg(long)]
    json: bool,

    /// Choose entries to remove (and rename) in an interactive checklist.
    /// Firmware-managed entries, the entry this system booted from, and the
    /// rollback path are shown but cannot be selected.
    #[arg(long)]
    interactive: bool,

    /// Offer to rename generic-labelled entries ("Linux", "UEFI OS", ...)
    /// to this system's PRETTY_NAME from /etc/os-release. A rename is a
    /// delete+recreate against NVRAM, so it needs --apply like any other
    /// change.
    #[arg(long)]
    rename_branding: bool,

    /// Actually write the planned changes to UEFI NVRAM. Without this every
    /// other flag only previews.
    #[arg(long)]
    apply: bool,

    /// Skip the typed confirmation that --apply otherwise requires.
    #[arg(long)]
    yes: bool,

    /// Restore UEFI NVRAM from the snapshot taken before a previous --apply.
    #[arg(long)]
    undo: bool,

    /// Snapshot file for --undo (default: the newest one in --backup-dir).
    #[arg(long)]
    snapshot: Option<PathBuf>,

    /// Where pre-change NVRAM snapshots are written and looked for.
    #[arg(long, default_value = bootc_migrate_core::boot_cleanup::live::BACKUP_DIR)]
    backup_dir: PathBuf,
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

    /// Print the planned route as stable JSON for frontends and orchestration.
    /// Implies --plan and never inspects or mutates the live system.
    #[arg(long)]
    plan_json: bool,

    /// Acknowledge a cross-base re-base (host and target disagree on
    /// ID/ID_LIKE) and proceed with its UID/GID remap (#67). Without this,
    /// a detected cross-base re-base is refused after printing the remap
    /// report so the blast radius is visible first.
    #[arg(long)]
    accept_cross_base: bool,

    /// When the target image ships a different desktop environment than this
    /// host, move each human account's outgoing DE config into a stash and
    /// re-expose any stash left by a previous re-base in the other direction
    /// (#68). Off by default — a re-base never touches per-user desktop
    /// state unless asked to.
    #[arg(long)]
    de_migrate: bool,
}

/// Stub (issue #65): the pure BLS-entry/karg-carry-over/entry-token core
/// lives in `bootc_migrate_core::migration::bootloader::systemd_boot` and is
/// unit-tested, but the live ESP populate + NVRAM cutover + kernel-install
/// resync hook aren't implemented — see the doc comment on
/// `Commands::MigrateBootloader`. Refuses unconditionally so this can't be
/// mistaken for a working migration.
fn run_migrate_bootloader(_args: &MigrateBootloaderArgs) -> Result<()> {
    let plan = bootloader_plan();
    println!("Phases: {}", plan.phase_names());
    println!("Bootloader policy: {:?}", plan.bootloader);
    bail!(
        "migrate-bootloader is not implemented yet (issue #65): the ESP/NVRAM mutation and \
         kernel-install resync hook don't exist. See \
         https://github.com/tuna-os/bootc-migrate/issues/65"
    );
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
    CoreMigrationConfig {
        target_image: &args.target_image,
        bootloader: &args.bootloader,
        dry_run: args.dry_run,
        skip_import: args.skip_import,
        skip_preflight: args.skip_preflight,
        force: args.force,
        de_migrate: args.de_migrate,
    }
    .run()
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
    let phase_plan = plan(from, to).expect("every route has a phase plan");

    if args.plan_json {
        let phases: Vec<String> = phase_plan.phases.iter().map(ToString::to_string).collect();
        let value = serde_json::json!({
            "from": from.to_string(),
            "to": to.to_string(),
            "strategy": format!("{:?}", r.strategy),
            "implemented": r.implemented,
            "phases": phases,
            "bootloader": format!("{:?}", phase_plan.bootloader),
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }
    println!(
        "Route: {from} -> {to} via {:?} ({})",
        r.strategy,
        if r.implemented {
            "implemented"
        } else {
            "planned, not yet implemented"
        }
    );
    println!("Phases: {}", phase_plan.phase_names());
    println!("Bootloader policy: {:?}", phase_plan.bootloader);

    if args.plan || args.plan_json {
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

fn main() {
    // Parsed before the tee is installed: clap exits the process from inside
    // `parse()` for `--help`/`--version`/usage errors, which would strand
    // that output in the pipe with no guard left to drain it.
    let cli = Cli::parse();

    // Same rationale as `bootc-migrate`: every mutating subcommand here
    // rewrites the boot path and then asks for a reboot, so the terminal that
    // carried the output is usually gone before anyone reads it. Gated on
    // root because that is exactly the set of runs that can mutate anything —
    // an unprivileged `scan` would otherwise warn about /var/log on every
    // invocation and has nothing worth recording.
    let guard = if rustix::process::getuid().is_root() {
        runlog::start(
            "/var/log/bootc-rebase.log",
            "bootc-rebase",
            env!("CARGO_PKG_VERSION"),
        )
    } else {
        None
    };

    let result = run(cli);

    // Drain the tee thread before exiting: process::exit skips destructors,
    // and a plain `return` from main races the thread, either of which drops
    // the tail of the output — including the error we are reporting.
    let code = match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("Error: {e:#}");
            1
        }
    };
    if let Some(g) = guard {
        g.finish();
    }
    std::process::exit(code);
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Some(Commands::Scan(ref scan_args)) => scan_command::run(scan_args),
        Some(Commands::Rollback(ref rollback_args)) => {
            check_root_privilege()?;
            bootc_migrate_core::migration::rollback::run_rollback(
                rollback_args.reboot,
                rollback_args.dry_run,
            )
        }
        Some(Commands::MigrateBootloader(ref args)) => run_migrate_bootloader(args),
        Some(Commands::BootEntries(ref args)) => boot_entries::run_boot_entries(args),
        Some(Commands::DeMigrate(ref args)) => de_migrate_command::run(args),
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

/// Scenario A' (issue #66): swap the image on a composefs-backed system.
/// The route itself lives in the core rebase controller; this only checks
/// privilege and translates flags.
fn run_image_swap(args: &Args) -> Result<()> {
    check_root_privilege()?;
    ImageSwapConfig {
        target_image: &args.target_image,
        dry_run: args.dry_run,
        force: args.force,
        de_migrate: args.de_migrate,
    }
    .run()
}

/// Scenario A (issue #30): re-base to another image as a plain OSTree
/// deployment. The route itself lives in the core rebase controller; this
/// only checks privilege and translates flags.
fn run_ostree_deploy(args: &Args) -> Result<()> {
    check_root_privilege()?;
    OstreeDeployConfig {
        target_image: &args.target_image,
        dry_run: args.dry_run,
        force: args.force,
        skip_preflight: args.skip_preflight,
        accept_cross_base: args.accept_cross_base,
        de_migrate: args.de_migrate,
    }
    .run()
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn de_migrate_flag_is_off_by_default_on_every_rebase_entry_point() {
        // Bare invocation, `rebase` subcommand, and the flag itself — #68
        // requires the DE step to be opt-in on all of them.
        let bare = Cli::parse_from(["bootc-rebase", "-t", "ghcr.io/projectbluefin/dakota:stable"]);
        assert!(!bare.rebase_args.de_migrate);

        let sub = Cli::parse_from([
            "bootc-rebase",
            "rebase",
            "-t",
            "ghcr.io/projectbluefin/dakota:stable",
        ]);
        match sub.command {
            Some(Commands::Rebase(args)) => assert!(!args.de_migrate),
            other => panic!("expected Commands::Rebase, got {other:?}"),
        }

        let opted_in = Cli::parse_from([
            "bootc-rebase",
            "-t",
            "ghcr.io/projectbluefin/dakota:stable",
            "--de-migrate",
        ]);
        assert!(opted_in.rebase_args.de_migrate);
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
            "-t",
            "ghcr.io/projectbluefin/dakota:stable",
            "--plan-json",
        ]);
        assert!(cli.rebase_args.plan_json);

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

    fn boot_entries_args(argv: &[&str]) -> BootEntriesArgs {
        match Cli::parse_from(argv).command {
            Some(Commands::BootEntries(args)) => args,
            _ => panic!("expected Commands::BootEntries for {argv:?}"),
        }
    }

    #[test]
    fn boot_entries_is_dry_run_and_read_only_by_default() {
        // The whole safety contract of #31's cleanup rests on a bare
        // invocation changing nothing.
        let args = boot_entries_args(&["bootc-rebase", "boot-entries"]);
        assert!(!args.apply, "--apply must not default to on");
        assert!(!args.yes, "--yes must not default to on");
        assert!(!args.interactive);
        assert!(!args.rename_branding);
        assert!(!args.undo);
        assert!(args.snapshot.is_none());
        assert_eq!(
            args.backup_dir,
            PathBuf::from(bootc_migrate_core::boot_cleanup::live::BACKUP_DIR)
        );
    }

    #[test]
    fn boot_entries_cleanup_flags_parse() {
        let args = boot_entries_args(&[
            "bootc-rebase",
            "boot-entries",
            "--interactive",
            "--rename-branding",
            "--apply",
            "--yes",
            "--backup-dir",
            "/tmp/snaps",
        ]);
        assert!(args.interactive);
        assert!(args.rename_branding);
        assert!(args.apply);
        assert!(args.yes);
        assert_eq!(args.backup_dir, PathBuf::from("/tmp/snaps"));

        let undo = boot_entries_args(&[
            "bootc-rebase",
            "boot-entries",
            "--undo",
            "--snapshot",
            "/var/lib/x/nvram-1.json",
        ]);
        assert!(undo.undo);
        assert_eq!(
            undo.snapshot,
            Some(PathBuf::from("/var/lib/x/nvram-1.json"))
        );
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
