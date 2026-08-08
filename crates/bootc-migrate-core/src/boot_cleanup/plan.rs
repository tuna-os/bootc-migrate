//! Pure planning for boot-entry cleanup: audit + user selection → a list of
//! operations, or a typed refusal explaining which safety rule stopped it.
//!
//! Nothing here performs I/O. That is the point: every rule that decides
//! whether a UEFI entry may be deleted or renamed is exercisable from a
//! unit test, so the executor in [`super::live`] never has to make a
//! judgement call while holding a live NVRAM handle.

use serde::Serialize;

use crate::boot_audit::{AuditFlag, AuditedEntry, BootEntry};
use crate::migration::rollback::is_ostree_rollback_entry;

/// Why an entry may never be deleted. These are checked before a selection
/// is honored *and* surfaced to the UI so protected entries can be rendered
/// unselectable rather than merely unselected — issue #31 asks for the
/// former ("never auto-remove firmware/setup entries", "preserve the
/// rollback escape hatch"), and "unselected by default" is only a
/// convention a user can click through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeleteProtection {
    /// PXE/HTTP boot, EFI Shell, Setup, removable-media fallback: owned by
    /// the firmware, frequently recreated by it, and never a real install.
    FirmwareManaged,
    /// The entry the running system booted from. Deleting it strands the
    /// machine on whatever the firmware picks next.
    CurrentlyBooted,
    /// The OSTree/GRUB entry `bootc-migrate rollback` re-orders back to the
    /// front. Removing it deletes the documented path back to the
    /// pre-migration deployment.
    RollbackPath,
}

impl DeleteProtection {
    /// One-line reason, for both CLI output and the TUI's detail pane.
    pub fn describe(self) -> &'static str {
        match self {
            Self::FirmwareManaged => "firmware-managed entry (PXE/Shell/Setup/removable fallback)",
            Self::CurrentlyBooted => "this system booted from this entry (BootCurrent)",
            Self::RollbackPath => "the rollback path back to the pre-migration deployment",
        }
    }
}

/// Why an entry cannot be renamed. A rename against NVRAM is a
/// delete+recreate, so an entry we could not faithfully recreate must not
/// be renamed at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RenameBlock {
    /// Same reason as [`DeleteProtection::FirmwareManaged`] — a rename
    /// deletes the entry first, and the firmware owns it.
    FirmwareManaged,
    /// No `File(...)` loader path was parsed, so `efibootmgr --create`
    /// has nothing to point the recreated entry at.
    NoLoaderPath,
    /// The loader path does not resolve on the audited ESP. Either the
    /// entry is genuinely dead (delete it instead of renaming it) or it
    /// lives on a disk this audit did not look at — recreating it against
    /// *our* ESP would silently repoint it.
    LoaderNotOnThisEsp,
    /// The proposed label is what the entry is already called.
    LabelUnchanged,
}

impl RenameBlock {
    /// One-line reason, for both CLI output and the TUI's detail pane.
    pub fn describe(self) -> &'static str {
        match self {
            Self::FirmwareManaged => "firmware-managed entries are never rewritten",
            Self::NoLoaderPath => "no loader path to recreate the entry with",
            Self::LoaderNotOnThisEsp => "loader path does not resolve on this ESP",
            Self::LabelUnchanged => "already has this label",
        }
    }
}

/// NVRAM facts the planner needs beyond the audit itself, parsed from the
/// same `efibootmgr -v` output (see
/// [`crate::migration::rollback::parse_boot_current`] /
/// [`crate::migration::rollback::parse_boot_order`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NvramFacts {
    /// `BootCurrent` — the entry this boot came from, if the firmware
    /// reports one.
    pub boot_current: Option<String>,
    /// `BootOrder`, verbatim.
    pub boot_order: Option<String>,
    /// The id [`crate::migration::rollback::parse_ostree_boot_entry_id`]
    /// picks out of the same output — i.e. the entry `bootc-migrate
    /// rollback` would actually re-order to the front. Protected
    /// unconditionally, even when it looks dead, because that function's
    /// choice is what rollback will use whatever this audit believes.
    pub rollback_entry_id: Option<String>,
}

/// One mutation the executor is authorized to perform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PlannedOp {
    Delete {
        id: String,
        label: String,
        /// The audit flags that made this entry a candidate, so the
        /// preview can say *why* each deletion was proposed.
        flags: Vec<AuditFlag>,
    },
    Rename {
        id: String,
        from_label: String,
        to_label: String,
        /// Carried through because the recreated entry must point at the
        /// exact same loader.
        loader_path: String,
    },
}

/// The approved set of mutations. Produced only by [`plan_cleanup`], so
/// holding one means every safety rule already passed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CleanupPlan {
    pub ops: Vec<PlannedOp>,
}

impl CleanupPlan {
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    pub fn delete_count(&self) -> usize {
        self.ops
            .iter()
            .filter(|o| matches!(o, PlannedOp::Delete { .. }))
            .count()
    }

    pub fn rename_count(&self) -> usize {
        self.ops
            .iter()
            .filter(|o| matches!(o, PlannedOp::Rename { .. }))
            .count()
    }
}

/// What the user asked for. Ids are `efibootmgr`'s 4-hex-digit ids without
/// the `Boot` prefix, matching [`BootEntry::id`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanupSelection {
    pub delete_ids: Vec<String>,
    /// `(entry id, proposed new label)`.
    pub renames: Vec<(String, String)>,
}

impl CleanupSelection {
    pub fn is_empty(&self) -> bool {
        self.delete_ids.is_empty() && self.renames.is_empty()
    }
}

/// A refusal to plan. Every variant is a safety rule, not an internal
/// error — callers are expected to print these to the user verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// Every entry that has a loader path was classified dead. On a
    /// running machine at least the booted entry's loader must resolve, so
    /// this is evidence the ESP root was resolved incorrectly (the exact
    /// failure mode that would otherwise turn "select all → apply" into
    /// wiping the working boot path) — not evidence that every install is
    /// gone.
    EspEvidenceImplausible { loader_entries: usize },
    /// A selected id is not in the audit.
    UnknownEntry { id: String },
    /// A selected id is protected from deletion.
    ProtectedEntry {
        id: String,
        protection: DeleteProtection,
    },
    /// A rename target cannot be safely recreated.
    NotRenameable { id: String, block: RenameBlock },
    /// The same entry was selected for deletion and rename.
    ConflictingOps { id: String },
    /// A proposed label is empty or contains control characters, which
    /// would produce an unusable NVRAM entry.
    InvalidLabel { id: String, label: String },
    /// Applying the deletions would leave no entry whose loader actually
    /// resolves — i.e. no way back to a booted system.
    RemovesLastBootableEntry,
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EspEvidenceImplausible { loader_entries } => write!(
                f,
                "refusing to plan: all {loader_entries} boot entries with a loader path were \
                 classified dead, which means the ESP was almost certainly resolved to the wrong \
                 filesystem rather than every install being gone — re-run the read-only audit and \
                 check the ESP mount first"
            ),
            Self::UnknownEntry { id } => {
                write!(f, "no boot entry Boot{id} in the audit")
            }
            Self::ProtectedEntry { id, protection } => {
                write!(f, "Boot{id} may not be deleted: {}", protection.describe())
            }
            Self::NotRenameable { id, block } => {
                write!(f, "Boot{id} may not be renamed: {}", block.describe())
            }
            Self::ConflictingOps { id } => write!(
                f,
                "Boot{id} was selected for both deletion and rename; pick one"
            ),
            Self::InvalidLabel { id, label } => write!(
                f,
                "proposed label {label:?} for Boot{id} is empty or contains control characters"
            ),
            Self::RemovesLastBootableEntry => write!(
                f,
                "refusing to plan: this selection would delete every boot entry whose loader \
                 still exists, leaving no way back to a booted system"
            ),
        }
    }
}

impl std::error::Error for PlanError {}

/// Whether an entry's loader is present on the audited ESP — the property
/// that makes it a real way to boot this machine.
fn is_bootable(entry: &AuditedEntry) -> bool {
    entry.entry.loader_path.is_some() && !entry.flags.contains(&AuditFlag::Dead)
}

/// The delete-protection covering `entry`, if any. Checked in
/// most-categorical-first order so the reason a user sees is the strongest
/// one.
pub fn delete_protection(entry: &AuditedEntry, facts: &NvramFacts) -> Option<DeleteProtection> {
    if entry.flags.contains(&AuditFlag::FirmwareManaged) {
        return Some(DeleteProtection::FirmwareManaged);
    }
    if facts
        .boot_current
        .as_deref()
        .is_some_and(|c| c.eq_ignore_ascii_case(&entry.entry.id))
    {
        return Some(DeleteProtection::CurrentlyBooted);
    }
    if facts
        .rollback_entry_id
        .as_deref()
        .is_some_and(|r| r.eq_ignore_ascii_case(&entry.entry.id))
    {
        return Some(DeleteProtection::RollbackPath);
    }
    // Any *other* entry that looks like a shim/GRUB/OSTree loader is
    // protected too — but only while its loader still resolves. The
    // markers are deliberately broad ("shim" matches every distro's
    // shim), and a dead one is by definition not a path back to anything;
    // refusing to remove it would make cleanup useless for the exact
    // graveyard-of-wiped-installs case issue #31 exists for. The entry
    // rollback would genuinely use is covered by `rollback_entry_id`
    // above, dead or not.
    if !entry.flags.contains(&AuditFlag::Dead)
        && is_ostree_rollback_entry(&entry.entry.label, entry.entry.loader_path.as_deref())
    {
        return Some(DeleteProtection::RollbackPath);
    }
    None
}

/// Whether `entry` can be renamed to `new_label`, and if not, why.
pub fn rename_block(entry: &AuditedEntry, new_label: &str) -> Option<RenameBlock> {
    if entry.flags.contains(&AuditFlag::FirmwareManaged) {
        return Some(RenameBlock::FirmwareManaged);
    }
    if entry.entry.loader_path.is_none() {
        return Some(RenameBlock::NoLoaderPath);
    }
    if entry.flags.contains(&AuditFlag::Dead) {
        return Some(RenameBlock::LoaderNotOnThisEsp);
    }
    if entry.entry.label == new_label {
        return Some(RenameBlock::LabelUnchanged);
    }
    None
}

/// Whether a proposed NVRAM label is usable. `efibootmgr` takes the label
/// as a UTF-16 description; an empty one produces an unidentifiable entry
/// and control characters corrupt firmware boot menus.
fn label_is_valid(label: &str) -> bool {
    !label.trim().is_empty() && !label.chars().any(char::is_control)
}

/// The conservative default deletion set: entries the audit calls clearly
/// dead *and* that carry no delete protection. Duplicates and
/// generic-label entries are deliberately excluded — issue #31 says
/// pre-select only clearly-dead entries; the rest are surfaced for a human
/// to decide on.
pub fn default_delete_selection(audited: &[AuditedEntry], facts: &NvramFacts) -> Vec<String> {
    audited
        .iter()
        .filter(|a| a.safe_to_preselect() && delete_protection(a, facts).is_none())
        .map(|a| a.entry.id.clone())
        .collect()
}

/// The non-interactive branding-rename proposal: at most one, for the
/// entry this system actually booted from (`BootCurrent`), when that entry
/// carries a generic label and can be faithfully recreated under
/// `pretty_name` (from `/etc/os-release`'s `PRETTY_NAME`).
///
/// Deliberately *one* entry. Renaming every generic-labelled entry to the
/// same `PRETTY_NAME` would leave a boot menu of identical labels — worse
/// than the generic names it replaced — and only `BootCurrent` is known to
/// belong to this installation, which is what issue #31 asks to brand
/// ("rename the migration's own loader entry"). Renaming any other entry
/// is a per-entry human decision, made in the interactive checklist.
///
/// Returns nothing when `pretty_name` is unusable, rather than proposing a
/// rename to an empty label.
pub fn branding_renames(
    audited: &[AuditedEntry],
    facts: &NvramFacts,
    pretty_name: &str,
) -> Vec<(String, String)> {
    let label = pretty_name.trim();
    if !label_is_valid(label) {
        return Vec::new();
    }
    let Some(booted) = facts.boot_current.as_deref() else {
        return Vec::new();
    };
    audited
        .iter()
        .filter(|a| a.entry.id.eq_ignore_ascii_case(booted))
        .filter(|a| a.flags.contains(&AuditFlag::GenericLabel))
        .filter(|a| rename_block(a, label).is_none())
        .map(|a| (a.entry.id.clone(), label.to_string()))
        .collect()
}

/// Turn an audit plus a selection into the operations the executor may
/// perform, or refuse with the specific rule that was violated.
///
/// An empty selection short-circuits to an empty plan: planning nothing is
/// always allowed, including on a system whose ESP evidence looks wrong.
pub fn plan_cleanup(
    audited: &[AuditedEntry],
    facts: &NvramFacts,
    selection: &CleanupSelection,
) -> Result<CleanupPlan, PlanError> {
    if selection.is_empty() {
        return Ok(CleanupPlan::default());
    }

    // Sanity-check the audit itself before honoring any selection built on
    // top of it: a mis-resolved ESP makes every entry look dead.
    let loader_entries = audited
        .iter()
        .filter(|a| a.entry.loader_path.is_some())
        .count();
    if loader_entries > 0 && !audited.iter().any(is_bootable) {
        return Err(PlanError::EspEvidenceImplausible { loader_entries });
    }

    let find = |id: &str| {
        audited
            .iter()
            .find(|a| a.entry.id.eq_ignore_ascii_case(id))
            .ok_or_else(|| PlanError::UnknownEntry { id: id.to_string() })
    };

    let mut ops = Vec::with_capacity(selection.delete_ids.len() + selection.renames.len());

    for id in &selection.delete_ids {
        let entry = find(id)?;
        if selection
            .renames
            .iter()
            .any(|(rid, _)| rid.eq_ignore_ascii_case(id))
        {
            return Err(PlanError::ConflictingOps { id: id.clone() });
        }
        if let Some(protection) = delete_protection(entry, facts) {
            return Err(PlanError::ProtectedEntry {
                id: id.clone(),
                protection,
            });
        }
        ops.push(PlannedOp::Delete {
            id: entry.entry.id.clone(),
            label: entry.entry.label.clone(),
            flags: entry.flags.clone(),
        });
    }

    for (id, new_label) in &selection.renames {
        let entry = find(id)?;
        let new_label = new_label.trim();
        if !label_is_valid(new_label) {
            return Err(PlanError::InvalidLabel {
                id: id.clone(),
                label: new_label.to_string(),
            });
        }
        if let Some(block) = rename_block(entry, new_label) {
            return Err(PlanError::NotRenameable {
                id: id.clone(),
                block,
            });
        }
        ops.push(PlannedOp::Rename {
            id: entry.entry.id.clone(),
            from_label: entry.entry.label.clone(),
            to_label: new_label.to_string(),
            loader_path: entry
                .entry
                .loader_path
                .clone()
                .expect("rename_block rejects entries with no loader path"),
        });
    }

    // Last line of defence: whatever the selection was built from, the
    // machine must still have a resolvable loader afterwards. Renames keep
    // their entry (the executor creates before it deletes), so only
    // deletions can violate this.
    let bootable_before = audited.iter().filter(|a| is_bootable(a)).count();
    let bootable_after = audited
        .iter()
        .filter(|a| {
            is_bootable(a)
                && !selection
                    .delete_ids
                    .iter()
                    .any(|id| id.eq_ignore_ascii_case(&a.entry.id))
        })
        .count();
    if bootable_before > 0 && bootable_after == 0 {
        return Err(PlanError::RemovesLastBootableEntry);
    }

    Ok(CleanupPlan { ops })
}

/// One step of restoring a pre-cleanup NVRAM snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum RestoreOp {
    /// Recreate an entry the cleanup removed. The firmware assigns a fresh
    /// id, so `original_id` is informational and used only to remap
    /// `BootOrder` afterwards (see [`remap_boot_order`]).
    Recreate {
        original_id: String,
        label: String,
        loader_path: String,
    },
    /// Restore the recorded `BootOrder`, after id remapping.
    SetBootOrder { order: String },
}

/// A backup entry that cannot be put back automatically.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnrestorableEntry {
    pub id: String,
    pub label: String,
    pub why: &'static str,
}

/// What an `--undo` would do.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RestorePlan {
    pub ops: Vec<RestoreOp>,
    /// Entries present in the backup, missing now, and not recreatable —
    /// reported rather than dropped, because the raw `efibootmgr -v` text
    /// in the backup is then the only way to put them back by hand.
    pub unrestorable: Vec<UnrestorableEntry>,
}

/// Two entries are "the same entry" when they agree on label and loader
/// path; ids are firmware-assigned and get reused, so they cannot identify
/// an entry across a delete/recreate cycle.
fn same_entry(a: &BootEntry, b: &BootEntry) -> bool {
    a.label == b.label && a.loader_path == b.loader_path
}

/// Compute the restore steps that would turn `current` back into
/// `backup_entries`. Purely additive: entries created *since* the backup
/// are left alone, because an undo of a cleanup has no mandate to delete
/// anything.
pub fn plan_restore(
    backup_entries: &[BootEntry],
    backup_boot_order: Option<&str>,
    current: &[BootEntry],
) -> RestorePlan {
    let mut ops = Vec::new();
    let mut unrestorable = Vec::new();

    for entry in backup_entries {
        if current.iter().any(|c| same_entry(c, entry)) {
            continue;
        }
        match &entry.loader_path {
            Some(loader_path) => ops.push(RestoreOp::Recreate {
                original_id: entry.id.clone(),
                label: entry.label.clone(),
                loader_path: loader_path.clone(),
            }),
            None => unrestorable.push(UnrestorableEntry {
                id: entry.id.clone(),
                label: entry.label.clone(),
                why: "no loader path recorded — firmware-owned entries cannot be recreated \
                      with efibootmgr",
            }),
        }
    }

    if let Some(order) = backup_boot_order {
        ops.push(RestoreOp::SetBootOrder {
            order: order.to_string(),
        });
    }

    RestorePlan { ops, unrestorable }
}

/// Rewrite a `BootOrder` string, substituting ids per `remap`
/// (`(old, new)`), preserving position. Used both after a rename (the
/// recreated entry must sit where the old one did, or a "rename" silently
/// demotes the entry) and after an undo recreates entries under fresh ids.
///
/// Ids in `order` with no mapping are kept as-is; empty fields are dropped.
pub fn remap_boot_order(order: &str, remap: &[(String, String)]) -> String {
    order
        .split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(|id| {
            remap
                .iter()
                .find(|(old, _)| old.eq_ignore_ascii_case(id))
                .map(|(_, new)| new.clone())
                .unwrap_or_else(|| id.to_string())
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// Combine a restored `BootOrder` with the one NVRAM currently has: the
/// restored ids first, then any id present now but missing from the
/// restored order, appended in their current relative order.
///
/// Writing a recorded `BootOrder` back verbatim would drop entries created
/// after the snapshot out of the boot order entirely. An undo must never
/// remove a boot path it did not create, so those are kept — behind the
/// restored ones, since the point of the undo is to restore the recorded
/// priority.
pub fn merge_boot_order(restored: &str, current: &str) -> String {
    let restored_ids: Vec<&str> = restored
        .split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .collect();
    let extras = current
        .split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .filter(|id| !restored_ids.iter().any(|r| r.eq_ignore_ascii_case(id)));
    restored_ids
        .iter()
        .copied()
        .chain(extras)
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an audited entry without going through the ESP-touching
    /// audit, so planner tests stay filesystem-free.
    fn audited(id: &str, label: &str, loader: Option<&str>, flags: &[AuditFlag]) -> AuditedEntry {
        AuditedEntry {
            entry: BootEntry {
                id: id.to_string(),
                label: label.to_string(),
                active: true,
                loader_path: loader.map(str::to_string),
            },
            flags: flags.to_vec(),
        }
    }

    /// A realistic distrohopper's NVRAM: a live systemd-boot entry, the
    /// OSTree/GRUB rollback entry, a dead leftover, a duplicate pair, a
    /// generic label, and a firmware entry.
    fn sample() -> Vec<AuditedEntry> {
        vec![
            audited(
                "0001",
                "Linux Boot Manager",
                Some("\\EFI\\systemd\\systemd-bootx64.efi"),
                &[],
            ),
            audited("0002", "Fedora", Some("\\EFI\\fedora\\shimx64.efi"), &[]),
            audited(
                "0003",
                "Old Ubuntu",
                Some("\\EFI\\ubuntu\\shimx64.efi"),
                &[AuditFlag::Dead],
            ),
            audited(
                "0004",
                "UEFI OS",
                Some("\\EFI\\BOOT\\BOOTX64.EFI"),
                &[AuditFlag::GenericLabel],
            ),
            audited("0005", "UEFI: PXEv4", None, &[AuditFlag::FirmwareManaged]),
        ]
    }

    fn facts() -> NvramFacts {
        NvramFacts {
            boot_current: Some("0001".to_string()),
            boot_order: Some("0001,0002,0003,0004,0005".to_string()),
            rollback_entry_id: Some("0002".to_string()),
        }
    }

    #[test]
    fn delete_protection_cases() {
        let entries = sample();
        let facts = facts();
        // (entry id, expected protection)
        let cases: &[(&str, Option<DeleteProtection>)] = &[
            // BootCurrent — booted from this one.
            ("0001", Some(DeleteProtection::CurrentlyBooted)),
            // The OSTree/GRUB rollback escape hatch.
            ("0002", Some(DeleteProtection::RollbackPath)),
            // Dead leftover from a wiped Ubuntu install. Its loader path
            // contains "shim", which the (deliberately broad) rollback
            // markers match — but it is dead and is not the id rollback
            // would use, so it stays removable.
            ("0003", None),
            // Generic label, still alive, not the rollback path: removable
            // (but never pre-selected — see default_delete_selection).
            ("0004", None),
            // Firmware-owned.
            ("0005", Some(DeleteProtection::FirmwareManaged)),
        ];
        for (id, expected) in cases {
            let entry = entries.iter().find(|e| e.entry.id == *id).unwrap();
            assert_eq!(delete_protection(entry, &facts), *expected, "Boot{id}");
        }
    }

    #[test]
    fn firmware_protection_holds_even_when_the_entry_is_also_dead() {
        // A firmware entry that happens to carry a stale loader path must
        // still be unselectable — FirmwareManaged outranks Dead.
        let entry = audited(
            "0009",
            "UEFI: Built-in EFI Shell",
            Some("\\EFI\\shell.efi"),
            &[AuditFlag::FirmwareManaged, AuditFlag::Dead],
        );
        assert_eq!(
            delete_protection(&entry, &NvramFacts::default()),
            Some(DeleteProtection::FirmwareManaged)
        );
        assert!(!entry.safe_to_preselect());
    }

    /// `(current label, loader path, audit flags, proposed label, expected block)`
    type RenameCase = (
        &'static str,
        Option<&'static str>,
        &'static [AuditFlag],
        &'static str,
        Option<RenameBlock>,
    );

    /// `(BootOrder, [(old id, new id)], expected BootOrder)`
    type RemapCase = (
        &'static str,
        &'static [(&'static str, &'static str)],
        &'static str,
    );

    #[test]
    fn rename_block_cases() {
        let cases: &[RenameCase] = &[
            (
                "UEFI OS",
                Some("\\EFI\\BOOT\\BOOTX64.EFI"),
                &[AuditFlag::GenericLabel],
                "Dakota",
                None,
            ),
            (
                "UEFI: PXEv4",
                None,
                &[AuditFlag::FirmwareManaged],
                "Dakota",
                Some(RenameBlock::FirmwareManaged),
            ),
            (
                "Diagnostics",
                None,
                &[],
                "Dakota",
                Some(RenameBlock::NoLoaderPath),
            ),
            (
                "Old Ubuntu",
                Some("\\EFI\\ubuntu\\shimx64.efi"),
                &[AuditFlag::Dead],
                "Dakota",
                Some(RenameBlock::LoaderNotOnThisEsp),
            ),
            (
                "Dakota",
                Some("\\EFI\\systemd\\systemd-bootx64.efi"),
                &[],
                "Dakota",
                Some(RenameBlock::LabelUnchanged),
            ),
        ];
        for (label, loader, flags, new_label, expected) in cases {
            let entry = audited("0000", label, *loader, flags);
            assert_eq!(
                rename_block(&entry, new_label),
                *expected,
                "label={label} -> {new_label}"
            );
        }
    }

    #[test]
    fn default_selection_takes_only_clearly_dead_unprotected_entries() {
        let selection = default_delete_selection(&sample(), &facts());
        assert_eq!(selection, vec!["0003".to_string()]);
    }

    #[test]
    fn the_entry_rollback_would_use_is_protected_even_when_it_looks_dead() {
        // If the ESP resolves badly enough to flag the OSTree entry Dead,
        // rollback would still target it — so it must stay unselectable,
        // and must not be pre-selected either.
        let entries = vec![
            audited(
                "0001",
                "Linux Boot Manager",
                Some("\\EFI\\systemd\\systemd-bootx64.efi"),
                &[],
            ),
            audited(
                "0002",
                "Fedora",
                Some("\\EFI\\fedora\\shimx64.efi"),
                &[AuditFlag::Dead],
            ),
        ];
        let facts = NvramFacts {
            rollback_entry_id: Some("0002".to_string()),
            ..NvramFacts::default()
        };
        assert_eq!(
            delete_protection(&entries[1], &facts),
            Some(DeleteProtection::RollbackPath)
        );
        assert!(default_delete_selection(&entries, &facts).is_empty());
    }

    /// Booted generic-labelled entry plus a second generic one that must
    /// never be swept into the same rename.
    fn branding_fixture() -> Vec<AuditedEntry> {
        vec![
            audited(
                "0001",
                "Linux Boot Manager",
                Some("\\EFI\\systemd\\systemd-bootx64.efi"),
                &[AuditFlag::GenericLabel],
            ),
            audited(
                "0004",
                "UEFI OS",
                Some("\\EFI\\BOOT\\BOOTX64.EFI"),
                &[AuditFlag::GenericLabel],
            ),
        ]
    }

    #[test]
    fn branding_renames_cases() {
        let entries = branding_fixture();
        let facts = NvramFacts {
            boot_current: Some("0001".to_string()),
            ..NvramFacts::default()
        };
        // (PRETTY_NAME, expected (id, new label) proposals)
        let cases: &[(&str, &[(&str, &str)])] = &[
            // Only the booted entry — never both generic entries, which
            // would leave two identically-labelled entries.
            ("Dakota", &[("0001", "Dakota")]),
            // Surrounding whitespace is trimmed off PRETTY_NAME.
            ("  Dakota  ", &[("0001", "Dakota")]),
            // Nothing to change.
            ("Linux Boot Manager", &[]),
            // An unusable PRETTY_NAME proposes nothing at all.
            ("", &[]),
            ("   ", &[]),
            ("Dak\nota", &[]),
        ];
        for (pretty, expected) in cases {
            let got = branding_renames(&entries, &facts, pretty);
            let want: Vec<(String, String)> = expected
                .iter()
                .map(|(i, l)| (i.to_string(), l.to_string()))
                .collect();
            assert_eq!(got, want, "PRETTY_NAME={pretty:?}");
        }
    }

    #[test]
    fn branding_rename_needs_a_generic_booted_entry() {
        // (description, entries, BootCurrent) -> no proposal
        let cases: &[(&str, Vec<AuditedEntry>, Option<&str>)] = &[
            ("firmware reported no BootCurrent", branding_fixture(), None),
            (
                "BootCurrent names an entry that is not in the audit",
                branding_fixture(),
                Some("00FF"),
            ),
            (
                "booted entry already carries a distro name",
                vec![audited(
                    "0001",
                    "Bluefin",
                    Some("\\EFI\\systemd\\systemd-bootx64.efi"),
                    &[],
                )],
                Some("0001"),
            ),
            (
                "booted entry's loader does not resolve on this ESP",
                vec![audited(
                    "0001",
                    "Linux Boot Manager",
                    Some("\\EFI\\systemd\\systemd-bootx64.efi"),
                    &[AuditFlag::GenericLabel, AuditFlag::Dead],
                )],
                Some("0001"),
            ),
            (
                "booted entry is firmware-managed",
                vec![audited(
                    "0001",
                    "UEFI OS",
                    Some("\\EFI\\BOOT\\BOOTX64.EFI"),
                    &[AuditFlag::GenericLabel, AuditFlag::FirmwareManaged],
                )],
                Some("0001"),
            ),
        ];
        for (why, entries, boot_current) in cases {
            let facts = NvramFacts {
                boot_current: boot_current.map(str::to_string),
                ..NvramFacts::default()
            };
            assert!(
                branding_renames(entries, &facts, "Dakota").is_empty(),
                "expected no branding rename when {why}"
            );
        }
    }

    #[test]
    fn plan_cleanup_accepts_the_safe_default_selection() {
        let entries = sample();
        let facts = facts();
        let selection = CleanupSelection {
            delete_ids: default_delete_selection(&entries, &facts),
            renames: Vec::new(),
        };
        let plan = plan_cleanup(&entries, &facts, &selection).unwrap();
        assert_eq!(
            plan.ops,
            vec![PlannedOp::Delete {
                id: "0003".to_string(),
                label: "Old Ubuntu".to_string(),
                flags: vec![AuditFlag::Dead],
            }]
        );
        assert_eq!(plan.delete_count(), 1);
        assert_eq!(plan.rename_count(), 0);
    }

    #[test]
    fn plan_cleanup_builds_a_rename_op_carrying_the_loader_path() {
        let entries = sample();
        let selection = CleanupSelection {
            delete_ids: Vec::new(),
            renames: vec![("0004".to_string(), "Dakota".to_string())],
        };
        let plan = plan_cleanup(&entries, &facts(), &selection).unwrap();
        assert_eq!(
            plan.ops,
            vec![PlannedOp::Rename {
                id: "0004".to_string(),
                from_label: "UEFI OS".to_string(),
                to_label: "Dakota".to_string(),
                loader_path: "\\EFI\\BOOT\\BOOTX64.EFI".to_string(),
            }]
        );
    }

    #[test]
    fn plan_cleanup_refusal_cases() {
        let entries = sample();
        let facts = facts();
        // (selection, expected refusal)
        let cases: Vec<(CleanupSelection, PlanError)> = vec![
            (
                CleanupSelection {
                    delete_ids: vec!["0005".to_string()],
                    renames: Vec::new(),
                },
                PlanError::ProtectedEntry {
                    id: "0005".to_string(),
                    protection: DeleteProtection::FirmwareManaged,
                },
            ),
            (
                CleanupSelection {
                    delete_ids: vec!["0002".to_string()],
                    renames: Vec::new(),
                },
                PlanError::ProtectedEntry {
                    id: "0002".to_string(),
                    protection: DeleteProtection::RollbackPath,
                },
            ),
            (
                CleanupSelection {
                    delete_ids: vec!["0001".to_string()],
                    renames: Vec::new(),
                },
                PlanError::ProtectedEntry {
                    id: "0001".to_string(),
                    protection: DeleteProtection::CurrentlyBooted,
                },
            ),
            (
                CleanupSelection {
                    delete_ids: vec!["dead".to_string()],
                    renames: Vec::new(),
                },
                PlanError::UnknownEntry {
                    id: "dead".to_string(),
                },
            ),
            (
                CleanupSelection {
                    delete_ids: vec!["0003".to_string()],
                    renames: vec![("0003".to_string(), "Dakota".to_string())],
                },
                PlanError::ConflictingOps {
                    id: "0003".to_string(),
                },
            ),
            (
                CleanupSelection {
                    delete_ids: Vec::new(),
                    renames: vec![("0003".to_string(), "Dakota".to_string())],
                },
                PlanError::NotRenameable {
                    id: "0003".to_string(),
                    block: RenameBlock::LoaderNotOnThisEsp,
                },
            ),
            (
                CleanupSelection {
                    delete_ids: Vec::new(),
                    renames: vec![("0004".to_string(), "  ".to_string())],
                },
                PlanError::InvalidLabel {
                    id: "0004".to_string(),
                    label: String::new(),
                },
            ),
        ];
        for (selection, expected) in cases {
            assert_eq!(
                plan_cleanup(&entries, &facts, &selection).unwrap_err(),
                expected,
                "selection={selection:?}"
            );
        }
    }

    #[test]
    fn plan_cleanup_refuses_when_every_loader_entry_looks_dead() {
        // The mis-resolved-ESP failure mode: nothing resolves, so a
        // "select all" would otherwise wipe the working boot path.
        let entries: Vec<AuditedEntry> = sample()
            .into_iter()
            .map(|mut a| {
                if a.entry.loader_path.is_some() && !a.flags.contains(&AuditFlag::Dead) {
                    a.flags.push(AuditFlag::Dead);
                }
                a
            })
            .collect();
        let selection = CleanupSelection {
            delete_ids: vec!["0003".to_string()],
            renames: Vec::new(),
        };
        assert_eq!(
            plan_cleanup(&entries, &NvramFacts::default(), &selection).unwrap_err(),
            PlanError::EspEvidenceImplausible { loader_entries: 4 }
        );
        // ...but planning nothing is still fine.
        assert!(
            plan_cleanup(
                &entries,
                &NvramFacts::default(),
                &CleanupSelection::default()
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn plan_cleanup_refuses_to_delete_the_last_bootable_entry() {
        // No BootCurrent and no rollback-shaped label, so nothing else
        // protects this entry — only the survivor invariant does.
        let entries = vec![
            audited(
                "0001",
                "Linux Boot Manager",
                Some("\\EFI\\systemd\\x.efi"),
                &[],
            ),
            audited(
                "0002",
                "Old Arch",
                Some("\\EFI\\arch\\x.efi"),
                &[AuditFlag::Dead],
            ),
        ];
        let selection = CleanupSelection {
            delete_ids: vec!["0001".to_string(), "0002".to_string()],
            renames: Vec::new(),
        };
        assert_eq!(
            plan_cleanup(&entries, &NvramFacts::default(), &selection).unwrap_err(),
            PlanError::RemovesLastBootableEntry
        );
    }

    #[test]
    fn plan_restore_cases() {
        let backup = vec![
            BootEntry {
                id: "0001".into(),
                label: "Linux Boot Manager".into(),
                active: true,
                loader_path: Some("\\EFI\\systemd\\systemd-bootx64.efi".into()),
            },
            BootEntry {
                id: "0003".into(),
                label: "Old Ubuntu".into(),
                active: true,
                loader_path: Some("\\EFI\\ubuntu\\shimx64.efi".into()),
            },
            BootEntry {
                id: "0005".into(),
                label: "UEFI: PXEv4".into(),
                active: true,
                loader_path: None,
            },
        ];
        // Boot0003 and Boot0005 are gone; Boot0009 appeared afterwards and
        // must be left alone.
        let current = vec![
            backup[0].clone(),
            BootEntry {
                id: "0009".into(),
                label: "Windows Boot Manager".into(),
                active: true,
                loader_path: Some("\\EFI\\Microsoft\\Boot\\bootmgfw.efi".into()),
            },
        ];

        let plan = plan_restore(&backup, Some("0001,0003,0005"), &current);
        assert_eq!(
            plan.ops,
            vec![
                RestoreOp::Recreate {
                    original_id: "0003".into(),
                    label: "Old Ubuntu".into(),
                    loader_path: "\\EFI\\ubuntu\\shimx64.efi".into(),
                },
                RestoreOp::SetBootOrder {
                    order: "0001,0003,0005".into()
                },
            ]
        );
        assert_eq!(plan.unrestorable.len(), 1);
        assert_eq!(plan.unrestorable[0].id, "0005");

        // Nothing missing and no recorded order: nothing to do.
        let noop = plan_restore(&backup, None, &backup);
        assert!(noop.ops.is_empty());
        assert!(noop.unrestorable.is_empty());
    }

    #[test]
    fn remap_boot_order_cases() {
        let cases: &[RemapCase] = &[
            // A renamed entry keeps its position rather than being demoted.
            ("0001,0004,0002", &[("0004", "0007")], "0001,0007,0002"),
            // Unmapped ids pass through untouched.
            ("0001,0002", &[("0009", "000A")], "0001,0002"),
            // Whitespace and empty fields are normalized away.
            (" 0001 , ,0002 ", &[], "0001,0002"),
            // Several remaps at once (an undo recreating two entries).
            (
                "0001,0003,0005",
                &[("0003", "000B"), ("0005", "000C")],
                "0001,000B,000C",
            ),
            // Case-insensitive id matching, as efibootmgr prints uppercase.
            ("000a,0001", &[("000A", "0002")], "0002,0001"),
            ("", &[], ""),
        ];
        for (order, remap, expected) in cases {
            let remap: Vec<(String, String)> = remap
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect();
            assert_eq!(remap_boot_order(order, &remap), *expected, "order={order}");
        }
    }

    #[test]
    fn merge_boot_order_cases() {
        // (restored order, current order, expected)
        let cases: &[(&str, &str, &str)] = &[
            // An entry created after the snapshot keeps a place in the
            // order rather than being dropped by the undo.
            ("0001,0003", "0009,0001", "0001,0003,0009"),
            // Nothing new: the restored order wins outright.
            ("0001,0003", "0003,0001", "0001,0003"),
            // Case-insensitive, so a differently-cased id isn't duplicated.
            ("000a,0001", "000A,0002", "000a,0001,0002"),
            // Degenerate inputs.
            ("", "0001,0002", "0001,0002"),
            ("0001", "", "0001"),
            ("", "", ""),
        ];
        for (restored, current, expected) in cases {
            assert_eq!(
                merge_boot_order(restored, current),
                *expected,
                "restored={restored} current={current}"
            );
        }
    }
}
