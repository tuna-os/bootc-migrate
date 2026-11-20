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
    for line in capabilities_table_lines(image, caps) {
        println!("{line}");
    }
}

/// The capability table, one entry per output line, so tests can assert on it.
fn capabilities_table_lines(
    image: &str,
    caps: &bootc_migrate_core::scan::Capabilities,
) -> Vec<String> {
    let yes_no = |value: bool| if value { "yes" } else { "no" };
    let mut lines = vec![
        "=== Target image capabilities ===".to_string(),
        format!("Image:                 {image}"),
        format!(
            "Composefs:             {}",
            if caps.composefs_capable {
                "capable"
            } else {
                "not enabled in prepare-root.conf"
            }
        ),
        format!("OSTree capable:        {}", yes_no(caps.ostree_capable)),
        format!(
            "Bootloader payload:    {}",
            if caps.systemd_boot_payload {
                "systemd-boot ✓"
            } else {
                "none"
            }
        ),
    ];
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
    lines.push(format!(
        "Target EFI loaders:  {}",
        if target_loaders.is_empty() {
            "none".to_string()
        } else {
            target_loaders.join(", ")
        }
    ));
    lines.push(format!(
        "bootc present:         {}",
        yes_no(caps.bootc_present)
    ));
    lines.push(format!(
        "bootupd present:       {}",
        yes_no(caps.bootupd_present)
    ));
    lines.push(format!(
        "Desktops:              {}",
        if caps.desktops.is_empty() {
            "none".to_string()
        } else {
            caps.desktops.join(", ")
        }
    ));
    if let Some(base) = &caps.base {
        lines.push(format!(
            "Base OS:               {} {}",
            base.id,
            base.version_id.as_deref().unwrap_or("")
        ));
    } else {
        lines.push("Base OS:               unknown".to_string());
    }
    lines.push(format!(
        "Sysusers:              {} static allocation(s)",
        caps.sysusers.len()
    ));
    lines.push(format!(
        "Transient root/etc:    {} / {}",
        yes_no(caps.root_transient),
        yes_no(caps.etc_transient)
    ));
    lines.push(format!(
        "fs-verity required:    {}",
        yes_no(caps.fs_verity_required)
    ));
    lines.push(format!(
        "Initramfs composefs:   {}",
        if caps.initramfs_has_composefs_module {
            "module present"
        } else {
            "not present (may need regeneration for a composefs boot)"
        }
    ));
    lines.push(format!(
        "Filesystem expected:   {}",
        caps.filesystem_expectation.as_deref().unwrap_or("unknown")
    ));
    lines.push(format!(
        "Kernel:                {}",
        caps.kernel_version
            .as_deref()
            .unwrap_or("no kernel modules found")
    ));
    let issues = bootc_migrate_core::scan::compatibility_issues(caps);
    lines.push(format!(
        "Compatible:            {}",
        if issues.is_empty() { "YES" } else { "NO" }
    ));
    lines.extend(issues.iter().map(|issue| format!("  - {issue}")));
    lines
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

#[cfg(test)]
mod tests {
    use super::*;
    use bootc_migrate_core::package_host::MountFacts;
    use bootc_migrate_core::preflight::{TargetBootloader, TargetBootloaderKind};
    use bootc_migrate_core::scan::{ProbeFiles, assemble};

    fn table(probe: &ProbeFiles) -> Vec<String> {
        capabilities_table_lines("ghcr.io/example/image:latest", &assemble(probe))
    }

    /// The value printed after `label`, or a panic naming the missing row.
    fn row<'a>(lines: &'a [String], label: &str) -> &'a str {
        lines
            .iter()
            .find_map(|line| line.strip_prefix(label))
            .unwrap_or_else(|| panic!("no {label:?} row in {lines:#?}"))
            .trim()
    }

    fn capable_probe() -> ProbeFiles {
        ProbeFiles {
            os_release: Some("ID=fedora\nVERSION_ID=43\n".to_string()),
            prepare_root: Some("[composefs]\nenabled = maybe\n[root]\ntransient = true\n".into()),
            sysusers: vec!["u foo 123\ng bar 456\n".to_string()],
            session_files: vec!["gnome.desktop".to_string()],
            has_systemd_boot_payload: true,
            target_bootloaders: vec![
                TargetBootloader {
                    kind: TargetBootloaderKind::Shim,
                    path: "usr/lib/shim/shimx64.efi".to_string(),
                    signature_present: true,
                    authorities: vec!["2011".to_string(), "2023".to_string()],
                    inspection_error: None,
                },
                TargetBootloader {
                    kind: TargetBootloaderKind::SystemdBoot,
                    path: "usr/lib/systemd/boot/efi/systemd-bootx64.efi".to_string(),
                    signature_present: false,
                    authorities: vec![],
                    inspection_error: None,
                },
            ],
            has_bootc: true,
            has_bootupd: true,
            dracut_module_dir_names: vec!["90composefs".to_string()],
            bootc_install_config: Some("[install.filesystem.root]\ntype = \"xfs\"\n".into()),
            ..Default::default()
        }
    }

    #[test]
    fn capable_image_reports_every_capability_and_is_compatible() {
        let lines = table(&capable_probe());
        assert_eq!(lines[0], "=== Target image capabilities ===");
        let expected: &[(&str, &str)] = &[
            ("Image:", "ghcr.io/example/image:latest"),
            ("Composefs:", "capable"),
            ("OSTree capable:", "yes"),
            ("Bootloader payload:", "systemd-boot ✓"),
            (
                "Target EFI loaders:",
                "usr/lib/shim/shimx64.efi (UEFI CA 2011/2023), \
                 usr/lib/systemd/boot/efi/systemd-bootx64.efi (unrecognized signer)",
            ),
            ("bootc present:", "yes"),
            ("bootupd present:", "yes"),
            ("Desktops:", "gnome"),
            ("Base OS:", "fedora 43"),
            ("Sysusers:", "2 static allocation(s)"),
            ("Transient root/etc:", "yes / no"),
            ("fs-verity required:", "yes"),
            ("Initramfs composefs:", "module present"),
            ("Compatible:", "YES"),
        ];
        for (label, want) in expected {
            assert_eq!(row(&lines, label), *want, "row {label:?}");
        }
        assert_eq!(
            lines.last().map(String::as_str),
            Some("Compatible:            YES"),
            "a compatible image lists no issues after the verdict"
        );
    }

    #[test]
    fn empty_probe_reports_absent_capabilities_and_lists_the_issue() {
        let lines = table(&ProbeFiles::default());
        let expected: &[(&str, &str)] = &[
            ("Composefs:", "not enabled in prepare-root.conf"),
            ("OSTree capable:", "no"),
            ("Bootloader payload:", "none"),
            ("Target EFI loaders:", "none"),
            ("bootc present:", "no"),
            ("bootupd present:", "no"),
            ("Desktops:", "none"),
            ("Base OS:", "unknown"),
            ("Sysusers:", "0 static allocation(s)"),
            ("Transient root/etc:", "no / no"),
            ("fs-verity required:", "no"),
            (
                "Initramfs composefs:",
                "not present (may need regeneration for a composefs boot)",
            ),
            ("Filesystem expected:", "unknown"),
            ("Kernel:", "no kernel modules found"),
            ("Compatible:", "NO"),
        ];
        for (label, want) in expected {
            assert_eq!(row(&lines, label), *want, "row {label:?}");
        }
        let verdict = lines
            .iter()
            .position(|line| line.starts_with("Compatible:"))
            .expect("verdict row");
        assert_eq!(
            lines[verdict + 1..],
            [
                "  - no usr/lib/ostree/prepare-root.conf found — not an ostree/bootc image"
                    .to_string()
            ],
            "every compatibility issue is listed right after the verdict"
        );
    }

    #[test]
    fn base_without_version_id_prints_the_id_alone() {
        let probe = ProbeFiles {
            os_release: Some("ID=arch\n".to_string()),
            ..capable_probe()
        };
        assert_eq!(row(&table(&probe), "Base OS:"), "arch");
    }

    #[test]
    fn host_mounts_name_subvolume_and_device_layers() {
        let mount = |fs: &str, subvol: Option<&str>, luks, lvm| MountFacts {
            fs_type: fs.to_string(),
            btrfs_subvol: subvol.map(str::to_string),
            on_luks: luks,
            on_lvm: lvm,
            ..Default::default()
        };
        let cases = [
            (mount("ext4", None, false, false), "ext4"),
            (
                mount("btrfs", Some("root"), false, false),
                "btrfs, subvol=root",
            ),
            (mount("xfs", None, true, false), "xfs, on LUKS"),
            (mount("xfs", None, true, true), "xfs, on LUKS+LVM"),
            (
                mount("btrfs", Some("home"), false, true),
                "btrfs, subvol=home, on LVM",
            ),
        ];
        for (facts, want) in cases {
            assert_eq!(format_host_mount(&facts), want);
        }
    }

    #[test]
    fn host_submount_falls_back_to_the_root_filesystem() {
        assert_eq!(format_host_submount(&None), "part of the root filesystem");
        let var = MountFacts {
            fs_type: "xfs".to_string(),
            ..Default::default()
        };
        assert_eq!(format_host_submount(&Some(var)), "xfs");
    }
}
