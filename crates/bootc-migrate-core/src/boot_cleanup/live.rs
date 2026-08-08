//! The `efibootmgr` executor for a plan [`super::plan`] already approved,
//! plus the NVRAM snapshot that makes the whole thing reversible.
//!
//! **This module contains no policy.** It never decides that an entry is
//! removable; it is handed a [`CleanupPlan`] and turns it into process
//! calls. Everything that could refuse lives in the planner, where it is
//! unit-testable without a UEFI system.
//!
//! Ordering rules that are safety properties, not implementation detail:
//!
//! 1. [`write_snapshot`] runs before any mutation and its path is printed,
//!    so `efibootmgr -v` output and `BootOrder` are recoverable by hand
//!    even if this process dies mid-run.
//! 2. A rename **creates the new entry before deleting the old one**.
//!    `efibootmgr` has no rename; a delete-then-create would leave a window
//!    with no NVRAM entry pointing at that loader, and a power loss inside
//!    it would strand the machine. Create-first makes the worst case a
//!    duplicate entry, which is a cosmetic problem.
//! 3. A rename then puts the new id where the old one sat in `BootOrder`,
//!    reading that order *before* the create (`efibootmgr --create` adds
//!    the new entry to the front itself). Without this the default entry
//!    is silently demoted by its own rename.
//! 4. `--undo` restores the recorded `BootOrder` but keeps any id NVRAM
//!    gained since the snapshot, appended behind it: undoing a cleanup
//!    restores a priority, it must not remove a boot path it never created.
//!
//! **Validation status**: none of this is exercised by `cargo test` or by
//! the E2E harness — there is no cell that mutates NVRAM. It needs a real
//! UEFI machine or a corral VM (AGENTS.md, "Interactive testing with Corral
//! VMs") before it is trusted.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use super::plan::{
    CleanupPlan, PlannedOp, RestoreOp, RestorePlan, merge_boot_order, plan_restore,
    remap_boot_order,
};
use crate::boot_audit::{BootEntry, parse_efibootmgr_entries};
use crate::migration::rollback::{parse_boot_current, parse_boot_order};

/// Where pre-mutation NVRAM snapshots are written. Under `/var/lib` rather
/// than `/var/tmp` deliberately: this is the undo record, and it has to
/// survive a reboot and a tmp cleaner.
pub const BACKUP_DIR: &str = "/var/lib/bootc-rebase/boot-entry-backups";

/// A full record of NVRAM as it was before a cleanup ran.
///
/// Both representations are kept on purpose: `entries` drives `--undo`,
/// and `efibootmgr_v` is the verbatim text a human needs to rebuild an
/// entry by hand if the automated restore cannot (firmware-owned entries
/// have no loader path to recreate from).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NvramSnapshot {
    pub taken_at_unix_secs: u64,
    pub boot_order: Option<String>,
    pub boot_current: Option<String>,
    pub entries: Vec<BootEntry>,
    pub efibootmgr_v: String,
}

/// Build a snapshot from `efibootmgr -v` output and a timestamp. Split
/// from [`write_snapshot`] so the parsing stays I/O-free.
pub fn snapshot_from_output(efibootmgr_v: &str, taken_at_unix_secs: u64) -> NvramSnapshot {
    NvramSnapshot {
        taken_at_unix_secs,
        boot_order: parse_boot_order(efibootmgr_v),
        boot_current: parse_boot_current(efibootmgr_v),
        entries: parse_efibootmgr_entries(efibootmgr_v),
        efibootmgr_v: efibootmgr_v.to_string(),
    }
}

/// Snapshot filename for a given capture time. Fixed-width seconds keep
/// the directory listing sortable by eye as well as by
/// [`parse_snapshot_timestamp`].
pub fn snapshot_filename(taken_at_unix_secs: u64) -> String {
    format!("nvram-{taken_at_unix_secs}.json")
}

/// Recover the capture time from a snapshot filename, or `None` if the
/// name was not produced by [`snapshot_filename`].
pub fn parse_snapshot_timestamp(filename: &str) -> Option<u64> {
    filename
        .strip_prefix("nvram-")?
        .strip_suffix(".json")?
        .parse()
        .ok()
}

/// Seconds since the epoch, or 0 if the clock is before it (which would
/// only happen on a badly-set RTC — a useless-but-harmless filename beats
/// refusing to take a backup).
fn now_unix_secs() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs(),
        Err(e) => {
            eprintln!(
                "Warning: system clock is before the Unix epoch ({e}); \
                 naming this NVRAM snapshot 0."
            );
            0
        }
    }
}

/// Capture NVRAM and write it to `dir`, returning the file's path. Called
/// before any mutation; the caller is expected to print the path.
pub fn write_snapshot(dir: &Path, efibootmgr_v: &str) -> Result<(PathBuf, NvramSnapshot)> {
    let snapshot = snapshot_from_output(efibootmgr_v, now_unix_secs());
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(snapshot_filename(snapshot.taken_at_unix_secs));
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&snapshot).expect("NvramSnapshot serialization cannot fail"),
    )
    .with_context(|| format!("writing NVRAM snapshot to {}", path.display()))?;
    Ok((path, snapshot))
}

/// Read a snapshot written by [`write_snapshot`].
pub fn read_snapshot(path: &Path) -> Result<NvramSnapshot> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading NVRAM snapshot {}", path.display()))?;
    serde_json::from_str(&text)
        .with_context(|| format!("parsing NVRAM snapshot {}", path.display()))
}

/// The most recent snapshot in `dir`, or `None` if there are none. A
/// missing directory is "no snapshots", not an error — it just means no
/// cleanup has ever run.
pub fn latest_snapshot(dir: &Path) -> Result<Option<PathBuf>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let mut best: Option<(u64, PathBuf)> = None;
    for entry in entries {
        let entry = entry.with_context(|| format!("listing {}", dir.display()))?;
        let name = entry.file_name();
        let Some(ts) = name.to_str().and_then(parse_snapshot_timestamp) else {
            continue;
        };
        if best.as_ref().is_none_or(|(best_ts, _)| ts > *best_ts) {
            best = Some((ts, entry.path()));
        }
    }
    Ok(best.map(|(_, p)| p))
}

/// Run `efibootmgr` with `args`, failing with its stderr on a non-zero
/// exit. Every NVRAM mutation in this module goes through here so no call
/// site can accidentally ignore a failure.
fn efibootmgr(args: &[&str]) -> Result<String> {
    let out = Command::new("efibootmgr")
        .args(args)
        .output()
        .with_context(|| format!("failed to execute efibootmgr {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "efibootmgr {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Current NVRAM entries, parsed.
fn current_entries() -> Result<Vec<BootEntry>> {
    Ok(parse_efibootmgr_entries(&efibootmgr(&["-v"])?))
}

/// The id present in `after` but not in `before` — how a freshly created
/// entry's firmware-assigned id is discovered. Returns `None` when zero or
/// more than one id appeared, since neither can be attributed to our own
/// `--create` with confidence.
pub fn newly_created_id(before: &[BootEntry], after: &[BootEntry]) -> Option<String> {
    let mut new_ids = after
        .iter()
        .filter(|a| !before.iter().any(|b| b.id.eq_ignore_ascii_case(&a.id)))
        .map(|a| a.id.clone());
    let first = new_ids.next()?;
    if new_ids.next().is_some() {
        return None;
    }
    Some(first)
}

/// Create an NVRAM entry pointing at `loader_path` on the ESP's
/// disk/partition, returning the id the firmware assigned it.
fn create_entry(esp_disk: &str, esp_part: &str, loader_path: &str, label: &str) -> Result<String> {
    let before = current_entries()?;
    efibootmgr(&[
        "--create",
        "--disk",
        esp_disk,
        "--part",
        esp_part,
        "--loader",
        loader_path,
        "--label",
        label,
    ])?;
    let after = current_entries()?;
    newly_created_id(&before, &after).ok_or_else(|| {
        anyhow::anyhow!(
            "created a boot entry labelled {label:?} but could not identify its new Boot#### id \
             (expected exactly one new id in efibootmgr output). Check `efibootmgr -v` by hand \
             before running anything else."
        )
    })
}

/// Delete an NVRAM entry by id.
fn delete_entry(id: &str) -> Result<()> {
    efibootmgr(&["-b", id, "-B"]).map(|_| ())
}

/// Set `BootOrder`.
fn set_boot_order(order: &str) -> Result<()> {
    efibootmgr(&["--bootorder", order]).map(|_| ())
}

/// The ESP's backing disk and partition number, which `efibootmgr
/// --create` needs. Resolved only when an operation actually recreates an
/// entry, so a delete-only run still works on a system whose ESP device
/// path this can't parse (LVM, device-mapper — see
/// `migration::boot::get_esp_disk_and_part`).
fn esp_disk_and_part(esp_path: &Path) -> Result<(String, String)> {
    crate::migration::boot::get_esp_disk_and_part(&esp_path.display().to_string()).ok_or_else(
        || {
            anyhow::anyhow!(
                "could not determine the ESP's backing disk and partition from `findmnt {}` — \
                 that is required to recreate a boot entry, so this run would leave the entry \
                 deleted and not replaced. Nothing was changed.",
                esp_path.display()
            )
        },
    )
}

/// Execute an approved plan against live NVRAM.
///
/// Deletions are performed first so that a rename's freshly created entry
/// cannot be confused with an entry a deletion was about to remove.
pub fn apply_plan(plan: &CleanupPlan, esp_path: &Path) -> Result<()> {
    // Resolve this up front: a plan that renames but can't recreate must
    // fail before it deletes anything, not halfway through.
    let esp_device = if plan.rename_count() > 0 {
        Some(esp_disk_and_part(esp_path)?)
    } else {
        None
    };

    for op in &plan.ops {
        if let PlannedOp::Delete { id, label, .. } = op {
            println!("Deleting Boot{id} ({label})...");
            delete_entry(id).with_context(|| format!("deleting Boot{id} ({label})"))?;
        }
    }

    let (esp_disk, esp_part) = match &esp_device {
        Some((d, p)) => (d.as_str(), p.as_str()),
        // No renames in this plan, so nothing below runs.
        None => return Ok(()),
    };

    for op in &plan.ops {
        let PlannedOp::Rename {
            id,
            from_label,
            to_label,
            loader_path,
        } = op
        else {
            continue;
        };

        println!("Renaming Boot{id}: {from_label:?} -> {to_label:?} (create, then delete)...");

        // Read BootOrder *before* the create. `efibootmgr --create` puts
        // the new entry at the front of BootOrder itself, so reading it
        // afterwards and then substituting the old id for the new one
        // would list the new entry twice.
        let order_before = parse_boot_order(&efibootmgr(&["-v"])?);

        // Deliberately create first: efibootmgr has no rename, and a
        // delete-then-create leaves a window in which nothing points at
        // this loader.
        let new_id = create_entry(esp_disk, esp_part, loader_path, to_label)
            .with_context(|| format!("creating the renamed replacement for Boot{id}"))?;

        // From here on the machine has *two* entries for this loader, so
        // any failure is recoverable by hand; say so rather than leaving
        // the user to work it out from an errno.
        delete_entry(id).with_context(|| {
            format!(
                "deleting the old Boot{id} after creating its replacement Boot{new_id}. \
                 Both entries currently exist; remove Boot{id} by hand with \
                 `efibootmgr -b {id} -B` once you have checked `efibootmgr -v`"
            )
        })?;

        // Put the replacement where the original sat, so a rename never
        // silently changes what this machine boots by default.
        if let Some(order) = order_before {
            let remapped = remap_boot_order(&order, &[(id.clone(), new_id.clone())]);
            set_boot_order(&remapped).with_context(|| {
                format!("restoring Boot{new_id} to Boot{id}'s position in BootOrder")
            })?;
        } else {
            eprintln!(
                "Warning: firmware reported no BootOrder, so Boot{new_id} was left wherever \
                 the firmware placed it after creation."
            );
        }
        println!("  Boot{id} -> Boot{new_id} ({to_label})");
    }

    Ok(())
}

/// Restore NVRAM from `snapshot`: recreate entries the cleanup removed and
/// put `BootOrder` back, remapping the ids the firmware reassigned.
///
/// Purely additive — entries created since the snapshot are left alone,
/// because undoing a cleanup is not a mandate to delete anything.
pub fn run_undo(snapshot: &NvramSnapshot, esp_path: &Path) -> Result<RestorePlan> {
    let current = current_entries()?;
    let plan = plan_restore(&snapshot.entries, snapshot.boot_order.as_deref(), &current);

    let needs_create = plan
        .ops
        .iter()
        .any(|op| matches!(op, RestoreOp::Recreate { .. }));
    let esp_device = if needs_create {
        Some(esp_disk_and_part(esp_path)?)
    } else {
        None
    };

    let mut id_remap: Vec<(String, String)> = Vec::new();
    for op in &plan.ops {
        match op {
            RestoreOp::Recreate {
                original_id,
                label,
                loader_path,
            } => {
                println!("Recreating Boot{original_id} ({label}) -> {loader_path}...");
                let (esp_disk, esp_part) = esp_device
                    .as_ref()
                    .expect("esp_device is resolved whenever the plan has a Recreate op");
                let new_id = create_entry(esp_disk, esp_part, loader_path, label)
                    .with_context(|| format!("recreating Boot{original_id} ({label})"))?;
                println!("  recreated as Boot{new_id}");
                id_remap.push((original_id.clone(), new_id));
            }
            RestoreOp::SetBootOrder { order } => {
                let remapped = remap_boot_order(order, &id_remap);
                // Keep anything NVRAM gained since the snapshot: an undo
                // restores a priority, it does not remove boot paths it
                // never created.
                let merged = match parse_boot_order(&efibootmgr(&["-v"])?) {
                    Some(current) => merge_boot_order(&remapped, &current),
                    None => remapped,
                };
                println!("Restoring BootOrder to {merged}...");
                set_boot_order(&merged).context("restoring BootOrder from the snapshot")?;
            }
        }
    }

    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
BootCurrent: 0001\n\
Timeout: 1 seconds\n\
BootOrder: 0001,0000\n\
Boot0000* Fedora\tHD(1,GPT,123)/File(\\EFI\\fedora\\shimx64.efi)\n\
Boot0001* Linux Boot Manager\tHD(1,GPT,123)/File(\\EFI\\systemd\\systemd-bootx64.efi)\n";

    fn entry(id: &str, label: &str) -> BootEntry {
        BootEntry {
            id: id.to_string(),
            label: label.to_string(),
            active: true,
            loader_path: Some("\\EFI\\x.efi".to_string()),
        }
    }

    #[test]
    fn snapshot_captures_both_representations() {
        let snapshot = snapshot_from_output(SAMPLE, 1_700_000_000);
        assert_eq!(snapshot.taken_at_unix_secs, 1_700_000_000);
        assert_eq!(snapshot.boot_order.as_deref(), Some("0001,0000"));
        assert_eq!(snapshot.boot_current.as_deref(), Some("0001"));
        assert_eq!(snapshot.entries.len(), 2);
        assert_eq!(snapshot.entries[0].label, "Fedora");
        // The raw text is kept verbatim for hand recovery.
        assert_eq!(snapshot.efibootmgr_v, SAMPLE);
    }

    #[test]
    fn snapshot_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let (path, written) = write_snapshot(dir.path(), SAMPLE).unwrap();
        assert_eq!(read_snapshot(&path).unwrap(), written);
    }

    #[test]
    fn snapshot_filename_round_trips() {
        // (timestamp or filename, expectation)
        for secs in [0_u64, 1, 1_700_000_000, u64::MAX] {
            let name = snapshot_filename(secs);
            assert_eq!(parse_snapshot_timestamp(&name), Some(secs), "{name}");
        }
        for bogus in [
            "nvram.json",
            "nvram-.json",
            "nvram-abc.json",
            "nvram-123.txt",
            "other-123.json",
            "",
        ] {
            assert_eq!(parse_snapshot_timestamp(bogus), None, "{bogus}");
        }
    }

    #[test]
    fn latest_snapshot_picks_the_newest_and_ignores_strays() {
        let dir = tempfile::tempdir().unwrap();
        // Missing directory is "nothing to undo", not an error.
        assert!(
            latest_snapshot(&dir.path().join("absent"))
                .unwrap()
                .is_none()
        );
        assert!(latest_snapshot(dir.path()).unwrap().is_none());

        for secs in [1_700_000_000_u64, 1_700_000_500, 999] {
            std::fs::write(dir.path().join(snapshot_filename(secs)), "{}").unwrap();
        }
        // A stray file must not be mistaken for a snapshot, even though it
        // sorts last.
        std::fs::write(dir.path().join("zzz-notes.txt"), "").unwrap();

        assert_eq!(
            latest_snapshot(dir.path()).unwrap().unwrap().file_name(),
            Some(snapshot_filename(1_700_000_500).as_ref())
        );
    }

    #[test]
    fn newly_created_id_cases() {
        let before = vec![entry("0000", "Fedora"), entry("0001", "Linux Boot Manager")];
        // Exactly one new id: that's ours.
        assert_eq!(
            newly_created_id(
                &before,
                &[
                    before[0].clone(),
                    before[1].clone(),
                    entry("0007", "Dakota")
                ]
            ),
            Some("0007".to_string())
        );
        // Nothing appeared.
        assert_eq!(newly_created_id(&before, &before), None);
        // Two appeared — not attributable to our own --create.
        assert_eq!(
            newly_created_id(
                &before,
                &[
                    before[0].clone(),
                    entry("0007", "Dakota"),
                    entry("0008", "Something else"),
                ]
            ),
            None
        );
        // Case-insensitive id comparison: efibootmgr prints uppercase hex.
        assert_eq!(
            newly_created_id(&[entry("000a", "x")], &[entry("000A", "x")]),
            None
        );
    }
}
