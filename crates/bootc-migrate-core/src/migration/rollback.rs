//! Subcommand & library helper to automate return to the original OSTree deployment (issue #26).
//!
//! Re-orders UEFI `BootOrder` so the OSTree shim/GRUB boot entry takes priority over systemd-boot.
//! Does not delete the composefs deployment — just changes the default boot entry.

use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::Command;

/// Check prerequisites for rolling back to OSTree.
pub fn verify_rollback_prerequisites() -> Result<()> {
    // 1. UEFI firmware check
    if !Path::new("/sys/firmware/efi").exists() {
        bail!("rollback requires UEFI firmware.");
    }

    // 2. Commit check: /sysroot/ostree (or /ostree) must exist
    if !Path::new("/sysroot/ostree").exists() && !Path::new("/ostree").exists() {
        bail!(
            "/sysroot/ostree not found — commit has already removed the OSTree deployment. \
             Rollback is not possible after commit."
        );
    }

    // 3. Booted state check: must currently be booted into composefs deployment
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    if !cmdline.contains("composefs=") {
        bail!("already booted on OSTree — nothing to rollback.");
    }

    // 4. OSTree BLS entry check: /boot/loader/entries/ostree-*.conf must exist
    let bls_dir = Path::new("/boot/loader/entries");
    let has_ostree_bls = if bls_dir.is_dir() {
        std::fs::read_dir(bls_dir)
            .ok()
            .map(|entries| {
                entries.flatten().any(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    name.starts_with("ostree-") && name.ends_with(".conf")
                })
            })
            .unwrap_or(false)
    } else {
        false
    };

    if !has_ostree_bls {
        bail!("No OSTree BLS entry found under /boot/loader/entries/ostree-*.conf.");
    }

    Ok(())
}

/// Substrings that identify the pre-migration OSTree/GRUB boot entry — the
/// rollback escape hatch this module exists to restore. Matched
/// case-insensitively against an entry's label and its loader path.
///
/// Shared with [`crate::boot_cleanup`], which must never let a boot-entry
/// cleanup delete the very entry `run_rollback` would re-order back to the
/// front; both therefore have to agree on what "the rollback entry" is.
pub const OSTREE_ROLLBACK_MARKERS: &[&str] = &["fedora", "shim", "efi\\fedora", "grub"];

/// Whether an entry's label (and loader path, when it has one) identifies
/// it as the OSTree/GRUB rollback entry.
pub fn is_ostree_rollback_entry(label: &str, loader_path: Option<&str>) -> bool {
    let haystack = match loader_path {
        Some(p) => format!("{label} {p}"),
        None => label.to_string(),
    }
    .to_ascii_lowercase();
    OSTREE_ROLLBACK_MARKERS.iter().any(|m| haystack.contains(m))
}

/// Parse output of `efibootmgr` or `efibootmgr -v` to locate the OSTree/Fedora boot entry ID.
/// Searches for lines starting with `BootXXXX` matching any of
/// [`OSTREE_ROLLBACK_MARKERS`].
pub fn parse_ostree_boot_entry_id(efibootmgr_output: &str) -> Option<String> {
    for line in efibootmgr_output.lines() {
        let line_trim = line.trim();
        if let Some(rest) = line_trim.strip_prefix("Boot")
            && rest.len() >= 4
            && rest[..4].chars().all(|c| c.is_ascii_hexdigit())
        {
            let id = &rest[..4];
            let line_lower = line_trim[8..].to_ascii_lowercase();
            if OSTREE_ROLLBACK_MARKERS
                .iter()
                .any(|m| line_lower.contains(m))
            {
                return Some(id.to_string());
            }
        }
    }
    None
}

/// Parse current `BootOrder: XXXX,YYYY,...` line from `efibootmgr` output.
pub fn parse_boot_order(efibootmgr_output: &str) -> Option<String> {
    parse_nvram_scalar(efibootmgr_output, "BootOrder:")
}

/// Parse the `BootCurrent: XXXX` line from `efibootmgr` output — the entry
/// the running system actually booted from, which no cleanup may delete.
pub fn parse_boot_current(efibootmgr_output: &str) -> Option<String> {
    parse_nvram_scalar(efibootmgr_output, "BootCurrent:")
}

fn parse_nvram_scalar(efibootmgr_output: &str, key: &str) -> Option<String> {
    for line in efibootmgr_output.lines() {
        let line_trim = line.trim();
        if let Some(value) = line_trim.strip_prefix(key) {
            return Some(value.trim().to_string());
        }
    }
    None
}

/// Build new `BootOrder` putting `target_id` first.
pub fn build_new_boot_order(current_order: &str, target_id: &str) -> String {
    let mut ids: Vec<String> = vec![target_id.to_string()];
    for id in current_order.split(',') {
        let id_trim = id.trim();
        if !id_trim.is_empty() && !id_trim.eq_ignore_ascii_case(target_id) {
            ids.push(id_trim.to_string());
        }
    }
    ids.join(",")
}

/// Execute rollback: re-orders UEFI BootOrder to prioritize OSTree deployment.
pub fn run_rollback(reboot: bool, dry_run: bool) -> Result<()> {
    if !dry_run {
        verify_rollback_prerequisites()?;
    }

    if dry_run {
        println!("*** DRY RUN MODE — no changes will be made ***");
    }

    println!("Checking UEFI NVRAM boot entries...");

    let output = Command::new("efibootmgr")
        .arg("-v")
        .output()
        .context("failed to invoke efibootmgr")?;

    if !output.status.success() {
        bail!(
            "efibootmgr failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let txt = String::from_utf8_lossy(&output.stdout);
    let target_id = parse_ostree_boot_entry_id(&txt);

    let target_id = match target_id {
        Some(id) => id,
        None => {
            println!("No existing Fedora entry found in NVRAM; attempting to re-register...");
            if !dry_run {
                let status = Command::new("efibootmgr")
                    .args([
                        "--create",
                        "--label",
                        "Fedora",
                        "--loader",
                        "\\EFI\\fedora\\shimx64.efi",
                    ])
                    .status()
                    .context("failed to execute efibootmgr --create")?;
                if !status.success() {
                    bail!("Failed to re-register Fedora boot entry in UEFI NVRAM via efibootmgr.");
                }
            } else {
                println!(
                    "[DRY RUN] Would execute: efibootmgr --create --label Fedora --loader \\EFI\\fedora\\shimx64.efi"
                );
            }
            "0000".to_string()
        }
    };

    let current_order = parse_boot_order(&txt).unwrap_or_default();
    let new_order = build_new_boot_order(&current_order, &target_id);

    println!("Reordering UEFI BootOrder to prioritize entry {target_id} (Fedora/OSTree)...");
    if dry_run {
        println!("[DRY RUN] Would execute: efibootmgr --bootorder {new_order}");
    } else {
        let status = Command::new("efibootmgr")
            .args(["--bootorder", &new_order])
            .status()
            .context("failed to execute efibootmgr --bootorder")?;
        if !status.success() {
            bail!("Failed to set UEFI BootOrder via efibootmgr.");
        }
        println!("Successfully set UEFI BootOrder to: {new_order}");
    }

    if reboot {
        println!("Triggering reboot into OSTree deployment...");
        if dry_run {
            println!("[DRY RUN] Would execute: systemctl reboot");
        } else {
            let status = Command::new("systemctl")
                .arg("reboot")
                .status()
                .context("failed to execute systemctl reboot")?;
            if !status.success() {
                bail!("Failed to trigger systemctl reboot.");
            }
        }
    } else {
        println!(
            "Reboot now to return to Bluefin OSTree. \
             Run bootc-migrate commit when ready to finalize."
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_ostree_boot_entry_id() {
        let sample = "\
BootCurrent: 0001\n\
Timeout: 1 seconds\n\
BootOrder: 0001,0000,0002\n\
Boot0000* Fedora\tHD(1,GPT,123)/File(\\EFI\\fedora\\shimx64.efi)\n\
Boot0001* Linux Boot Manager\tHD(1,GPT,123)/File(\\EFI\\systemd\\systemd-bootx64.efi)\n\
";
        assert_eq!(parse_ostree_boot_entry_id(sample), Some("0000".to_string()));
    }

    #[test]
    fn test_parse_boot_order() {
        let sample = "\
BootCurrent: 0001\n\
Timeout: 1 seconds\n\
BootOrder: 0001,0000,0002\n\
";
        assert_eq!(parse_boot_order(sample), Some("0001,0000,0002".to_string()));
    }

    #[test]
    fn test_parse_boot_current() {
        let sample = "\
BootCurrent: 0001\n\
Timeout: 1 seconds\n\
BootOrder: 0001,0000,0002\n\
";
        assert_eq!(parse_boot_current(sample), Some("0001".to_string()));
        assert_eq!(parse_boot_current("BootOrder: 0001\n"), None);
    }

    #[test]
    fn test_build_new_boot_order() {
        assert_eq!(
            build_new_boot_order("0001,0000,0002", "0000"),
            "0000,0001,0002"
        );
        assert_eq!(build_new_boot_order("0000,0001", "0000"), "0000,0001");
        assert_eq!(build_new_boot_order("", "0000"), "0000");
    }

    #[test]
    fn is_ostree_rollback_entry_cases() {
        // (label, loader_path, expected) — the rollback entry has to be
        // recognizable from either half, since firmware labels vary.
        let cases: &[(&str, Option<&str>, bool)] = &[
            ("Fedora", Some("\\EFI\\fedora\\shimx64.efi"), true),
            // Label alone is generic, but the loader path names shim.
            ("UEFI OS", Some("\\EFI\\fedora\\shimx64.efi"), true),
            // Loader path alone is generic, but the label names GRUB.
            ("GRUB2 bootloader", Some("\\EFI\\BOOT\\BOOTX64.EFI"), true),
            // No loader path at all: label still decides.
            ("Fedora", None, true),
            // systemd-boot is the *migrated* entry, never the rollback one.
            (
                "Linux Boot Manager",
                Some("\\EFI\\systemd\\systemd-bootx64.efi"),
                false,
            ),
            (
                "Windows Boot Manager",
                Some("\\EFI\\Microsoft\\Boot\\bootmgfw.efi"),
                false,
            ),
            ("UEFI OS", Some("\\EFI\\BOOT\\BOOTX64.EFI"), false),
            // Case-insensitive on both halves.
            ("FEDORA LINUX", None, true),
        ];
        for (label, loader, expected) in cases {
            assert_eq!(
                is_ostree_rollback_entry(label, *loader),
                *expected,
                "label={label} loader={loader:?}"
            );
        }
    }
}
