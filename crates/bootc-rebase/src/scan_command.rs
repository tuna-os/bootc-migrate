//! CLI adapter for `bootc-rebase scan`: dispatch plus the human-readable
//! capability table.
//!
//! Presentation only. Capability discovery and every compatibility decision
//! stay in `bootc_migrate_core::scan` — this module must not grow scanning
//! policy, or the CLI and the engine can disagree about what a target
//! supports (#158).

use anyhow::Result;

use crate::ScanArgs;

/// Scan the target image and render the result as JSON or a table.
pub fn run(args: &ScanArgs) -> Result<()> {
    println!("Scanning target image {}...", args.image);
    let caps = bootc_migrate_core::scan::scan_target_image(&args.image)?;
    if args.json {
        println!("{}", caps.to_json());
    } else {
        print_capabilities_table(&args.image, &caps);
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
    println!(
        "bootc present:         {}",
        if caps.bootc_present { "yes" } else { "no" }
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
    let issues = bootc_migrate_core::scan::compatibility_issues(caps);
    println!(
        "Compatible:            {}",
        if issues.is_empty() { "YES" } else { "NO" }
    );
    for issue in &issues {
        println!("  - {issue}");
    }
}
