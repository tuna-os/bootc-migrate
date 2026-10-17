//! CLI adapter for `bootc-rebase scan`: dispatch plus the human-readable
//! capability table.
//!
//! Presentation only. Capability discovery and every compatibility decision
//! stay in `bootc_migrate_core::scan` — this module must not grow scanning
//! policy, or the CLI and the engine can disagree about what a target
//! supports (#158).

use anyhow::Result;
use std::io::Write;

use bootc_migrate_core::hardware as hw;

use crate::ScanArgs;

/// Scan the target image and render the result as JSON or a table.
pub fn run(args: &ScanArgs) -> Result<()> {
    println!("Scanning target image {}...", args.image);
    let caps = bootc_migrate_core::scan::scan_target_image(&args.image)?;
    // The hardware check compares *this* machine with the image, so it is
    // only meaningful where the scan runs on the machine to be migrated.
    let hardware = caps
        .kernel
        .as_ref()
        .map(|k| hw::assess(&hw::read_host_hardware(), k));
    if args.json {
        let mut value = serde_json::to_value(&caps)?;
        value["hardware"] = serde_json::to_value(&hardware)?;
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        let stdout = std::io::stdout();
        let mut stdout = stdout.lock();
        print_capabilities_table(&mut stdout, &args.image, &caps)?;
        stdout.flush()?;
        drop(stdout);
        println!();
        match &hardware {
            Some(report) => print!("{}", hw::render(report, &args.image)),
            None => print!(
                "{}",
                hw::render_unknown(&args.image, "the target image ships no kernel modules")
            ),
        }
    }
    Ok(())
}

fn print_capabilities_table(
    out: &mut impl Write,
    image: &str,
    caps: &bootc_migrate_core::scan::Capabilities,
) -> std::io::Result<()> {
    writeln!(out, "=== Target image capabilities ===")?;
    writeln!(out, "Image:                 {image}")?;
    writeln!(
        out,
        "Composefs:             {}",
        if caps.composefs_capable {
            "capable"
        } else {
            "not enabled in prepare-root.conf"
        }
    )?;
    writeln!(
        out,
        "OSTree capable:        {}",
        if caps.ostree_capable { "yes" } else { "no" }
    )?;
    writeln!(
        out,
        "Bootloader payload:    {}",
        if caps.systemd_boot_payload {
            "systemd-boot ✓"
        } else {
            "none"
        }
    )?;
    writeln!(
        out,
        "bootc present:         {}",
        if caps.bootc_present { "yes" } else { "no" }
    )?;
    writeln!(
        out,
        "bootupd present:       {}",
        if caps.bootupd_present { "yes" } else { "no" }
    )?;
    writeln!(
        out,
        "Desktops:              {}",
        if caps.desktops.is_empty() {
            "none".to_string()
        } else {
            caps.desktops.join(", ")
        }
    )?;
    if let Some(base) = &caps.base {
        writeln!(
            out,
            "Base OS:               {} {}",
            base.id,
            base.version_id.as_deref().unwrap_or("")
        )?;
    } else {
        writeln!(out, "Base OS:               unknown")?;
    }
    writeln!(
        out,
        "Sysusers:              {} static allocation(s)",
        caps.sysusers.len()
    )?;
    writeln!(
        out,
        "Transient root/etc:    {} / {}",
        if caps.root_transient { "yes" } else { "no" },
        if caps.etc_transient { "yes" } else { "no" }
    )?;
    writeln!(
        out,
        "fs-verity required:    {}",
        if caps.fs_verity_required { "yes" } else { "no" }
    )?;
    writeln!(
        out,
        "Initramfs composefs:   {}",
        if caps.initramfs_has_composefs_module {
            "module present"
        } else {
            "not present (may need regeneration for a composefs boot)"
        }
    )?;
    writeln!(
        out,
        "Filesystem expected:   {}",
        caps.filesystem_expectation.as_deref().unwrap_or("unknown")
    )?;
    writeln!(
        out,
        "Kernel:                {}",
        caps.kernel_version
            .as_deref()
            .unwrap_or("no kernel modules found")
    )?;
    let issues = bootc_migrate_core::scan::compatibility_issues(caps);
    writeln!(
        out,
        "Compatible:            {}",
        if issues.is_empty() { "YES" } else { "NO" }
    )?;
    for issue in &issues {
        writeln!(out, "  - {issue}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bootc_migrate_core::scan::{BaseInfo, Capabilities};

    fn render(caps: &Capabilities, image: &str) -> String {
        let mut output = Vec::new();
        print_capabilities_table(&mut output, image, caps).expect("Vec writer cannot fail");
        String::from_utf8(output).expect("table output is UTF-8")
    }

    #[test]
    fn table_renders_representative_capability_reports() {
        let cases = [
            (
                "all-capable",
                Capabilities {
                    composefs_capable: true,
                    ostree_capable: true,
                    systemd_boot_payload: true,
                    bootc_present: true,
                    bootupd_present: true,
                    desktops: vec!["gnome".into(), "kde".into()],
                    base: Some(BaseInfo {
                        id: "fedora".into(),
                        id_like: Some("rhel fedora".into()),
                        version_id: Some("42".into()),
                        pkg_family: None,
                    }),
                    sysusers: Vec::new(),
                    fs_verity_required: true,
                    root_transient: true,
                    etc_transient: false,
                    initramfs_has_composefs_module: true,
                    filesystem_expectation: Some("xfs".into()),
                    kernel_version: Some("6.12.1".into()),
                    kernel: None,
                },
                "=== Target image capabilities ===\n\
Image:                 quay.io/example/target:stable\n\
Composefs:             capable\n\
OSTree capable:        yes\n\
Bootloader payload:    systemd-boot ✓\n\
bootc present:         yes\n\
bootupd present:       yes\n\
Desktops:              gnome, kde\n\
Base OS:               fedora 42\n\
Sysusers:              0 static allocation(s)\n\
Transient root/etc:    yes / no\n\
fs-verity required:    yes\n\
Initramfs composefs:   module present\n\
Filesystem expected:   xfs\n\
Kernel:                6.12.1\n\
Compatible:            YES\n",
            ),
            (
                "all-absent",
                Capabilities {
                    composefs_capable: false,
                    ostree_capable: false,
                    systemd_boot_payload: false,
                    bootc_present: false,
                    bootupd_present: false,
                    desktops: Vec::new(),
                    base: None,
                    sysusers: Vec::new(),
                    fs_verity_required: false,
                    root_transient: false,
                    etc_transient: true,
                    initramfs_has_composefs_module: false,
                    filesystem_expectation: None,
                    kernel_version: None,
                    kernel: None,
                },
                "=== Target image capabilities ===\n\
Image:                 scratch:latest\n\
Composefs:             not enabled in prepare-root.conf\n\
OSTree capable:        no\n\
Bootloader payload:    none\n\
bootc present:         no\n\
bootupd present:       no\n\
Desktops:              none\n\
Base OS:               unknown\n\
Sysusers:              0 static allocation(s)\n\
Transient root/etc:    no / yes\n\
fs-verity required:    no\n\
Initramfs composefs:   not present (may need regeneration for a composefs boot)\n\
Filesystem expected:   unknown\n\
Kernel:                no kernel modules found\n\
Compatible:            NO\n\
  - no usr/lib/ostree/prepare-root.conf found — not an ostree/bootc image\n",
            ),
        ];

        for (name, caps, expected) in cases {
            let image = if name == "all-capable" {
                "quay.io/example/target:stable"
            } else {
                "scratch:latest"
            };
            assert_eq!(render(&caps, image), expected, "{name}");
        }
    }
}
