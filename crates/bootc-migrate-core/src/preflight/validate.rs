//! Per-direction preflight validators.
//!
//! Consume a [`SystemInfo`](super::SystemInfo) and produce judgments for a
//! specific migration direction. Currently: OSTree → ComposeFS.

use super::PreflightReport;
use super::system_info::SystemInfo;
use crate::scan::BaseInfo;

/// Cross-base readiness information (issue #67).
///
/// When the source and target disagree on ID/ID_LIKE, the migration needs
/// extra hardening that same-family migrations skip. This struct collects
/// the additional warnings and advisories specific to cross-base re-bases.
#[derive(Debug, Clone)]
pub struct CrossBaseReadiness {
    /// True when the source and target are from different base families.
    pub is_cross_base: bool,
    /// Human-readable warnings to show the user before proceeding.
    pub warnings: Vec<String>,
}

impl CrossBaseReadiness {
    /// When cross-base and there are warnings, returns false — the caller
    /// should print the warnings and refuse unless `--accept-cross-base`.
    pub fn is_clean(&self) -> bool {
        !self.is_cross_base || self.warnings.is_empty()
    }
}

/// Validate readiness for a cross-base migration.
///
/// Compares the source host's base identity against the target image's.
/// Returns [`CrossBaseReadiness`] with
/// - `is_cross_base`: whether the two are from different base families
///   (same logic as [`crate::scan::is_cross_base`]).
/// - `warnings`: advisories about SELinux policy-type changes, UID/GID
///   divergence risk, and boot-critical configuration that may need review.
pub fn cross_base(host: &BaseInfo, target: &BaseInfo) -> CrossBaseReadiness {
    let is_cross_base = crate::scan::is_cross_base(host, target);
    let mut warnings = Vec::new();

    if !is_cross_base {
        return CrossBaseReadiness {
            is_cross_base: false,
            warnings,
        };
    }

    // When the two families don't share an ID_LIKE lineage, the SELinux
    // policy type and policy module set are likely to differ — even if the
    // type name is the same, the module versions and allowed transitions
    // may not be. The migration will detect this at apply time and schedule
    // /.autorelabel if needed.
    warnings.push(
        "Cross-base re-base detected: the target image is from a different \
         OS family. SELinux policy may differ — an /.autorelabel will be \
         scheduled automatically if the policy type changes. UID/GID remap \
         will run for system accounts that differ between the two bases. \
         /etc paths whose defaults differ will take the target's version \
         with the previous value preserved as a .rebase-old sidecar."
            .to_string(),
    );

    if host.id != target.id {
        warnings.push(format!(
            "Base identity change: {} → {}",
            host.id,
            target.id
        ));
    }

    CrossBaseReadiness {
        is_cross_base: true,
        warnings,
    }
}

/// Validate readiness for an OSTree → ComposeFS migration.
///
/// The only direction-specific judgment today is ESP readiness for
/// systemd-boot (≥150 MB free on a detected ESP); everything else in the
/// report is generic system state passed through from [`SystemInfo`].
pub fn ostree_to_composefs(sys: SystemInfo) -> PreflightReport {
    let esp_ready_for_systemd_boot =
        sys.esp_detected && sys.esp_free_space_bytes >= 150 * 1024 * 1024;
    PreflightReport {
        is_bootc_ostree: sys.is_bootc_ostree,
        pending_transaction: sys.pending_transaction,
        is_uefi: sys.is_uefi,
        nvram_writable: sys.nvram_writable,
        esp_path: sys.esp_path,
        esp_free_space_bytes: sys.esp_free_space_bytes,
        esp_fs_type: sys.esp_fs_type,
        esp_detected: sys.esp_detected,
        supports_reflink: sys.supports_reflink,
        is_btrfs: sys.is_btrfs,
        fs_type: sys.fs_type,
        ostree_repo_size_bytes: sys.ostree_repo_size_bytes,
        composefs_free_bytes: sys.composefs_free_bytes,
        esp_ready_for_systemd_boot,
        systemd_boot_binaries_present: sys.systemd_boot_binaries_present,
        grub_tools_available: sys.grub_tools_available,
        sysroot_was_ro: sys.sysroot_was_ro,
    }
}

#[cfg(test)]
mod tests {
    use super::super::system_info::PendingTransactionStatus;
    use super::*;

    fn sys_with_esp(detected: bool, free: u64) -> SystemInfo {
        SystemInfo {
            is_bootc_ostree: true,
            pending_transaction: PendingTransactionStatus::Clean,
            is_uefi: true,
            nvram_writable: true,
            esp_path: detected.then(|| "/boot/efi".to_string()),
            esp_free_space_bytes: free,
            esp_fs_type: Some("vfat".into()),
            esp_detected: detected,
            supports_reflink: true,
            is_btrfs: true,
            fs_type: Some("btrfs".into()),
            ostree_repo_size_bytes: 0,
            composefs_free_bytes: 0,
            systemd_boot_binaries_present: true,
            grub_tools_available: true,
            sysroot_was_ro: false,
        }
    }

    #[test]
    fn esp_readiness_threshold_is_150_mb() {
        let at = 150 * 1024 * 1024;
        assert!(ostree_to_composefs(sys_with_esp(true, at)).esp_ready_for_systemd_boot);
        assert!(!ostree_to_composefs(sys_with_esp(true, at - 1)).esp_ready_for_systemd_boot);
    }

    #[test]
    fn undetected_esp_is_never_ready() {
        let r = ostree_to_composefs(sys_with_esp(false, u64::MAX));
        assert!(!r.esp_ready_for_systemd_boot);
    }

    // --- cross_base validator tests ---

    #[test]
    fn cross_base_same_family_is_clean() {
        let host = BaseInfo {
            id: "fedora".into(),
            id_like: None,
            version_id: Some("44".into()),
        };
        let target = BaseInfo {
            id: "fedora".into(),
            id_like: None,
            version_id: Some("44".into()),
        };
        let readiness = cross_base(&host, &target);
        assert!(!readiness.is_cross_base);
        assert!(readiness.is_clean());
        assert!(readiness.warnings.is_empty());
    }

    #[test]
    fn cross_base_different_family_produces_warnings() {
        let host = BaseInfo {
            id: "fedora".into(),
            id_like: None,
            version_id: Some("44".into()),
        };
        let target = BaseInfo {
            id: "centos".into(),
            id_like: Some("rhel".into()),
            version_id: Some("10".into()),
        };
        let readiness = cross_base(&host, &target);
        assert!(readiness.is_cross_base);
        assert!(!readiness.is_clean());
        assert!(!readiness.warnings.is_empty());
        assert!(readiness.warnings.iter().any(|w| w.contains("fedora")));
        assert!(readiness.warnings.iter().any(|w| w.contains("centos")));
    }

    #[test]
    fn cross_base_same_family_via_id_like_is_clean() {
        // Bluefin (fedora-derived) → Dakota (fedora-derived) is same-base.
        let host = BaseInfo {
            id: "bluefin".into(),
            id_like: Some("fedora".into()),
            version_id: None,
        };
        let target = BaseInfo {
            id: "dakota".into(),
            id_like: Some("fedora".into()),
            version_id: None,
        };
        let readiness = cross_base(&host, &target);
        assert!(!readiness.is_cross_base);
        assert!(readiness.is_clean());
        assert!(readiness.warnings.is_empty());
    }

    #[test]
    fn cross_base_readiness_struct_is_debug_and_clone() {
        let readiness = CrossBaseReadiness {
            is_cross_base: false,
            warnings: vec![],
        };
        // Compile-time check that Debug + Clone are implemented.
        let _ = format!("{readiness:?}");
        let _ = readiness.clone();
    }
