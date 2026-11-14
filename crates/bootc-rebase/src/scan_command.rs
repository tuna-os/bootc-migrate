//! CLI adapter for `bootc-rebase scan`: dispatch plus the human-readable
//! capability table.
//!
//! Presentation only. Capability discovery and every compatibility decision
//! stay in `bootc_migrate_core::scan` — this module must not grow scanning
//! policy, or the CLI and the engine can disagree about what a target
//! supports (#158).

use std::io::{self, Write};

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
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        write_capabilities_table(&mut io::stdout().lock(), &args.image, &caps)?;
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

fn write_capabilities_table(
    out: &mut impl Write,
    image: &str,
    caps: &bootc_migrate_core::scan::Capabilities,
) -> io::Result<()> {
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
    writeln!(
        out,
        "Target EFI loaders:  {}",
        if target_loaders.is_empty() {
            "none".to_string()
        } else {
            target_loaders.join(", ")
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
    use bootc_migrate_core::preflight::secure_boot::{TargetBootloader, TargetBootloaderKind};
    use bootc_migrate_core::scan::{ProbeFiles, assemble};

    fn render(probe: &ProbeFiles) -> String {
        let caps = assemble(probe);
        let mut out = Vec::new();
        write_capabilities_table(&mut out, "ghcr.io/example/img:stable", &caps)
            .expect("writing to a Vec cannot fail");
        String::from_utf8(out).expect("table is UTF-8")
    }

    fn loader(path: &str, authorities: &[&str]) -> TargetBootloader {
        TargetBootloader {
            kind: TargetBootloaderKind::Shim,
            path: path.to_string(),
            signature_present: !authorities.is_empty(),
            authorities: authorities.iter().map(|a| a.to_string()).collect(),
            inspection_error: None,
        }
    }

    #[test]
    fn table_for_fully_capable_image() {
        let table = render(&ProbeFiles {
            os_release: Some("ID=fedora\nVERSION_ID=44\n".into()),
            prepare_root: Some(
                "[composefs]\nenabled = maybe\n[root]\ntransient = true\n[etc]\ntransient = true\n"
                    .into(),
            ),
            sysusers: vec!["u dnsmasq 983 - -\n".into()],
            session_files: vec!["gnome.desktop".into(), "plasma.desktop".into()],
            has_systemd_boot_payload: true,
            target_bootloaders: vec![loader("usr/lib/bootupd/shimx64.efi", &["2011", "2023"])],
            has_bootc: true,
            has_bootupd: true,
            dracut_module_dir_names: vec!["37composefs".into()],
            bootc_install_config: Some("root-fs = \"xfs\"\n".into()),
            ..Default::default()
        });
        let expected = "\
=== Target image capabilities ===
Image:                 ghcr.io/example/img:stable
Composefs:             capable
OSTree capable:        yes
Bootloader payload:    systemd-boot ✓
Target EFI loaders:  usr/lib/bootupd/shimx64.efi (UEFI CA 2011/2023)
bootc present:         yes
bootupd present:       yes
Desktops:              gnome, kde
Base OS:               fedora 44
Sysusers:              1 static allocation(s)
Transient root/etc:    yes / yes
fs-verity required:    yes
Initramfs composefs:   module present
Filesystem expected:   xfs
Kernel:                no kernel modules found
Compatible:            YES
";
        assert_eq!(table, expected);
    }

    #[test]
    fn table_for_empty_probe_reports_absent_and_incompatible() {
        let table = render(&ProbeFiles::default());
        let expected = "\
=== Target image capabilities ===
Image:                 ghcr.io/example/img:stable
Composefs:             not enabled in prepare-root.conf
OSTree capable:        no
Bootloader payload:    none
Target EFI loaders:  none
bootc present:         no
bootupd present:       no
Desktops:              none
Base OS:               unknown
Sysusers:              0 static allocation(s)
Transient root/etc:    no / no
fs-verity required:    no
Initramfs composefs:   not present (may need regeneration for a composefs boot)
Filesystem expected:   unknown
Kernel:                no kernel modules found
Compatible:            NO
  - no usr/lib/ostree/prepare-root.conf found — not an ostree/bootc image
";
        assert_eq!(table, expected);
    }

    #[test]
    fn table_marks_unsigned_loader_and_ostree_only_image() {
        // prepare-root.conf present but composefs not enabled: still a
        // compatible re-base target, reported as ostree-only.
        let table = render(&ProbeFiles {
            prepare_root: Some("[composefs]\nenabled = no\n".into()),
            target_bootloaders: vec![
                loader("usr/lib/systemd/boot/efi/systemd-bootx64.efi", &[]),
                loader("usr/lib/bootupd/shimx64.efi", &["2023"]),
            ],
            ..Default::default()
        });
        assert!(table.contains("Composefs:             not enabled in prepare-root.conf\n"));
        assert!(table.contains("OSTree capable:        yes\n"));
        assert!(table.contains(
            "Target EFI loaders:  usr/lib/systemd/boot/efi/systemd-bootx64.efi \
             (unrecognized signer), usr/lib/bootupd/shimx64.efi (UEFI CA 2023)\n"
        ));
        assert!(table.ends_with("Compatible:            YES\n"));
    }

    #[test]
    fn table_write_error_is_propagated() {
        struct Failing;
        impl Write for Failing {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("closed"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let caps = assemble(&ProbeFiles::default());
        let err = write_capabilities_table(&mut Failing, "img", &caps).unwrap_err();
        assert_eq!(err.to_string(), "closed");
    }
}
