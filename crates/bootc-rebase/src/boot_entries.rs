//! CLI adapter for `bootc-rebase boot-entries`: the audit table, the cleanup
//! plan presentation, the destructive-apply confirmation, and the undo
//! dispatch.
//!
//! Every safety decision and every NVRAM mutation is delegated to
//! `bootc_migrate_core::boot_audit` and `bootc_migrate_core::boot_cleanup`;
//! nothing here decides *what* is safe to delete. `main.rs` only dispatches
//! `Commands::BootEntries` to [`run_boot_entries`].

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

use bootc_migrate_core::migration;

use crate::BootEntriesArgs;
use crate::boot_entry_review;

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
pub(crate) fn run_boot_entries(args: &BootEntriesArgs) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
