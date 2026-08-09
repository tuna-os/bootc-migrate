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
use bootc_migrate_core::{etc_conflict, registry, remap, scan};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

mod boot_entry_review;
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
    /// GRUB2 -> systemd-boot bootloader migration (issue #65). Stages a
    /// one-boot `BootNext` trial (BootOrder untouched, so a failed trial
    /// falls back to the existing default) and installs a kernel-install
    /// resync hook so future kernel updates keep the ESP entry current.
    /// `--promote` moves the trial to the front of BootOrder after a human
    /// confirms it booted correctly; `--undo` reverses everything.
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
    /// Target bootloader (only "systemd-boot" is implemented)
    #[arg(long, default_value = "systemd-boot")]
    to: String,

    /// Dry-run: print every action without executing
    #[arg(long)]
    dry_run: bool,

    /// Undo a previous migrate-bootloader run: removes the ESP entries,
    /// resync hook, and NVRAM entry it created. GRUB is never touched by
    /// this route, so nothing GRUB-side needs restoring.
    #[arg(long)]
    undo: bool,

    /// Promote the one-boot BootNext trial to the front of BootOrder,
    /// making it the permanent default. Run this only after confirming the
    /// trial boot actually worked.
    #[arg(long)]
    promote: bool,

    /// Internal: invoked by the installed kernel-install resync hook on
    /// every `kernel-install add`, not meant for interactive use. Re-copies
    /// the given kernel/initrd to the ESP and rewrites the BLS entry
    /// without touching NVRAM.
    #[arg(long, hide = true)]
    resync: bool,

    /// Entry token to resync under (only meaningful with --resync).
    #[arg(long)]
    entry_token: Option<String>,

    /// Kernel version to resync (only meaningful with --resync).
    #[arg(long)]
    kernel_version: Option<String>,

    /// BLS entry title to resync with (only meaningful with --resync).
    #[arg(long)]
    title: Option<String>,

    /// OCI image to fetch systemd-bootx64.efi from when the current
    /// deployment doesn't ship it locally — the common case in practice
    /// (confirmed empirically: Bluefin stable's GRUB deployment doesn't).
    /// Any image known to ship systemd-boot works, e.g. a composefs
    /// sibling target image.
    #[arg(long)]
    from_image: Option<String>,
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

    /// When the target image ships a different desktop environment than this
    /// host, move each human account's outgoing DE config into a stash and
    /// re-expose any stash left by a previous re-base in the other direction
    /// (#68). Off by default — a re-base never touches per-user desktop
    /// state unless asked to.
    #[arg(long)]
    de_migrate: bool,
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

/// GRUB2 → systemd-boot conversion (issue #65). Converts the *current*
/// deployment's own bootloader — there's no target image involved, unlike
/// `rebase`. Sequencing: `migrate-bootloader` stages a one-boot `BootNext`
/// trial (BootOrder untouched, so a failed trial falls back to the
/// existing default); `--promote` moves it to the front of BootOrder after
/// a human confirms the trial boot worked; `--undo` reverses everything.
/// `--resync` is the internal entry point the installed kernel-install
/// hook calls on every kernel update.
fn run_migrate_bootloader(args: &MigrateBootloaderArgs) -> Result<()> {
    use bootc_migrate_core::migration::bootloader::live;

    if args.to != "systemd-boot" {
        bail!("migrate-bootloader only supports --to systemd-boot");
    }

    let state_path = Path::new(live::STATE_PATH);

    if args.resync {
        let entry_token = args
            .entry_token
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("--resync requires --entry-token"))?;
        let kver = args
            .kernel_version
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("--resync requires --kernel-version"))?;
        let title = args
            .title
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("--resync requires --title"))?;
        check_root_privilege()?;
        let esp_path =
            migration::boot::find_esp_or_mount().context("failed to locate the ESP for resync")?;
        let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
        live::resync_for_kernel_update(
            Path::new(&esp_path),
            entry_token,
            kver,
            cmdline.trim(),
            title,
        )
        .context("resyncing ESP for kernel update")?;
        println!("Resynced ESP for kernel {kver}.");
        return Ok(());
    }

    if args.undo {
        check_root_privilege()?;
        live::run_undo(state_path)?;
        return Ok(());
    }

    if args.promote {
        check_root_privilege()?;
        live::run_promote(state_path)?;
        return Ok(());
    }

    check_root_privilege()?;

    let esp_path = migration::boot::find_esp_or_mount()
        .context("failed to locate the ESP for migrate-bootloader")?;
    let os_release = bootc_migrate_core::migration::os_release::read_os_release(Path::new("/"))
        .context("failed to read /etc/os-release")?;
    let title = if os_release.pretty_name.is_empty() {
        os_release.name.clone()
    } else {
        os_release.pretty_name.clone()
    };
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    let machine_id = std::fs::read_to_string("/etc/machine-id").unwrap_or_default();
    let entry_token_file = std::fs::read_to_string("/etc/kernel/entry-token").ok();

    if args.dry_run {
        println!("*** DRY RUN MODE — no changes will be made ***");
        println!(
            "Would populate the ESP at {esp_path} with a systemd-boot entry titled {title:?}, \
             install the kernel-install resync hook, and create a one-boot BootNext trial \
             NVRAM entry. Nothing has been touched."
        );
        return Ok(());
    }

    let inputs = live::MigrateInputs {
        esp_path: Path::new(&esp_path),
        state_path,
        title: &title,
        cmdline: cmdline.trim(),
        entry_token_file: entry_token_file.as_deref(),
        machine_id: machine_id.trim(),
        from_image: args.from_image.as_deref(),
    };
    live::run_migrate(&inputs)?;
    Ok(())
}

/// The only answer accepted at the destructive-apply prompt. A full word,
/// not `y`, so a stray keypress cannot authorize an NVRAM mutation.
const APPLY_CONFIRMATION: &str = "yes";

/// Whether what the user typed at the apply prompt authorizes the change.
fn confirmation_accepted(input: &str) -> bool {
    input.trim().eq_ignore_ascii_case(APPLY_CONFIRMATION)
}

/// Ask for the typed confirmation on stdin.
fn prompt_for_apply_confirmation() -> Result<bool> {
    use std::io::Write;
    print!("Type '{APPLY_CONFIRMATION}' to apply these UEFI NVRAM changes: ");
    std::io::stdout().flush().context("flushing prompt")?;
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .context("reading confirmation from stdin")?;
    Ok(confirmation_accepted(&answer))
}

fn flag_word(flag: bootc_migrate_core::boot_audit::AuditFlag) -> &'static str {
    use bootc_migrate_core::boot_audit::AuditFlag;
    match flag {
        AuditFlag::Dead => "DEAD",
        AuditFlag::GenericLabel => "generic-label",
        AuditFlag::DuplicateLoaderPath => "duplicate",
        AuditFlag::FirmwareManaged => "firmware",
    }
}

fn print_audit_table(
    audited: &[bootc_migrate_core::boot_audit::AuditedEntry],
    facts: &bootc_migrate_core::boot_cleanup::plan::NvramFacts,
) {
    use bootc_migrate_core::boot_cleanup::plan::delete_protection;

    println!("=== UEFI boot-entry audit ({} entries) ===", audited.len());
    for a in audited {
        let marker = if a.entry.active { "*" } else { " " };
        let flag_str = if a.flags.is_empty() {
            "ok".to_string()
        } else {
            a.flags
                .iter()
                .map(|f| flag_word(*f))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let protection = match delete_protection(a, facts) {
            Some(p) => format!("  protected: {}", p.describe()),
            None => String::new(),
        };
        println!(
            "  Boot{}{} {:<28} [{}]{}",
            a.entry.id, marker, a.entry.label, flag_str, protection
        );
    }
}

fn print_plan(plan: &bootc_migrate_core::boot_cleanup::plan::CleanupPlan) {
    use bootc_migrate_core::boot_cleanup::plan::PlannedOp;

    println!("\n=== Planned UEFI NVRAM changes ===");
    if plan.is_empty() {
        println!("  (nothing selected)");
        return;
    }
    for op in &plan.ops {
        match op {
            PlannedOp::Delete { id, label, flags } => {
                let why = flags
                    .iter()
                    .map(|f| flag_word(*f))
                    .collect::<Vec<_>>()
                    .join(", ");
                println!("  DELETE Boot{id}  {label:<28} [{why}]");
            }
            PlannedOp::Rename {
                id,
                from_label,
                to_label,
                loader_path,
            } => {
                println!(
                    "  RENAME Boot{id}  {from_label:?} -> {to_label:?}  \
                     (delete + recreate against {loader_path})"
                );
            }
        }
    }
    println!(
        "\n{} deletion(s), {} rename(s).",
        plan.delete_count(),
        plan.rename_count()
    );
}

/// This system's `PRETTY_NAME`, for branding renames. A missing or
/// unreadable `/etc/os-release` degrades to "no renames offered" with the
/// reason printed, rather than failing the whole command — the deletion
/// half is still useful without it.
fn host_pretty_name() -> Option<String> {
    match migration::os_release::read_os_release(Path::new("/")) {
        Ok(os) if !os.pretty_name.trim().is_empty() => Some(os.pretty_name),
        Ok(_) => {
            eprintln!(
                "Note: /etc/os-release has no PRETTY_NAME, so no branding rename can be proposed."
            );
            None
        }
        Err(e) => {
            eprintln!("Note: could not read /etc/os-release ({e:#}); no branding rename proposed.");
            None
        }
    }
}

/// UEFI boot-entry audit and cleanup (issue #31).
///
/// **Dry-run by default**: without `--apply` this enumerates, classifies,
/// and prints exactly what it *would* change, and exits having touched
/// nothing. `--apply` additionally writes a full NVRAM snapshot before the
/// first mutation and asks for a typed confirmation; `--undo` restores from
/// that snapshot.
fn run_boot_entries(args: &BootEntriesArgs) -> Result<()> {
    use bootc_migrate_core::boot_audit;
    use bootc_migrate_core::boot_cleanup::live;
    use bootc_migrate_core::boot_cleanup::plan::{
        self, CleanupSelection, NvramFacts, branding_renames, default_delete_selection,
    };
    use bootc_migrate_core::migration::rollback;

    if args.undo && (args.apply || args.interactive || args.rename_branding) {
        bail!(
            "--undo restores a previous run and cannot be combined with --apply/--interactive/--rename-branding"
        );
    }
    if args.json && args.interactive {
        bail!("--json and --interactive are mutually exclusive: the checklist needs the terminal");
    }
    if args.apply && args.json && !args.yes {
        bail!(
            "--apply with --json cannot prompt for confirmation; pass --yes to state the intent explicitly"
        );
    }

    if args.undo {
        // A restore that only puts BootOrder back needs no ESP at all, so
        // a system whose ESP can't be located must still get that far
        // rather than being refused outright.
        let esp_root = migration::boot::find_esp_or_mount().unwrap_or_else(|e| {
            eprintln!(
                "Note: could not locate the ESP ({e:#}). Restoring BootOrder will still work; \
                 recreating a deleted entry will fail with the reason."
            );
            migration::boot::DEFAULT_ESP_MOUNT.to_string()
        });
        return run_boot_entries_undo(args, Path::new(&esp_root));
    }

    let nvram = boot_audit::read_efibootmgr_verbose()?;
    let esp_root = migration::boot::find_esp_or_mount()
        .context("failed to locate the ESP for the boot-entry audit")?;
    let esp_root = PathBuf::from(esp_root);
    let entries = boot_audit::parse_efibootmgr_entries(&nvram);
    let audited = boot_audit::audit_entries(&entries, &esp_root);
    let facts = NvramFacts {
        boot_current: rollback::parse_boot_current(&nvram),
        boot_order: rollback::parse_boot_order(&nvram),
        rollback_entry_id: rollback::parse_ostree_boot_entry_id(&nvram),
    };

    // Plain `boot-entries` stays exactly the read-only audit it has always
    // been, including its JSON shape.
    let wants_cleanup = args.interactive || args.rename_branding || args.apply;
    if !wants_cleanup {
        if args.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&audited).expect("audited entries always serialize")
            );
            return Ok(());
        }
        print_audit_table(&audited, &facts);
        let preselect_count = audited
            .iter()
            .filter(|a| a.safe_to_preselect() && plan::delete_protection(a, &facts).is_none())
            .count();
        println!(
            "\n{preselect_count} entry(ies) would be pre-selected for removal (clearly dead, not protected)."
        );
        println!(
            "No entries were modified — this is a read-only audit. Add --interactive to choose \
             entries, and --apply to actually change UEFI NVRAM."
        );
        return Ok(());
    }

    let pretty_name = if args.rename_branding {
        host_pretty_name()
    } else {
        None
    };

    let selection = if args.interactive {
        let rows = boot_entry_review::build_rows(&audited, &facts, pretty_name.as_deref());
        match boot_entry_review::run_review(rows)? {
            Some(selection) => selection,
            None => {
                println!("Cancelled — no entries were modified.");
                return Ok(());
            }
        }
    } else {
        if !args.json {
            print_audit_table(&audited, &facts);
        }
        CleanupSelection {
            delete_ids: default_delete_selection(&audited, &facts),
            renames: pretty_name
                .as_deref()
                .map(|n| branding_renames(&audited, &facts, n))
                .unwrap_or_default(),
        }
    };

    let cleanup_plan = plan::plan_cleanup(&audited, &facts, &selection)
        .context("refusing to change UEFI boot entries")?;

    if args.json {
        let doc = serde_json::json!({ "audit": audited, "plan": cleanup_plan });
        println!(
            "{}",
            serde_json::to_string_pretty(&doc).expect("audit and plan always serialize")
        );
    } else {
        print_plan(&cleanup_plan);
    }

    // With --json, stdout carries one JSON document and nothing else;
    // every human-facing line below goes to stderr so the output stays
    // machine-readable (REVIEW.md: design for machine-readable output).
    let note = |line: &str| {
        if args.json {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    };

    if cleanup_plan.is_empty() {
        note("\nNothing to do — no entries were modified.");
        return Ok(());
    }

    if !args.apply {
        note(
            "\nDry run: nothing was changed. Re-run the same command with --apply to write these \
             changes to UEFI NVRAM.",
        );
        return Ok(());
    }

    if !args.yes && !prompt_for_apply_confirmation()? {
        note("Not confirmed — no entries were modified.");
        return Ok(());
    }

    // Backup first, and say where it went, so the state is recoverable by
    // hand even if this process dies mid-run.
    let (snapshot_path, _) = live::write_snapshot(&args.backup_dir, &nvram)
        .context("writing the pre-cleanup NVRAM snapshot")?;
    note(&format!(
        "\nNVRAM snapshot written to {} (restore it with `bootc-rebase boot-entries --undo`).",
        snapshot_path.display()
    ));

    live::apply_plan(&cleanup_plan, &esp_root).context("applying UEFI boot-entry changes")?;
    note("\nDone. Verify with `efibootmgr -v` before rebooting.");
    Ok(())
}

/// Restore NVRAM from a snapshot taken by a previous `--apply` run.
fn run_boot_entries_undo(args: &BootEntriesArgs, esp_root: &Path) -> Result<()> {
    use bootc_migrate_core::boot_cleanup::live;

    let snapshot_path = match &args.snapshot {
        Some(p) => p.clone(),
        None => live::latest_snapshot(&args.backup_dir)
            .with_context(|| {
                format!(
                    "looking for NVRAM snapshots in {}",
                    args.backup_dir.display()
                )
            })?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no NVRAM snapshot found in {} — there is nothing to undo (pass --snapshot \
                     <file> if you kept one elsewhere)",
                    args.backup_dir.display()
                )
            })?,
    };

    let snapshot = live::read_snapshot(&snapshot_path)?;
    println!(
        "Restoring UEFI boot entries from {} ({} entries recorded).",
        snapshot_path.display(),
        snapshot.entries.len()
    );

    let restored = live::run_undo(&snapshot, esp_root).context("restoring UEFI boot entries")?;
    let recreated = restored
        .ops
        .iter()
        .filter(|op| {
            matches!(
                op,
                bootc_migrate_core::boot_cleanup::plan::RestoreOp::Recreate { .. }
            )
        })
        .count();
    if recreated == 0 {
        println!("No entry needed recreating — every entry in the snapshot is still present.");
    } else {
        println!("Recreated {recreated} entry(ies) from the snapshot.");
    }
    for entry in &restored.unrestorable {
        eprintln!(
            "Warning: Boot{} ({}) could not be recreated: {}. Its original definition is in the \
             snapshot's efibootmgr_v field if the firmware does not recreate it itself.",
            entry.id, entry.label, entry.why
        );
    }
    println!("Done. Verify with `efibootmgr -v` before rebooting.");
    Ok(())
}

fn parse_de(name: &str) -> Result<bootc_migrate_core::de_migrate::DesktopEnvironment> {
    bootc_migrate_core::de_migrate::parse_desktop_environment(name).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown desktop environment '{name}' \
             (expected gnome, kde, cosmic, niri, or xfce)"
        )
    })
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

/// Where a user's stash lives, relative to their home. Same value as the
/// `de-migrate stash|restore --stash-dir` default, so a stash written by the
/// standalone subcommand is found by the `rebase` flow and vice versa.
const DE_STASH_SUBDIR: &str = ".local/share/de-migrate";

/// What the DE step of a re-base will do, decided before anything is staged.
#[derive(Debug)]
struct DesktopMigrationPlan {
    from: bootc_migrate_core::de_migrate::DesktopEnvironment,
    to: bootc_migrate_core::de_migrate::DesktopEnvironment,
    users: Vec<bootc_migrate_core::de_migrate::UserHome>,
}

/// Decide whether this re-base needs a DE config stash/restore (#68), always
/// saying out loud why it does not: a silent no-op here looks identical to a
/// broken `--de-migrate`.
///
/// Every way this can come up short — the flag off, an ambiguous or
/// unrecognized desktop on either side, an unreachable registry, no human
/// accounts — degrades to "do nothing" rather than failing the re-base, but
/// none of them degrade quietly. Errors are reported once, here, so the
/// stash/restore functions themselves can propagate normally: a *failed*
/// half-move of a user's config must abort, unlike a decision not to move
/// anything at all.
fn plan_desktop_migration(args: &Args) -> Option<DesktopMigrationPlan> {
    match try_plan_desktop_migration(args) {
        Ok(plan) => plan,
        Err(e) => {
            eprintln!("Warning: DE migration skipped — {e:#}");
            None
        }
    }
}

fn try_plan_desktop_migration(args: &Args) -> Result<Option<DesktopMigrationPlan>> {
    use bootc_migrate_core::{de_detect, de_migrate};

    if !args.de_migrate {
        println!(
            "DE migration: skipped (--de-migrate not passed); per-user desktop config is \
             left exactly as it is."
        );
        return Ok(None);
    }

    let host = de_detect::detect_host_desktop().context("detecting this host's desktop")?;
    let target = de_detect::detect_image_desktop(&args.target_image)
        .with_context(|| format!("detecting the desktop shipped by {}", args.target_image))?;

    let Some((from, to)) = de_detect::cross_desktop_pair(&host, &target) else {
        println!(
            "DE migration: nothing to do (this host: {host}, {}: {target}).",
            args.target_image
        );
        return Ok(None);
    };

    let passwd = std::fs::read_to_string("/etc/passwd").context("reading /etc/passwd")?;
    let users = de_migrate::parse_user_homes(&passwd);
    if users.is_empty() {
        println!("DE migration: {from} -> {to}, but /etc/passwd has no human accounts to stash.");
        return Ok(None);
    }

    Ok(Some(DesktopMigrationPlan { from, to, users }))
}

/// Stash the outgoing DE's config for every planned user and run the
/// `pre-switch.d` hooks, before the target is staged. Hook ordering matches
/// `de-migrate stash --run-hooks`: the stash is in place by the time a hook
/// runs, so a hook can read what was moved out of the way.
///
/// A failure here aborts the re-base before anything is staged, which is the
/// point of running it first: a half-moved home is worse than no re-base.
fn run_pre_switch_desktop_migration(plan: &DesktopMigrationPlan, dry_run: bool) -> Result<()> {
    use bootc_migrate_core::de_migrate;

    println!(
        "DE migration: {} -> {} for {} user(s){}.",
        plan.from,
        plan.to,
        plan.users.len(),
        if dry_run { " [DRY RUN]" } else { "" }
    );
    let hooks = de_migrate::discover_hooks(Path::new(de_migrate::PRE_SWITCH_HOOK_DIR))
        .context("discovering pre-switch hooks")?;

    for user in &plan.users {
        let stash_dir = user.home.join(DE_STASH_SUBDIR);
        let moved = de_migrate::stash(plan.from, &user.home, &stash_dir, dry_run)
            .with_context(|| format!("stashing {} config for {}", plan.from, user.name))?;
        if moved.is_empty() {
            println!("  {}: no {} config to stash.", user.name, plan.from);
        } else {
            println!(
                "  {}: stashed {} path(s) into {}",
                user.name,
                moved.len(),
                stash_dir.display()
            );
            for p in &moved {
                println!("    {p}");
            }
        }
        // No hooks installed is the common case; don't print an empty
        // "no hooks" block once per user for it.
        if !hooks.is_empty() {
            let env = de_migrate::build_hook_env(plan.from, plan.to, &stash_dir, &user.home);
            let results = de_migrate::run_hooks(&hooks, &env, dry_run)
                .with_context(|| format!("running pre-switch hooks for {}", user.name))?;
            print_hook_results(&results);
        }
    }
    Ok(())
}

/// Re-expose the incoming DE's stash — the one a previous re-base in the
/// other direction left behind — and run the `post-switch.d` hooks, after
/// the target is staged. A first-time switch to a DE simply has no stash to
/// restore, which is reported, not treated as an error.
fn run_post_switch_desktop_migration(plan: &DesktopMigrationPlan, dry_run: bool) -> Result<()> {
    use bootc_migrate_core::de_migrate;

    let hooks = de_migrate::discover_hooks(Path::new(de_migrate::POST_SWITCH_HOOK_DIR))
        .context("discovering post-switch hooks")?;

    println!(
        "DE migration: re-exposing any previous {} stash{}.",
        plan.to,
        if dry_run { " [DRY RUN]" } else { "" }
    );
    for user in &plan.users {
        let stash_dir = user.home.join(DE_STASH_SUBDIR);
        let restored = de_migrate::restore(plan.to, &user.home, &stash_dir, dry_run)
            .with_context(|| format!("restoring {} config for {}", plan.to, user.name))?;
        if restored.is_empty() {
            println!(
                "  {}: no previous {} stash to restore (first switch to it).",
                user.name, plan.to
            );
        } else {
            println!(
                "  {}: restored {} previously-stashed {} path(s).",
                user.name,
                restored.len(),
                plan.to
            );
        }
        if !hooks.is_empty() {
            let env = de_migrate::build_hook_env(plan.from, plan.to, &stash_dir, &user.home);
            let results = de_migrate::run_hooks(&hooks, &env, dry_run)
                .with_context(|| format!("running post-switch hooks for {}", user.name))?;
            print_hook_results(&results);
        }
    }
    Ok(())
}

/// Run both halves against a plan without staging anything, so `--dry-run`
/// prints the whole DE step instead of only the part that precedes staging.
fn preview_desktop_migration(plan: &DesktopMigrationPlan) -> Result<()> {
    run_pre_switch_desktop_migration(plan, true)?;
    run_post_switch_desktop_migration(plan, true)
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

    // #68: decide the DE step before the pipeline runs so a --dry-run shows
    // it too, and stash before anything is staged.
    let de_plan = plan_desktop_migration(args);
    if args.dry_run {
        if let Some(plan) = &de_plan {
            preview_desktop_migration(plan)?;
        }
    } else if let Some(plan) = &de_plan {
        run_pre_switch_desktop_migration(plan, false)?;
    }

    println!("Starting migration to OCI image: {}...", args.target_image);
    // bootc-rebase has no interactive Config Drift Review wiring yet
    // (issue #15's TUI lives in the `bootc-migrate` binary); always fall
    // back to the default 3-way /etc merge.
    migration::run_migration(
        &report,
        &args.target_image,
        args.dry_run,
        args.skip_import,
        &args.bootloader,
        args.force,
        None,
    )?;

    if !args.dry_run
        && let Some(plan) = &de_plan
    {
        run_post_switch_desktop_migration(plan, false)?;
    }
    Ok(())
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
        Some(Commands::BootEntries(ref args)) => run_boot_entries(args),
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

    // #68: decide the DE step before staging so a --dry-run shows it too.
    let de_plan = plan_desktop_migration(args);

    if args.dry_run {
        if let Some(plan) = &de_plan {
            preview_desktop_migration(plan)?;
        }
        println!("[DRY RUN] Would run: bootc switch {}", args.target_image);
        return Ok(());
    }

    let _sleep_guard = Some(bootc_migrate_core::migration::SleepGuard::new(
        "bootc image swap in progress",
    ));

    if let Some(plan) = &de_plan {
        run_pre_switch_desktop_migration(plan, false)?;
    }

    stage_via_bootc_switch(&args.target_image)?;

    if let Some(plan) = &de_plan {
        run_post_switch_desktop_migration(plan, false)?;
    }

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

/// `ostree admin status` stdout, or an error naming why it could not be
/// obtained.
fn ostree_admin_status() -> Result<String> {
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
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The staged deployment's root directory under `/ostree/deploy/<stateroot>`,
/// found via `ostree admin status`: exactly two deployments exist right
/// after `bootc switch` stages a target (booted + staged), and the booted
/// one is marked with a leading `*` — so the other line is unambiguously the
/// staged deployment. Mirrors the parsing tests/run-e2e.sh's ostree-rebase
/// cell already relies on for its own post-merge fixture injection.
fn staged_deployment_root() -> Result<PathBuf> {
    parse_staged_deployment_root(&ostree_admin_status()?)
}

/// The booted deployment's root directory — the `*`-marked line. Needed by
/// the cross-base `/etc` policy, whose "what did the source image ship"
/// input is that deployment's `usr/etc`.
fn booted_deployment_root() -> Result<PathBuf> {
    parse_booted_deployment_root(&ostree_admin_status()?)
}

/// Build a deployment root from one `ostree admin status` deployment line
/// (`* dakota abc123.0`, or the same without the booted marker).
fn deployment_root_from_line(line: &str) -> Result<PathBuf> {
    let mut fields = line.split_whitespace().filter(|f| *f != "*");
    let stateroot = fields
        .next()
        .ok_or_else(|| anyhow::anyhow!("malformed ostree admin status line: {line}"))?;
    let checksum_serial = fields
        .next()
        .ok_or_else(|| anyhow::anyhow!("malformed ostree admin status line: {line}"))?;
    Ok(PathBuf::from("/ostree/deploy")
        .join(stateroot)
        .join("deploy")
        .join(checksum_serial))
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
    deployment_root_from_line(deploy_line)
}

/// Testable core of [`booted_deployment_root`].
fn parse_booted_deployment_root(admin_status_stdout: &str) -> Result<PathBuf> {
    let deploy_line = admin_status_stdout
        .lines()
        .find(|l| l.trim_start().starts_with('*'))
        .ok_or_else(|| anyhow::anyhow!("no booted deployment found in ostree admin status"))?;
    deployment_root_from_line(deploy_line)
}

/// Apply the cross-base remap plan (chown /var + preserved /etc in the
/// staged deployment to the target's ids) after `bootc switch` has staged
/// it. No-op when `plan` is empty (same-base re-base, or no accounts
/// diverged even though the bases differ).
fn apply_cross_base_remap(staged_root: &Path, plan: &remap::RemapPlan) -> Result<()> {
    if plan.is_empty() {
        return Ok(());
    }
    let changed = remap::apply_remap_plan(staged_root, plan)
        .context("failed to apply cross-base UID/GID remap")?;
    println!(
        "Cross-base remap applied: {changed} file(s)/dir(s) rechowned under {}",
        staged_root.display()
    );
    Ok(())
}

/// The user's live `/etc` — the middle input of the cross-base three-way
/// conflict check, and the same tree `bootc switch` merged from.
const LIVE_ETC: &str = "/etc";

/// #67 part 2: reconcile the `/etc` paths where the target image ships a
/// different default *and* this host had modified the source's. `bootc
/// switch`'s native merge keeps the local value for every one of them,
/// which is right within one base lineage and wrong across two — see
/// [`etc_conflict`]'s module docs for the seam and the policy.
///
/// Runs after [`apply_cross_base_remap`] on purpose: the defaults this
/// writes already carry the target image's numeric ownership, and a chown
/// pass running afterwards could mistake one of those ids for a stale
/// source id and renumber it a second time.
///
/// Degrades to a warning rather than an error when one of the three trees
/// is missing: the re-base is already staged and sound by this point, and
/// the only consequence of skipping is that the local value stays in place
/// — exactly the pre-#67 behavior.
fn apply_cross_base_etc_policy(staged_root: &Path) -> Result<()> {
    let booted_root = booted_deployment_root()
        .context("failed to locate the booted deployment for the cross-base /etc policy")?;
    let source_defaults = etc_conflict::vendor_etc_dir(&booted_root);
    let target_defaults = etc_conflict::vendor_etc_dir(staged_root);
    let staged_etc = staged_root.join("etc");
    let current = Path::new(LIVE_ETC);

    for (label, dir) in [
        ("source image /etc defaults", source_defaults.as_path()),
        ("target image /etc defaults", target_defaults.as_path()),
        ("staged /etc", staged_etc.as_path()),
    ] {
        if !dir.is_dir() {
            eprintln!(
                "Warning: {label} not found at {} — skipping the cross-base /etc conflict \
                 policy (#67). The staged deployment keeps `bootc switch`'s own merge result, \
                 so any target default this host had locally modified stays overridden by the \
                 local value.",
                dir.display()
            );
            return Ok(());
        }
    }

    let triples = etc_conflict::collect_etc_triples(&source_defaults, current, &target_defaults)
        .context("failed to read the three /etc trees for the cross-base conflict policy")?;
    let plan = etc_conflict::plan_etc_conflicts(&triples);
    print!("{}", etc_conflict::render_report(&plan));

    let rewritten =
        etc_conflict::apply_etc_conflict_plan(&staged_etc, current, &target_defaults, &plan)
            .context("failed to apply the cross-base /etc conflict policy")?;
    if rewritten > 0 {
        println!(
            "Cross-base /etc policy applied: {rewritten} path(s) under {} now hold the \
             target image's default; the displaced value is beside each one.",
            staged_etc.display()
        );
    }
    Ok(())
}

/// Scenario A (issue #30): re-base to another image as a plain OSTree
/// deployment. `bootc switch` already does the heavy lifting on an
/// OSTree-backed system — staging the target with OSTree's native 3-way /etc
/// merge and shared /var — so this route is preflight + gating + `bootc
/// switch` + verification. The previous deployment stays as the rollback
/// entry, matching the engine's two-phase contract.
///
/// The one place that merge is second-guessed is a cross-base re-base, where
/// "keep the user's value" is the wrong answer for a path whose vendor
/// default also moved — see [`apply_cross_base_etc_policy`].
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

    // #68: decide the DE step before staging so a --dry-run shows it too.
    let de_plan = plan_desktop_migration(args);

    if args.dry_run {
        if let Some(plan) = &de_plan {
            preview_desktop_migration(plan)?;
        }
        println!("[DRY RUN] Would run: bootc switch {}", args.target_image);
        return Ok(());
    }

    let _sleep_guard = Some(bootc_migrate_core::migration::SleepGuard::new(
        "bootc ostree re-base in progress",
    ));

    if let Some(plan) = &de_plan {
        run_pre_switch_desktop_migration(plan, false)?;
    }

    stage_via_bootc_switch(&args.target_image)?;

    if let Some(plan) = &cross_base_plan {
        let staged_root = staged_deployment_root()
            .context("failed to locate the staged deployment for cross-base post-processing")?;
        apply_cross_base_remap(&staged_root, plan)?;
        apply_cross_base_etc_policy(&staged_root)?;
    }

    if let Some(plan) = &de_plan {
        run_post_switch_desktop_migration(plan, false)?;
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
    fn booted_deployment_root_picks_the_starred_line() {
        // The staged deployment is listed first; the booted one carries the
        // '*' marker wherever it appears.
        let status = "  bluefin def456.1\n* dakota abc123.0\n";
        assert_eq!(
            parse_booted_deployment_root(status).unwrap(),
            PathBuf::from("/ostree/deploy/dakota/deploy/abc123.0")
        );
        assert_eq!(
            parse_staged_deployment_root(status).unwrap(),
            PathBuf::from("/ostree/deploy/bluefin/deploy/def456.1")
        );
    }

    #[test]
    fn booted_deployment_root_errors_when_nothing_is_booted() {
        assert!(parse_booted_deployment_root("  bluefin def456.1\n").is_err());
        assert!(parse_booted_deployment_root("* only-one-field\n").is_err());
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
    fn parse_de_accepts_every_known_desktop_and_rejects_others() {
        use bootc_migrate_core::de_migrate::DesktopEnvironment;
        let cases: &[(&str, Option<DesktopEnvironment>)] = &[
            ("gnome", Some(DesktopEnvironment::Gnome)),
            ("GNOME", Some(DesktopEnvironment::Gnome)),
            ("kde", Some(DesktopEnvironment::Kde)),
            ("cosmic", Some(DesktopEnvironment::Cosmic)),
            ("niri", Some(DesktopEnvironment::Niri)),
            ("xfce", Some(DesktopEnvironment::Xfce)),
            ("plasma", None),
            ("", None),
        ];
        for (input, expected) in cases {
            match (parse_de(input), expected) {
                (Ok(de), Some(want)) => assert_eq!(de, *want, "parsing {input:?}"),
                (Err(e), None) => assert!(
                    e.to_string().contains("unknown desktop environment"),
                    "parsing {input:?}: unexpected error {e}"
                ),
                (got, want) => panic!("parsing {input:?}: got {got:?}, wanted {want:?}"),
            }
        }
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
    fn apply_confirmation_cases() {
        // (typed answer, accepted)
        let cases: &[(&str, bool)] = &[
            ("yes", true),
            ("YES", true),
            ("  yes\n", true),
            // A single keystroke must not be enough to mutate NVRAM.
            ("y", false),
            ("Y", false),
            ("", false),
            ("\n", false),
            ("no", false),
            ("yes please", false),
            ("yess", false),
        ];
        for (answer, accepted) in cases {
            assert_eq!(
                confirmation_accepted(answer),
                *accepted,
                "answer={answer:?}"
            );
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
    fn migrate_bootloader_refuses_unsupported_target() {
        let args = MigrateBootloaderArgs {
            to: "grub2".into(),
            dry_run: false,
            undo: false,
            promote: false,
            resync: false,
            entry_token: None,
            kernel_version: None,
            title: None,
            from_image: None,
        };
        let err = run_migrate_bootloader(&args).unwrap_err();
        assert!(err.to_string().contains("systemd-boot"));
    }

    #[test]
    fn migrate_bootloader_resync_requires_its_args() {
        let args = MigrateBootloaderArgs {
            to: "systemd-boot".into(),
            dry_run: false,
            undo: false,
            promote: false,
            resync: true,
            entry_token: None,
            kernel_version: None,
            title: None,
            from_image: None,
        };
        let err = run_migrate_bootloader(&args).unwrap_err();
        assert!(err.to_string().contains("--entry-token"));
    }
}
