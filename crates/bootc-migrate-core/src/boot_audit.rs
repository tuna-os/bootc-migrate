//! UEFI boot-entry audit (issue #31): enumerate `efibootmgr` entries and
//! classify them so a caller can show the user what's dead, generic, or a
//! duplicate — before offering to clean anything up.
//!
//! This module is deliberately **read-only**: it parses `efibootmgr -v`
//! output and classifies entries against what's actually present on disk.
//! It does not remove or rename any entry. Per the issue's own safety
//! section ("always dry-run first", "never auto-remove firmware/setup
//! entries", "preserve the rollback escape hatch"), destructive NVRAM/ESP
//! mutation is a separate, much higher-risk piece of work that this module
//! deliberately does not attempt — see the crate's CLI for where the
//! read-only audit is exposed.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// One UEFI boot entry, as parsed from `efibootmgr -v`.
///
/// `Deserialize` is implemented so an NVRAM snapshot taken before a
/// destructive cleanup can be read back (see
/// [`crate::boot_cleanup`]); nothing else round-trips these.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootEntry {
    /// 4-hex-digit id, e.g. `"0001"` (from `Boot0001`).
    pub id: String,
    pub label: String,
    /// Whether the entry is active (`Boot0001*` vs `Boot0001 `).
    pub active: bool,
    /// The `File(...)` loader path, if the entry's device path has one
    /// (firmware-internal entries like PXE/Shell/Setup often don't).
    pub loader_path: Option<String>,
}

/// Why an entry was flagged during the audit. An entry can match more than
/// one reason (e.g. dead AND a duplicate of another dead entry) — callers
/// get the full set, not just the first match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditFlag {
    /// The loader path doesn't exist under the ESP root — almost certainly
    /// a leftover from a wiped install.
    Dead,
    /// Label matches a well-known generic/unbranded name
    /// ("Linux", "Fedora", "UEFI OS", ...) rather than a distro's
    /// `PRETTY_NAME`.
    GenericLabel,
    /// Another entry has the same loader path.
    DuplicateLoaderPath,
    /// A firmware-managed entry (PXE/HTTP boot, EFI Shell, Setup,
    /// removable-media fallback) — never a cleanup candidate, called out
    /// explicitly so a caller's "safe to remove" default excludes it.
    FirmwareManaged,
}

/// One entry plus the flags the audit raised for it, in [`BootEntry::id`]
/// order (i.e. `efibootmgr`'s own listing order).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuditedEntry {
    pub entry: BootEntry,
    pub flags: Vec<AuditFlag>,
}

impl AuditedEntry {
    /// Conservative "safe to pre-select for removal" default (issue #31:
    /// "pre-select only clearly-dead entries"): dead AND not
    /// firmware-managed. Generic labels and duplicates are surfaced but not
    /// pre-selected — renaming/deduplicating needs a human decision this
    /// module doesn't make.
    pub fn safe_to_preselect(&self) -> bool {
        self.flags.contains(&AuditFlag::Dead) && !self.flags.contains(&AuditFlag::FirmwareManaged)
    }
}

/// Labels firmware ships for entries that are never a real OS install.
/// Matched case-insensitively as a substring, since firmware vendors vary
/// capitalization and add suffixes (e.g. "UEFI: Built-in EFI Shell").
const FIRMWARE_LABEL_MARKERS: &[&str] = &[
    "efi shell",
    "pxe",
    "http boot",
    "diagnostic",
    "bios setup",
    "setup",
    "removable media",
    "usb",
    "network",
    "nic",
];

/// Labels that mean "some Linux distro" without saying which — the
/// unbranded names issue #31 wants replaced with the real `PRETTY_NAME`.
const GENERIC_LABEL_MARKERS: &[&str] =
    &["linux", "fedora", "uefi os", "rhel", "centos", "opensuse"];

/// Run `efibootmgr -v` and return its stdout. Kept separate from
/// [`parse_efibootmgr_entries`] so the parser stays free of process I/O
/// (REVIEW.md's parsing/I-O split), and shared so every caller reports the
/// same failure the same way.
pub fn read_efibootmgr_verbose() -> Result<String> {
    let out = std::process::Command::new("efibootmgr")
        .arg("-v")
        .output()
        .context("failed to execute efibootmgr -v")?;
    if !out.status.success() {
        bail!(
            "efibootmgr -v failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Parse `efibootmgr -v` output into structured entries. Malformed lines
/// (not matching `BootXXXX[*] label\t...`) are skipped — an audit must not
/// fail outright on an unexpected firmware quirk.
pub fn parse_efibootmgr_entries(output: &str) -> Vec<BootEntry> {
    let mut entries = Vec::new();
    for line in output.lines() {
        let Some(rest) = line.strip_prefix("Boot") else {
            continue;
        };
        if rest.len() < 5 || !rest[..4].chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        let id = rest[..4].to_string();
        let after_id = &rest[4..];
        let (active, after_marker) = match after_id.strip_prefix('*') {
            Some(r) => (true, r),
            None => (false, after_id.strip_prefix(' ').unwrap_or(after_id)),
        };
        let label_and_path = after_marker.trim_start();
        // Label is everything before the first tab (efibootmgr's own
        // separator between label and device path); if there's no tab, the
        // whole remainder is the label and there's no parseable loader path.
        let (label, path_part) = match label_and_path.split_once('\t') {
            Some((l, p)) => (l.trim().to_string(), Some(p)),
            None => (label_and_path.trim().to_string(), None),
        };
        let loader_path = path_part.and_then(|p| {
            let start = p.find("File(")? + "File(".len();
            let rest = &p[start..];
            let end = rest.find(')')?;
            Some(rest[..end].to_string())
        });
        entries.push(BootEntry {
            id,
            label,
            active,
            loader_path,
        });
    }
    entries
}

/// Whether `entry`'s loader path resolves under `esp_root`. Backslash path
/// separators (as EFI device paths use) are normalized to the host's `/`.
/// Entries with no loader path at all (firmware-internal ones) are treated
/// as not-dead here — [`AuditFlag::Dead`] only applies when there's a path
/// to check and it's missing.
fn loader_path_exists(loader_path: &str, esp_root: &Path) -> bool {
    let rel = loader_path.trim_start_matches('\\').replace('\\', "/");
    esp_root.join(rel).exists()
}

/// Audit every parsed entry against the ESP's actual contents. Duplicate
/// detection compares normalized loader paths across the whole entry set,
/// so it needs all entries at once (not per-entry).
pub fn audit_entries(entries: &[BootEntry], esp_root: &Path) -> Vec<AuditedEntry> {
    use std::collections::HashMap;

    let mut path_counts: HashMap<String, u32> = HashMap::new();
    for e in entries {
        if let Some(p) = &e.loader_path {
            *path_counts.entry(p.to_ascii_lowercase()).or_insert(0) += 1;
        }
    }

    entries
        .iter()
        .map(|entry| {
            let mut flags = Vec::new();
            let label_lower = entry.label.to_ascii_lowercase();

            if FIRMWARE_LABEL_MARKERS
                .iter()
                .any(|m| label_lower.contains(m))
            {
                flags.push(AuditFlag::FirmwareManaged);
            }

            if let Some(path) = &entry.loader_path {
                if !loader_path_exists(path, esp_root) {
                    flags.push(AuditFlag::Dead);
                }
                if path_counts
                    .get(&path.to_ascii_lowercase())
                    .copied()
                    .unwrap_or(0)
                    > 1
                {
                    flags.push(AuditFlag::DuplicateLoaderPath);
                }
            }

            if GENERIC_LABEL_MARKERS
                .iter()
                .any(|m| label_lower.contains(m))
            {
                flags.push(AuditFlag::GenericLabel);
            }

            AuditedEntry {
                entry: entry.clone(),
                flags,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const SAMPLE_OUTPUT: &str = "\
BootCurrent: 0001\n\
Timeout: 1 seconds\n\
BootOrder: 0001,0000,0002,0003,0004\n\
Boot0000* Fedora\tHD(1,GPT,123)/File(\\EFI\\fedora\\shimx64.efi)\n\
Boot0001* Linux Boot Manager\tHD(1,GPT,123)/File(\\EFI\\systemd\\systemd-bootx64.efi)\n\
Boot0002  UEFI OS\tHD(1,GPT,123)/File(\\EFI\\BOOT\\BOOTX64.EFI)\n\
Boot0003  Diagnostics\tVenHw(...)\n\
Boot0004  Windows Boot Manager\tHD(1,GPT,999)/File(\\EFI\\Microsoft\\Boot\\bootmgfw.efi)\n";

    #[test]
    fn parses_active_and_inactive_entries() {
        let entries = parse_efibootmgr_entries(SAMPLE_OUTPUT);
        assert_eq!(entries.len(), 5);
        assert_eq!(entries[0].id, "0000");
        assert_eq!(entries[0].label, "Fedora");
        assert!(entries[0].active);
        assert_eq!(
            entries[0].loader_path.as_deref(),
            Some("\\EFI\\fedora\\shimx64.efi")
        );

        assert_eq!(entries[2].id, "0002");
        assert_eq!(entries[2].label, "UEFI OS");
        assert!(!entries[2].active);
    }

    #[test]
    fn parses_entry_with_no_file_loader_path() {
        let entries = parse_efibootmgr_entries(SAMPLE_OUTPUT);
        let diag = entries.iter().find(|e| e.label == "Diagnostics").unwrap();
        assert_eq!(diag.loader_path, None);
        assert!(!diag.active);
    }

    #[test]
    fn ignores_non_boot_entry_lines() {
        let entries = parse_efibootmgr_entries(SAMPLE_OUTPUT);
        // BootCurrent/Timeout/BootOrder lines must not be mistaken for entries.
        assert!(entries.iter().all(|e| e.id.len() == 4));
    }

    #[test]
    fn audit_flags_dead_entry_whose_loader_is_missing() {
        let esp = tempdir().unwrap();
        // Only systemd-boot's loader actually exists on this ESP.
        std::fs::create_dir_all(esp.path().join("EFI/systemd")).unwrap();
        std::fs::write(esp.path().join("EFI/systemd/systemd-bootx64.efi"), "").unwrap();

        let entries = parse_efibootmgr_entries(SAMPLE_OUTPUT);
        let audited = audit_entries(&entries, esp.path());

        let fedora = audited.iter().find(|a| a.entry.label == "Fedora").unwrap();
        assert!(fedora.flags.contains(&AuditFlag::Dead));
        assert!(fedora.safe_to_preselect());

        let sdboot = audited
            .iter()
            .find(|a| a.entry.label == "Linux Boot Manager")
            .unwrap();
        assert!(!sdboot.flags.contains(&AuditFlag::Dead));
        assert!(!sdboot.safe_to_preselect());
    }

    #[test]
    fn audit_flags_generic_labels() {
        let esp = tempdir().unwrap();
        let entries = parse_efibootmgr_entries(SAMPLE_OUTPUT);
        let audited = audit_entries(&entries, esp.path());

        assert!(
            audited
                .iter()
                .find(|a| a.entry.label == "Fedora")
                .unwrap()
                .flags
                .contains(&AuditFlag::GenericLabel)
        );
        assert!(
            audited
                .iter()
                .find(|a| a.entry.label == "UEFI OS")
                .unwrap()
                .flags
                .contains(&AuditFlag::GenericLabel)
        );
        assert!(
            !audited
                .iter()
                .find(|a| a.entry.label == "Windows Boot Manager")
                .unwrap()
                .flags
                .contains(&AuditFlag::GenericLabel)
        );
    }

    #[test]
    fn audit_flags_firmware_entries_and_never_preselects_them() {
        let esp = tempdir().unwrap();
        let entries = parse_efibootmgr_entries(SAMPLE_OUTPUT);
        let audited = audit_entries(&entries, esp.path());

        let diag = audited
            .iter()
            .find(|a| a.entry.label == "Diagnostics")
            .unwrap();
        assert!(diag.flags.contains(&AuditFlag::FirmwareManaged));
        // Firmware entries have no loader path here, so they're also not
        // flagged Dead — but even if they were, safe_to_preselect excludes
        // FirmwareManaged unconditionally.
        assert!(!diag.safe_to_preselect());
    }

    #[test]
    fn audit_flags_duplicate_loader_paths() {
        let esp = tempdir().unwrap();
        let dup_output = "\
Boot0000* Fedora\tHD(1,GPT,123)/File(\\EFI\\fedora\\shimx64.efi)\n\
Boot0001* Old Fedora Install\tHD(1,GPT,123)/File(\\EFI\\fedora\\shimx64.efi)\n";
        let entries = parse_efibootmgr_entries(dup_output);
        let audited = audit_entries(&entries, esp.path());
        assert!(
            audited
                .iter()
                .all(|a| a.flags.contains(&AuditFlag::DuplicateLoaderPath))
        );
    }

    #[test]
    fn audit_no_duplicate_when_paths_differ() {
        let esp = tempdir().unwrap();
        let entries = parse_efibootmgr_entries(SAMPLE_OUTPUT);
        let audited = audit_entries(&entries, esp.path());
        assert!(
            !audited
                .iter()
                .find(|a| a.entry.label == "Fedora")
                .unwrap()
                .flags
                .contains(&AuditFlag::DuplicateLoaderPath)
        );
    }

    #[test]
    fn empty_output_produces_no_entries() {
        assert!(parse_efibootmgr_entries("").is_empty());
    }
}
