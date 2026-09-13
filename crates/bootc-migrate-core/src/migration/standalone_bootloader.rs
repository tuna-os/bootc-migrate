//! Standalone bootloader conversion without changing the root filesystem backend.

use super::boot::register_systemd_boot_nvram;
use super::boot_deployments::{BootDeployment, enumerate_deployments};
use super::{bootloader, find_esp_or_mount, os_release};
use crate::preflight::SystemInfo;
use anyhow::{Context, Result};
use std::fs;
use std::path::Path;
use std::process::Command;

/// Convert GRUB to systemd-boot or systemd-boot to GRUB without changing the
/// root filesystem backend.
///
/// * `to` — "systemd-boot" or "grub2"
/// * `dry_run` — print actions without executing
/// * `force` — skip readiness gates
/// * `from_image` — optional OCI image reference to extract systemd-boot from
pub fn migrate_bootloader_standalone(
    to: &str,
    dry_run: bool,
    force: bool,
    from_image: Option<&str>,
) -> Result<()> {
    println!("=== Standalone Bootloader Migration ===");
    if dry_run {
        println!("*** DRY RUN MODE — no changes will be made ***");
    }

    let sys = SystemInfo::gather()?;

    // Gate: UEFI + NVRAM writable for systemd-boot.
    if to == "systemd-boot" && !force {
        if !sys.is_uefi {
            anyhow::bail!("System is not booted in UEFI mode; cannot migrate to systemd-boot.");
        }
        if !sys.nvram_writable {
            anyhow::bail!(
                "UEFI NVRAM is not writable; cannot register systemd-boot entry. Use --force to skip."
            );
        }
        if !sys.esp_detected {
            anyhow::bail!(
                "No ESP (EFI System Partition) detected; systemd-boot requires an ESP. \
                 Use --force to skip."
            );
        }
        if sys.esp_free_space_bytes < 50 * 1024 * 1024 {
            anyhow::bail!(
                "ESP has less than 50 MB free ({:.2} MB); \
                 systemd-boot needs space for kernel+initrd. Use --force to skip.",
                sys.esp_free_space_bytes as f64 / (1024.0 * 1024.0)
            );
        }
    }

    // Enumerate current deployments.
    let deployments = enumerate_deployments()?;
    if deployments.is_empty() {
        anyhow::bail!("No bootable deployments found on this system.");
    }
    println!("Found {} deployment(s):", deployments.len());
    for dep in &deployments {
        let kind = if dep.is_composefs {
            "composefs"
        } else {
            "OSTree"
        };
        println!("  - {} {} (kernel {})", kind, dep.checksum, dep.kver);
    }

    match to {
        "systemd-boot" => migrate_to_systemd_boot(&sys, &deployments, dry_run, from_image),
        "grub2" => migrate_to_grub2(&sys, &deployments, dry_run),
        other => anyhow::bail!(
            "Unknown target bootloader '{}'. Use 'systemd-boot' or 'grub2'.",
            other
        ),
    }
}

/// Migrate to systemd-boot: install EFI loader, copy kernel/initrd to ESP,
/// write BLS entries, register NVRAM entry, keep GRUB fallback.
fn migrate_to_systemd_boot(
    _sys: &SystemInfo,
    deps: &[BootDeployment],
    dry_run: bool,
    from_image: Option<&str>,
) -> Result<()> {
    let esp_path = find_esp_or_mount()?;
    println!("ESP mounted at: {}", esp_path);

    // 1. Install systemd-boot EFI binary.
    let esp_path = Path::new(&esp_path);
    let sd_dir = esp_path.join("EFI/systemd");
    let removable_dir = esp_path.join("EFI/BOOT");

    if dry_run {
        println!("[DRY RUN] Would install systemd-bootx64.efi to ESP.");
    } else {
        fs::create_dir_all(&sd_dir)?;
        fs::create_dir_all(&removable_dir)?;

        // Source the systemd-boot binary: from --from-image (registry), or from the
        // running system's /usr/lib/systemd/boot/efi.
        let sd_src = Path::new("/usr/lib/systemd/boot/efi/systemd-bootx64.efi");
        if let Some(img) = from_image {
            println!(
                "Extracting systemd-boot from image '{}' via registry...",
                img
            );
            let sd_dst = sd_dir.join("systemd-bootx64.efi");
            crate::registry::extract_files_via_registry(
                img,
                &[(
                    Path::new("/usr/lib/systemd/boot/efi/systemd-bootx64.efi"),
                    &sd_dst,
                )],
            )
            .context("failed to extract systemd-boot from image")?;
            fs::copy(&sd_dst, removable_dir.join("BOOTX64.EFI"))?;
        } else if sd_src.exists() {
            println!(
                "Copying systemd-boot from running system: {}",
                sd_src.display()
            );
            fs::copy(sd_src, sd_dir.join("systemd-bootx64.efi"))?;
            fs::copy(sd_src, removable_dir.join("BOOTX64.EFI"))?;
        } else {
            anyhow::bail!(
                "systemd-boot binary not found in the running system at {}. \
                 Use --from-image <image> to extract it from a container image.",
                sd_src.display()
            );
        }
        println!("systemd-boot installed to ESP.");
    }

    // 2. Derive entry-token (for $BOOT/<token>/<version> layout).
    let machine_id =
        fs::read_to_string("/etc/machine-id").context("failed to read /etc/machine-id")?;
    let entry_token_file = fs::read_to_string("/etc/kernel/entry-token").ok();
    let entry_token =
        bootloader::systemd_boot::derive_entry_token(entry_token_file.as_deref(), &machine_id);

    // 3. Read live kernel cmdline for karg carry-over.
    let live_cmdline =
        fs::read_to_string("/proc/cmdline").context("failed to read /proc/cmdline")?;

    // 4. Copy kernel+initrd for each deployment to ESP, write BLS entries.
    let entries_dir = esp_path.join("loader/entries");
    let staged_dir = esp_path.join("loader/entries.staged");
    if !dry_run {
        fs::create_dir_all(&entries_dir)?;
    }

    let mut entries: Vec<bootloader::BlsEntry> = Vec::new();

    // Read os-release from the first deployment for naming.
    let os = os_release::read_os_release(&deps[0].root).unwrap_or_else(|_| os_release::OsRelease {
        id: "linux".into(),
        version_id: String::new(),
        name: String::new(),
        pretty_name: String::new(),
    });

    for (i, dep) in deps.iter().enumerate() {
        let priority = i as u32;
        let kind = if dep.is_composefs {
            "composefs"
        } else {
            "ostree"
        };
        let title = format!(
            "{} ({}:{})",
            os_release::bls_entry_title(&os, kind),
            dep.checksum.get(..12).unwrap_or(&dep.checksum),
            priority
        );
        let filename = format!(
            "bootc_{}-{}-{}.conf",
            os.id.replace('-', "_"),
            dep.checksum.get(..12).unwrap_or(&dep.checksum),
            priority
        );
        let sort_key = format!("bootc-{}-{}", os.id, priority);

        if dry_run {
            println!(
                "[DRY RUN] Would copy kernel {} -> ESP/{}/{}/linux",
                dep.vmlinuz.display(),
                entry_token,
                dep.kver
            );
            println!("[DRY RUN] Would write BLS entry: {}", filename);
            continue;
        }

        // Copy kernel + initrd to ESP under <entry-token>/<kver>.
        let esp_kernel_dir = esp_path.join(&entry_token).join(&dep.kver);
        fs::create_dir_all(&esp_kernel_dir)?;
        fs::copy(&dep.vmlinuz, esp_kernel_dir.join("linux"))
            .with_context(|| format!("copying kernel {} to ESP", dep.vmlinuz.display()))?;
        if dep.initrd.exists() {
            fs::copy(&dep.initrd, esp_kernel_dir.join("initrd"))
                .with_context(|| format!("copying initrd {} to ESP", dep.initrd.display()))?;
        }

        // Carry over kargs, stripping composefs= for OSTree entries.
        let strip: &[&str] = if dep.is_composefs {
            &[]
        } else {
            &["composefs="]
        };
        let options = bootloader::systemd_boot::carry_over_kargs(&live_cmdline, strip);

        let entry = bootloader::BlsEntry {
            title,
            version: dep.kver.clone(),
            linux: format!("/{}/{}/linux", entry_token, dep.kver),
            initrds: vec![format!("/{}/{}/initrd", entry_token, dep.kver)],
            options,
            filename: filename.clone(),
            sort_key,
        };
        entries.push(entry);
    }

    if !dry_run && !entries.is_empty() {
        // Write entries atomically via staged directory.
        fs::create_dir_all(&staged_dir)?;
        for entry in &entries {
            fs::write(staged_dir.join(&entry.filename), entry.render())
                .with_context(|| format!("writing staged BLS entry: {}", entry.filename))?;
            fs::rename(
                staged_dir.join(&entry.filename),
                entries_dir.join(&entry.filename),
            )
            .with_context(|| format!("promoting BLS entry: {}", entry.filename))?;
        }

        // Write loader.conf: default to first entry, 3s timeout.
        let default_id = entries[0].filename.trim_end_matches(".conf");
        let loader_conf = format!("default {}\ntimeout 3\nconsole-mode keep\n", default_id);
        fs::write(esp_path.join("loader/loader.conf"), loader_conf)
            .context("failed to write loader.conf")?;

        // Register NVRAM entry.
        register_systemd_boot_nvram(esp_path.to_str().unwrap_or("/boot/efi"));

        println!(
            "{} BLS entr{} written to ESP. GRUB retained as fallback.",
            entries.len(),
            if entries.len() == 1 { "y" } else { "ies" }
        );
    }

    Ok(())
}

/// Migrate to GRUB2: copy kernel/initrd to /boot and write BLS Type 1 entries.
fn migrate_to_grub2(_sys: &SystemInfo, deps: &[BootDeployment], dry_run: bool) -> Result<()> {
    println!("Migrating to GRUB2 (BLS Type 1)...");

    let entries_dir = Path::new("/boot/loader/entries");
    let staged_dir = Path::new("/boot/loader/entries.staged");

    if !dry_run {
        fs::create_dir_all(entries_dir)?;
    }

    let live_cmdline =
        fs::read_to_string("/proc/cmdline").context("failed to read /proc/cmdline")?;

    let os = os_release::read_os_release(&deps[0].root).unwrap_or_else(|_| os_release::OsRelease {
        id: "linux".into(),
        version_id: String::new(),
        name: String::new(),
        pretty_name: String::new(),
    });

    let mut entries: Vec<bootloader::BlsEntry> = Vec::new();

    for (i, dep) in deps.iter().enumerate() {
        let priority = i as u32;
        let boot_dir_name = format!(
            "bootc-{}-{}",
            dep.checksum.get(..12).unwrap_or(&dep.checksum),
            priority
        );
        let boot_dir = Path::new("/boot").join(&boot_dir_name);

        if dry_run {
            println!(
                "[DRY RUN] Would copy kernel/initrd to {} and write BLS entry",
                boot_dir.display()
            );
            continue;
        }

        fs::create_dir_all(&boot_dir)?;
        fs::copy(&dep.vmlinuz, boot_dir.join("vmlinuz"))?;
        if dep.initrd.exists() {
            fs::copy(&dep.initrd, boot_dir.join("initrd"))?;
        }

        let strip: &[&str] = if dep.is_composefs {
            &[]
        } else {
            &["composefs="]
        };
        let options = bootloader::systemd_boot::carry_over_kargs(&live_cmdline, strip);

        let kind = if dep.is_composefs {
            "composefs"
        } else {
            "ostree"
        };
        let title = format!(
            "{} ({}:{})",
            os_release::bls_entry_title(&os, kind),
            dep.checksum.get(..12).unwrap_or(&dep.checksum),
            priority
        );
        let filename = format!(
            "bootc_{}-{}-{}.conf",
            os.id.replace('-', "_"),
            dep.checksum.get(..12).unwrap_or(&dep.checksum),
            priority
        );

        entries.push(bootloader::BlsEntry {
            title,
            version: dep.kver.clone(),
            linux: format!("/{}/vmlinuz", boot_dir_name),
            initrds: vec![format!("/{}/initrd", boot_dir_name)],
            options,
            filename,
            sort_key: format!("bootc-{}-{}", os.id, priority),
        });
    }

    if !dry_run && !entries.is_empty() {
        // Write entries atomically.
        fs::create_dir_all(staged_dir)?;
        for entry in &entries {
            fs::write(staged_dir.join(&entry.filename), entry.render())?;
            fs::rename(
                staged_dir.join(&entry.filename),
                entries_dir.join(&entry.filename),
            )?;
        }

        // Set default GRUB entry.
        let entry_id = entries[0].filename.trim_end_matches(".conf");
        let grubenv = "/boot/grub2/grubenv";
        if Path::new(grubenv).exists() {
            let saved_ok = Command::new("grub2-editenv")
                .args([grubenv, "set", &format!("saved_entry={}", entry_id)])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !saved_ok {
                let _ = Command::new("grub2-set-default").arg(entry_id).status();
            }
            let _ = Command::new("grub2-reboot").arg(entry_id).status();
        }

        // Ensure GRUB_DEFAULT=saved in /etc/default/grub.
        let grub_defaults_path = "/etc/default/grub";
        if let Ok(existing) = fs::read_to_string(grub_defaults_path) {
            let mut new_cfg = String::new();
            let mut found = false;
            for line in existing.lines() {
                if line.starts_with("GRUB_DEFAULT=") {
                    new_cfg.push_str("GRUB_DEFAULT=saved\n");
                    found = true;
                } else {
                    new_cfg.push_str(line);
                    new_cfg.push('\n');
                }
            }
            if !found {
                new_cfg.push_str("GRUB_DEFAULT=saved\n");
            }
            fs::write(grub_defaults_path, &new_cfg)?;
        }

        println!(
            "{} BLS entr{} written to /boot/loader/entries. GRUB default set to {}.",
            entries.len(),
            if entries.len() == 1 { "y" } else { "ies" },
            entry_id
        );
    }

    Ok(())
}
