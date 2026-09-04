//! Human-readable preflight reporting and migration gating shared by every
//! binary that drives the core pipeline (bootc-migrate, bootc-rebase).
//!
//! Split from the migrator binary so output stays identical across drivers:
//! [`print_report`] and [`print_readiness`] emit the exact preflight summary
//! the E2E suite and users have always seen; [`gate`] encodes the go/no-go
//! rules (`--force` / `--skip-preflight` semantics included).

use super::{PendingTransactionStatus, PreflightReport};
use crate::rebase_plan::Backend;

/// Print the detailed preflight report ("  - ..." lines).
pub fn print_report(report: &PreflightReport) {
    println!(
        "  - Booted bootc backend:  {}",
        match report.booted_backend {
            Some(Backend::Ostree) => "ostree",
            Some(Backend::Composefs) => "composefs (already converted)",
            None => "none — not a bootc deployment",
        }
    );
    match report.pending_transaction {
        PendingTransactionStatus::Clean => {}
        ref other => println!(
            "  ⚠ Pending OSTree transaction: {} — aborting (run `ostree admin undeploy` or complete the update first)",
            other
        ),
    }
    println!(
        "  - UEFI Boot Mode:        {}",
        if report.is_uefi {
            "Yes"
        } else {
            "No (Legacy BIOS)"
        }
    );
    println!(
        "  - NVRAM writable:        {}",
        if report.nvram_writable { "Yes" } else { "No" }
    );
    println!(
        "  - ESP Mounted Path:      {}",
        report
            .esp_path
            .as_deref()
            .unwrap_or("None — GRUB2-only migration")
    );
    if let Some(ref fs) = report.esp_fs_type {
        println!("  - ESP Filesystem:        {}", fs);
    }
    println!(
        "  - ESP Free Space:        {:.2} MB",
        report.esp_free_space_bytes as f64 / (1024.0 * 1024.0)
    );
    println!(
        "  - Filesystem:            {}",
        report.fs_type.as_deref().unwrap_or("unknown")
    );
    println!(
        "  - Btrfs Filesystem:      {}",
        if report.is_btrfs { "Yes" } else { "No" }
    );
    if report.sysroot_was_ro {
        println!("  - /sysroot was RO:       Yes (remounted rw for reflink test)");
    }
    println!(
        "  - Reflink (CoW) Support: {}",
        if report.supports_reflink { "Yes" } else { "No" }
    );
    println!(
        "  - OSTree repo size:      {:.2} GB",
        report.ostree_repo_size_bytes as f64 / 1e9
    );
    println!(
        "  - ComposeFS free space:  {:.2} GB",
        report.composefs_free_bytes as f64 / 1e9
    );
    println!(
        "  - GRUB tools available:  {}",
        if report.grub_tools_available {
            "Yes"
        } else {
            "No"
        }
    );
    println!(
        "  - ESP ready for sd-boot: {}",
        if report.esp_ready_for_systemd_boot {
            "Yes (>=150 MB)"
        } else {
            "No"
        }
    );
    println!(
        "  - systemd-boot binaries: {}",
        if report.systemd_boot_binaries_present {
            "Yes (/usr/lib/systemd/boot/efi)"
        } else {
            "No (bootctl install would fail)"
        }
    );
    println!();
}

/// Minimum free space required where podman's container storage lives.
///
/// Preflight cannot know the target image's size — it runs before anything is
/// pulled, and asking the registry would make a local check depend on network
/// access. So this is a floor, not a prediction: it is the point below which a
/// pull of a normal desktop bootc image is very unlikely to fit.
///
/// Calibrated against the E2E LVM cell, which is the case that produced
/// bootc-migrate#185: a 4 GiB `/var` failed partway through the Phase-2 pull of
/// `dakota:stable` (~5 GB compressed), and 20 GiB completed it with room to
/// spare. 10 GiB sits between the two — high enough to catch the layouts that
/// genuinely cannot work, low enough not to cry wolf on a tight-but-viable
/// system. It is advisory: exceeding it is not a guarantee, and falling below
/// it warns rather than refuses.
pub const MIN_CONTAINER_STORAGE_BYTES: u64 = 10 * 1024 * 1024 * 1024;

/// Whether the container-storage filesystem has room for the target image pull.
///
/// Pure so the threshold and the message can be tested without a real `/var`.
pub fn container_storage_has_room(free_bytes: u64) -> bool {
    free_bytes >= MIN_CONTAINER_STORAGE_BYTES
}

/// Compute the readiness warnings for this system. Empty means all clear.
pub fn readiness_issues(report: &PreflightReport) -> Vec<String> {
    let mut issues: Vec<String> = Vec::new();
    if report.booted_backend.is_none() {
        issues.push("Not booted into a bootc deployment — nothing to migrate from.".to_string());
    }
    if !report.is_uefi {
        issues.push(
            "Legacy BIOS boot detected — systemd-boot unavailable; will stay on GRUB2.".to_string(),
        );
    }
    if report.is_uefi && !report.nvram_writable {
        issues.push(
            "UEFI NVRAM not writable — efibootmgr may fail; systemd-boot may not register."
                .to_string(),
        );
    }
    if !report.esp_detected {
        issues.push("No ESP found — systemd-boot unavailable; will use GRUB2.".to_string());
    }
    if report.is_uefi && report.esp_path.is_some() && !report.esp_ready_for_systemd_boot {
        issues.push(
            "ESP too small for systemd-boot — need >=150 MB free; will use GRUB2 instead."
                .to_string(),
        );
    }
    if report.is_uefi && !report.systemd_boot_binaries_present {
        issues.push("systemd-boot binaries missing in source OS — migration will extract them from the target image instead.".to_string());
    }
    if !report.grub_tools_available {
        issues.push(
            "No GRUB tools (grub2-reboot, grub2-editenv) — one-shot boot selection may fail."
                .to_string(),
        );
    }
    if !report.supports_reflink {
        issues
            .push("No reflink support — object copies will use 1.5× more disk space.".to_string());
    }
    let has_free_space =
        report.composefs_free_bytes as f64 > (report.ostree_repo_size_bytes as f64 * 1.5);
    if !has_free_space && report.ostree_repo_size_bytes > 0 {
        issues.push(
            "Insufficient free space for migration — need >=1.5× repo size (without reflink)."
                .to_string(),
        );
    }
    // Separate from the check above: that one sizes the composefs store's
    // filesystem against the repo being converted, while Phase 2 pulls the
    // target image into podman's storage under /var. On a dedicated /var those
    // are different volumes, and only this check looks at the one the pull
    // actually writes to (bootc-migrate#185).
    if !container_storage_has_room(report.container_storage_free_bytes) {
        let free_gb = report.container_storage_free_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
        let min_gb = MIN_CONTAINER_STORAGE_BYTES as f64 / (1024.0 * 1024.0 * 1024.0);
        let separate = if report.var_is_separate_mount {
            " /var is a separate volume, so free space on / does not help here."
        } else {
            ""
        };
        issues.push(format!(
            "Low free space on {} ({:.1} GB) for the target image pull — \
             recommend >={:.0} GB.{}",
            report.container_storage_path, free_gb, min_gb, separate
        ));
    }
    issues
}

/// Print the readiness summary and the bootloader plan.
pub fn print_readiness(report: &PreflightReport) {
    println!("=== Migration Readiness ===");
    let issues = readiness_issues(report);
    if issues.is_empty() {
        println!("  ✓ All preflight checks passed.");
    } else {
        for issue in &issues {
            println!("  ⚠ {}", issue);
        }
    }

    // We migrate to systemd-boot by lifting the loader binary out of the target image,
    // so the source OS no longer needs to ship systemd-boot. The systemd_boot_binaries_present
    // field is now purely informational (warning if neither side ships it).
    let use_systemd_boot = report.esp_ready_for_systemd_boot && report.nvram_writable;
    if use_systemd_boot {
        println!("\nBootloader: Will migrate to systemd-boot (ESP ready, NVRAM writable).");
    } else if report.esp_path.is_some() {
        println!("\nBootloader: Will stay on GRUB2 (BLS Type 1).");
        if !report.grub_tools_available {
            println!("  WARNING: grub2-reboot not found. Boot selection may not work.");
            println!(
                "  The composefs entry will be written but you may need to select it manually"
            );
            println!("  from the GRUB menu on next boot.");
        }
    } else {
        println!("\nBootloader: Will stay on GRUB2 (BLS Type 1) — no ESP detected.");
    }
}

/// Go/no-go decision for starting the migration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationGate {
    /// All gates passed — start the OSTree → ComposeFS migration.
    Proceed,
    /// The host is already composefs-backed, so there is no backend to
    /// convert: the caller must run the image swap
    /// ([`crate::rebase_controller::ImageSwapConfig`]) instead of the
    /// conversion pipeline.
    ///
    /// This is deliberately its own variant rather than `Proceed`. Running the
    /// ostree→composefs phases against a composefs host would convert a repo
    /// that is not there, and a caller that has not been made to think about
    /// the difference should not compile.
    ImageSwap,
    /// No reflink support: the driver should get explicit confirmation (an
    /// interactive prompt or `--force`) before a full-copy migration.
    ConfirmFullCopy,
    /// Hard refusal with the reason to show the user.
    Refuse(String),
}

/// Evaluate the migration gates in order. `force` overrides everything except
/// nothing; `skip_preflight` additionally waives the pending-transaction gate.
pub fn gate(report: &PreflightReport, force: bool, skip_preflight: bool) -> MigrationGate {
    match report.booted_backend {
        // Nothing to migrate from. `force` still overrides, as it always has.
        None if !force => {
            return MigrationGate::Refuse(
                "System is not booted into a bootc deployment (neither ostree nor composefs). \
                 Cannot perform migration."
                    .to_string(),
            );
        }
        // Already converted — the useful operation is swapping the image, not
        // converting the backend. Checked before the pending-transaction and
        // reflink gates below because neither applies: there is no ostree repo
        // to have a pending transaction on, and no object copies to reflink.
        Some(Backend::Composefs) => return MigrationGate::ImageSwap,
        _ => {}
    }
    // Block on pending transactions — they cause incomplete composefs images
    // and switch-root-os-release-errors on next boot.
    if report.pending_transaction != PendingTransactionStatus::Clean && !force && !skip_preflight {
        return MigrationGate::Refuse(format!(
            "Pending OSTree transaction detected: {}.\n\
             The OSTree repo has uncommitted state from a previous update. The migration\n\
             would produce an incomplete composefs image that cannot boot.\n\
             \n\
             To resolve:\n\
               - If you ran `bootc upgrade` or `rpm-ostree upgrade`, complete it first.\n\
               - If the update was interrupted, run `ostree admin undeploy <index>`\n\
                 to remove the pending deployment.\n\
               - Or run `bootc upgrade` to finish/finalize the pending transaction.\n",
            report.pending_transaction
        ));
    }
    if !report.supports_reflink && !force {
        return MigrationGate::ConfirmFullCopy;
    }
    MigrationGate::Proceed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A report with every gate green — the baseline each test perturbs.
    fn healthy() -> PreflightReport {
        PreflightReport {
            booted_backend: Some(Backend::Ostree),
            booted_image: Some("ghcr.io/ublue-os/bluefin:gts".into()),
            pending_transaction: PendingTransactionStatus::Clean,
            is_uefi: true,
            nvram_writable: true,
            esp_path: Some("/boot/efi".into()),
            esp_free_space_bytes: 500 * 1024 * 1024,
            esp_fs_type: Some("vfat".into()),
            esp_detected: true,
            supports_reflink: true,
            is_btrfs: true,
            fs_type: Some("btrfs".into()),
            ostree_repo_size_bytes: 8_000_000_000,
            composefs_free_bytes: 33_000_000_000,
            container_storage_free_bytes: MIN_CONTAINER_STORAGE_BYTES,
            container_storage_path: "/var/lib/containers/storage".to_string(),
            var_is_separate_mount: false,
            esp_ready_for_systemd_boot: true,
            systemd_boot_binaries_present: true,
            grub_tools_available: true,
            sysroot_was_ro: false,
        }
    }

    #[test]
    fn healthy_report_proceeds_with_no_issues() {
        let r = healthy();
        assert!(readiness_issues(&r).is_empty());
        assert_eq!(gate(&r, false, false), MigrationGate::Proceed);
    }

    /// A system that is not a bootc deployment at all: nothing to migrate.
    #[test]
    fn non_bootc_boot_is_refused_unless_forced() {
        let mut r = healthy();
        r.booted_backend = None;
        assert!(matches!(gate(&r, false, false), MigrationGate::Refuse(_)));
        // force overrides — the operator has taken responsibility.
        assert_eq!(gate(&r, true, false), MigrationGate::Proceed);
    }

    /// The behaviour this replaced: a composefs host was lumped in with a
    /// non-bootc one and refused, which is what made "already migrated" look
    /// like a hard blocker. It now routes to the image swap.
    #[test]
    fn composefs_host_routes_to_image_swap_instead_of_refusing() {
        let mut r = healthy();
        r.booted_backend = Some(Backend::Composefs);
        assert_eq!(gate(&r, false, false), MigrationGate::ImageSwap);
        // And it is not a readiness issue either — there is nothing wrong.
        assert!(
            !readiness_issues(&r)
                .iter()
                .any(|i| i.contains("bootc deployment")),
            "a composefs host is the finished state, not a warning: {:?}",
            readiness_issues(&r)
        );
    }

    /// The image swap does not touch the ostree repo, so neither the
    /// pending-transaction gate nor the reflink gate applies to it. Both would
    /// otherwise fire on a composefs host and block a swap for reasons that
    /// cannot affect it.
    #[test]
    fn image_swap_is_not_blocked_by_ostree_only_gates() {
        let mut r = healthy();
        r.booted_backend = Some(Backend::Composefs);
        r.pending_transaction = PendingTransactionStatus::StagedDeployment;
        r.supports_reflink = false;
        assert_eq!(gate(&r, false, false), MigrationGate::ImageSwap);
    }

    #[test]
    fn pending_transaction_is_refused_unless_waived() {
        for pending in [
            PendingTransactionStatus::StagedDeployment,
            PendingTransactionStatus::PendingDeployment,
            PendingTransactionStatus::StaleTransactionFiles,
        ] {
            let mut r = healthy();
            r.pending_transaction = pending.clone();
            assert!(
                matches!(gate(&r, false, false), MigrationGate::Refuse(_)),
                "{pending:?} must refuse"
            );
            // Either waiver flag lets it pass.
            assert_eq!(gate(&r, true, false), MigrationGate::Proceed);
            assert_eq!(gate(&r, false, true), MigrationGate::Proceed);
        }
    }

    #[test]
    fn refusal_message_names_the_pending_state_and_a_fix() {
        let mut r = healthy();
        r.pending_transaction = PendingTransactionStatus::StagedDeployment;
        let MigrationGate::Refuse(msg) = gate(&r, false, false) else {
            panic!("expected refusal");
        };
        assert!(msg.contains("Pending OSTree transaction"));
        assert!(msg.contains("undeploy"), "must tell the user how to fix it");
    }

    #[test]
    fn no_reflink_asks_for_confirmation_not_refusal() {
        let mut r = healthy();
        r.supports_reflink = false;
        assert_eq!(gate(&r, false, false), MigrationGate::ConfirmFullCopy);
        assert_eq!(gate(&r, true, false), MigrationGate::Proceed);
        // skip_preflight is NOT a full-copy consent — still asks.
        assert_eq!(gate(&r, false, true), MigrationGate::ConfirmFullCopy);
    }

    #[test]
    fn gate_order_refusal_beats_full_copy_confirmation() {
        // A non-bootc system without reflink must refuse, not ask about disk.
        let mut r = healthy();
        r.booted_backend = None;
        r.supports_reflink = false;
        assert!(matches!(gate(&r, false, false), MigrationGate::Refuse(_)));
    }

    #[test]
    fn issues_fire_per_condition() {
        let mut r = healthy();
        r.nvram_writable = false;
        r.supports_reflink = false;
        r.systemd_boot_binaries_present = false;
        let issues = readiness_issues(&r);
        assert!(issues.iter().any(|i| i.contains("NVRAM")));
        assert!(issues.iter().any(|i| i.contains("reflink")));
        assert!(issues.iter().any(|i| i.contains("systemd-boot binaries")));
        assert_eq!(issues.len(), 3, "no unexpected extra issues: {issues:?}");
    }

    #[test]
    fn tight_disk_without_reflink_warns_about_space() {
        let mut r = healthy();
        r.supports_reflink = false;
        r.composefs_free_bytes = r.ostree_repo_size_bytes; // < 1.5×
        assert!(
            readiness_issues(&r)
                .iter()
                .any(|i| i.contains("Insufficient free space"))
        );
    }

    /// bootc-migrate#185: the Phase-2 pull lands in podman's storage under
    /// /var, not on the composefs store's filesystem. A system with plenty of
    /// room for the store and none for the pull must still be warned.
    #[test]
    fn low_container_storage_is_reported_even_when_composefs_space_is_fine() {
        let mut r = healthy();
        // Composefs side deliberately generous — this is the exact shape the
        // LVM E2E cell had: roomy root, tiny dedicated /var.
        r.composefs_free_bytes = 200 * 1024 * 1024 * 1024;
        r.ostree_repo_size_bytes = 1024 * 1024 * 1024;
        r.container_storage_free_bytes = 4 * 1024 * 1024 * 1024;
        r.container_storage_path = "/var".to_string();
        r.var_is_separate_mount = true;

        let issues = readiness_issues(&r);
        let hit = issues
            .iter()
            .find(|i| i.contains("Low free space"))
            .expect("expected a container-storage warning");
        // The message has to name the mount to enlarge; "insufficient space"
        // pointing at repo size is what made this failure hard to read.
        assert!(hit.contains("/var"), "message must name the mount: {hit}");
        assert!(
            hit.contains("4.0 GB"),
            "message must state what was found: {hit}"
        );
        assert!(
            hit.contains("separate volume"),
            "a dedicated /var is the case where free space on / misleads: {hit}"
        );
        // The repo-size check must NOT fire here — they are separate concerns.
        assert!(
            !issues.iter().any(|i| i.contains("1.5×")),
            "composefs space was ample; only the /var warning belongs: {issues:?}"
        );
    }

    #[test]
    fn ample_container_storage_is_silent() {
        let mut r = healthy();
        r.container_storage_free_bytes = 50 * 1024 * 1024 * 1024;
        assert!(readiness_issues(&r).is_empty());
    }

    #[test]
    fn container_storage_threshold_is_inclusive_at_the_boundary() {
        assert!(container_storage_has_room(MIN_CONTAINER_STORAGE_BYTES));
        assert!(!container_storage_has_room(MIN_CONTAINER_STORAGE_BYTES - 1));
        assert!(!container_storage_has_room(0));
    }

    /// Without a separate /var the message must not claim there is one.
    #[test]
    fn single_filesystem_omits_the_separate_volume_note() {
        let mut r = healthy();
        r.container_storage_free_bytes = 1024;
        r.container_storage_path = "/var/lib/containers/storage".to_string();
        r.var_is_separate_mount = false;
        let issues = readiness_issues(&r);
        let hit = issues
            .iter()
            .find(|i| i.contains("Low free space"))
            .unwrap();
        assert!(!hit.contains("separate volume"), "{hit}");
    }
}
