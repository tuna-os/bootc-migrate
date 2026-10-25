//! CLI adapter for `bootc-rebase scan`: dispatch plus the human-readable
//! capability table.
//!
//! Presentation only. Capability discovery and every compatibility decision
//! stay in `bootc_migrate_core::scan` — this module must not grow scanning
//! policy, or the CLI and the engine can disagree about what a target
//! supports (#158).

use anyhow::Result;

use bootc_migrate_core::hardware as hw;

use crate::ScanArgs;

/// Scan the target image and render the result as JSON or a table.
pub fn run(args: &ScanArgs) -> Result<()> {
    if args.json {
        eprintln!("Scanning target image {}...", args.image);
    } else {
        println!("Scanning target image {}...", args.image);
    }
    let caps = bootc_migrate_core::scan::scan_target_image(&args.image)?;
    let software = bootc_migrate_core::software_report::gather_live(&caps.software, &args.image)?;
    // `host` reports this machine (issue #371) when it is itself the
    // package-managed source `scan` is being run ahead of; `installed_package_count`
    // reuses the count `software` already paid to read, rather than reading
    // the package database a second time just to count it.
    let host = bootc_migrate_core::host_overview::gather(
        software
            .as_ref()
            .map(|report| report.installed_package_count),
    );
    // The hardware check compares *this* machine with the image, so it is
    // only meaningful where the scan runs on the machine to be migrated.
    let hardware = caps
        .kernel
        .as_ref()
        .map(|k| hw::assess(&hw::read_host_hardware(), k));
    if args.json {
        let mut value = serde_json::to_value(&caps)?;
        value["hardware"] = serde_json::to_value(&hardware)?;
        value["software_report"] = serde_json::to_value(&software)?;
        value["host"] = serde_json::to_value(&host)?;
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        if let Some(host) = &host {
            print_host_table(host);
            println!();
        }
        print_capabilities_table(&args.image, &caps);
        println!();
        match &hardware {
            Some(report) => print!("{}", hw::render(report, &args.image)),
            None => print!(
                "{}",
                hw::render_unknown(&args.image, "the target image ships no kernel modules")
            ),
        }
        if let Some(report) = software {
            println!();
            print!("{}", report.to_markdown());
        }
    }
    Ok(())
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
    let target_loaders = caps
        .target_bootloaders
        .iter()
        .map(|loader| {
            let authorities = if loader.authorities.is_empty() {
                "unrecognized signer".to_string()
            } else {
                format!("UEFI CA {}", loader.authorities.join("/"))
            };
            format!("{} ({authorities})", loader.path)
        })
        .collect::<Vec<_>>();
    println!(
        "Target EFI loaders:  {}",
        if target_loaders.is_empty() {
            "none".to_string()
        } else {
            target_loaders.join(", ")
        }
    );
    println!(
        "bootc present:         {}",
        if caps.bootc_present { "yes" } else { "no" }
    );
    println!(
        "bootupd present:       {}",
        if caps.bootupd_present { "yes" } else { "no" }
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
    println!(
        "Kernel:                {}",
        caps.kernel_version
            .as_deref()
            .unwrap_or("no kernel modules found")
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

/// This host's own facts (issue #371), when `scan` is run ahead of a
/// migration from this same package-managed machine. Independent of the
/// target image above — `package_host::assess` is the pass/fail gate this
/// feeds into once a real run starts; this is purely informational.
fn print_host_table(host: &bootc_migrate_core::host_overview::HostOverview) {
    use bootc_migrate_core::package_host::SelinuxMode;

    let row = |label: &str, value: String| println!("{label:<24}{value}");

    println!("=== Host overview ===");
    match &host.distro {
        Some(distro) => row(
            "Distribution:",
            format!("{} ({})", distro.pretty_name, distro.version_id),
        ),
        None => row(
            "Distribution:",
            "unknown (/etc/os-release unreadable)".to_string(),
        ),
    }
    row(
        "Family:",
        host.distro
            .as_ref()
            .and_then(|distro| distro.pkg_family)
            .map(|family| family.label().to_string())
            .unwrap_or_else(|| "unknown".to_string()),
    );
    row(
        "Package manager:",
        host.facts
            .package_db
            .map(|db| db.to_string())
            .unwrap_or_else(|| "unknown".to_string()),
    );
    row(
        "Installed packages:",
        host.installed_package_count
            .map(|count| count.to_string())
            .unwrap_or_else(|| "unknown".to_string()),
    );
    row(
        "Boot mode:",
        if host.facts.uefi { "UEFI" } else { "BIOS" }.to_string(),
    );
    row(
        "Secure Boot:",
        match host.facts.secure_boot {
            Some(true) => "enabled",
            Some(false) => "disabled",
            None if host.facts.uefi => "unknown (efivar unreadable)",
            None => "not applicable (BIOS)",
        }
        .to_string(),
    );
    row(
        "SELinux:",
        match host.facts.selinux {
            SelinuxMode::Disabled => "disabled",
            SelinuxMode::Permissive => "permissive",
            SelinuxMode::Enforcing => "enforcing",
        }
        .to_string(),
    );
    row(
        "Root filesystem:",
        host.facts
            .root
            .as_ref()
            .map(format_host_mount)
            .unwrap_or_else(|| "unknown".to_string()),
    );
    row("/home:", format_host_submount(&host.facts.home));
    row("/boot:", format_host_submount(&host.facts.boot));
    row("/var:", format_host_submount(&host.facts.var));
}

/// `None`: a directory on the root filesystem rather than a separate mount.
fn format_host_submount(mount: &Option<bootc_migrate_core::package_host::MountFacts>) -> String {
    mount
        .as_ref()
        .map(format_host_mount)
        .unwrap_or_else(|| "part of the root filesystem".to_string())
}

fn format_host_mount(mount: &bootc_migrate_core::package_host::MountFacts) -> String {
    let mut parts = vec![mount.fs_type.clone()];
    if let Some(subvol) = &mount.btrfs_subvol {
        parts.push(format!("subvol={subvol}"));
    }
    let mut layers = Vec::new();
    if mount.on_luks {
        layers.push("LUKS");
    }
    if mount.on_lvm {
        layers.push("LVM");
    }
    if !layers.is_empty() {
        parts.push(format!("on {}", layers.join("+")));
    }
    parts.join(", ")
}
