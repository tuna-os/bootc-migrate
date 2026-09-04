//! Preflight checks: generic system introspection ([`SystemInfo`]) plus
//! per-direction validators ([`validate`]).

pub mod readiness;
pub mod system_info;
pub mod validate;

pub use system_info::{
    BootcStatus, BootedStatus, HostStatus, PendingTransactionStatus, SystemInfo,
    check_pending_ostree_transaction, check_reflink_support, count_composefs_files, get_free_space,
    parse_ostree_status_for_pending, parse_var_is_separate_mount, select_container_storage_path,
};

use anyhow::Result;

#[derive(Debug)]
pub struct PreflightReport {
    /// The backend the running deployment boots from, or `None` if this is not
    /// a bootc deployment at all — the only state that is a hard blocker.
    ///
    /// This replaced a bool `is_bootc_ostree`, which could not tell a composefs
    /// host apart from a non-bootc one and so refused both.
    pub booted_backend: Option<crate::rebase_plan::Backend>,
    /// The image reference the running deployment was booted from, if any.
    pub booted_image: Option<String>,
    pub pending_transaction: PendingTransactionStatus,
    pub is_uefi: bool,
    pub nvram_writable: bool,
    pub esp_path: Option<String>,
    pub esp_free_space_bytes: u64,
    pub esp_fs_type: Option<String>,
    /// Whether an ESP was detected (even if temporarily mounted during preflight).
    pub esp_detected: bool,
    pub supports_reflink: bool,
    pub is_btrfs: bool,
    /// Filesystem type string from /proc/mounts ("btrfs", "xfs", "ext4", etc.)
    pub fs_type: Option<String>,
    pub ostree_repo_size_bytes: u64,
    pub composefs_free_bytes: u64,
    /// Free space where podman's container storage lives — where the Phase-2
    /// pull of the target image lands, which is a different filesystem from
    /// the composefs store whenever /var is its own volume.
    pub container_storage_free_bytes: u64,
    /// Which path `container_storage_free_bytes` was measured on.
    pub container_storage_path: String,
    /// Whether /var is a separate mount from the root filesystem.
    pub var_is_separate_mount: bool,
    /// Whether the ESP has enough space for systemd-boot (≥150 MB).
    pub esp_ready_for_systemd_boot: bool,
    /// Whether the systemd-boot EFI binaries are installed in the running deployment
    /// (i.e. `/usr/lib/systemd/boot/efi` exists). `bootctl install` requires this.
    pub systemd_boot_binaries_present: bool,
    /// Whether grub2-reboot / grub2-editenv are available.
    pub grub_tools_available: bool,
    pub sysroot_was_ro: bool,
}

/// Gather system state and validate it for the OSTree → ComposeFS direction.
pub fn run_preflight_checks() -> Result<PreflightReport> {
    let sys = SystemInfo::gather()?;
    Ok(validate::ostree_to_composefs(sys))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use tempfile::tempdir;

    #[test]
    fn get_free_space_returns_value() {
        let dir = tempdir().unwrap();
        let space = get_free_space(dir.path()).unwrap();
        assert!(space > 0);
    }

    #[test]
    fn preflight_report_has_all_fields() {
        let report = PreflightReport {
            booted_backend: Some(crate::rebase_plan::Backend::Ostree),
            booted_image: Some("ghcr.io/ublue-os/bluefin:gts".into()),
            pending_transaction: PendingTransactionStatus::Clean,
            is_uefi: true,
            nvram_writable: true,
            esp_path: Some("/boot/efi".into()),
            esp_free_space_bytes: 400 * 1024 * 1024,
            esp_fs_type: Some("vfat".into()),
            supports_reflink: true,
            is_btrfs: true,
            fs_type: Some("btrfs".to_string()),
            ostree_repo_size_bytes: 1024 * 1024 * 1024,
            composefs_free_bytes: 5 * 1024 * 1024 * 1024,
            container_storage_free_bytes: 20 * 1024 * 1024 * 1024,
            container_storage_path: "/var/lib/containers/storage".to_string(),
            var_is_separate_mount: false,
            esp_ready_for_systemd_boot: true,
            systemd_boot_binaries_present: true,
            grub_tools_available: true,
            esp_detected: true,
            sysroot_was_ro: true,
        };
        assert!(report.esp_ready_for_systemd_boot);
        assert!(report.grub_tools_available);
    }

    // ---- Pending-transaction detection tests ----

    #[test]
    fn parse_clean_status() {
        let out = concat!(
            "* default abcdef1234567890.0\n",
            "    origin: <unknown origin type>\n",
        );
        assert_eq!(
            parse_ostree_status_for_pending(out),
            PendingTransactionStatus::Clean
        );
    }

    #[test]
    fn parse_staged_deployment() {
        let out = concat!(
            "* default abcdef1234567890.0\n",
            "  default abcdef1234567891.0 (staged)\n",
        );
        assert_eq!(
            parse_ostree_status_for_pending(out),
            PendingTransactionStatus::StagedDeployment
        );
    }

    #[test]
    fn parse_pending_deployment() {
        let out = concat!(
            "* default abcdef1234567890.0\n",
            "  default abcdef1234567892.0 (pending)\n",
            "  default abcdef1234567891.0 (rollback)\n",
        );
        assert_eq!(
            parse_ostree_status_for_pending(out),
            PendingTransactionStatus::PendingDeployment
        );
    }

    #[test]
    fn parse_staged_takes_priority() {
        // Both (staged) and (pending) present; (staged) comes first.
        let out = concat!(
            "* default abcdef1234567890.0\n",
            "  default abcdef1234567892.0 (staged)\n",
            "  default abcdef1234567893.0 (pending)\n",
        );
        assert_eq!(
            parse_ostree_status_for_pending(out),
            PendingTransactionStatus::StagedDeployment
        );
    }

    #[test]
    fn parse_empty_output() {
        assert_eq!(
            parse_ostree_status_for_pending(""),
            PendingTransactionStatus::Clean
        );
    }

    #[test]
    fn parse_no_deployments() {
        assert_eq!(
            parse_ostree_status_for_pending("No deployments.\n"),
            PendingTransactionStatus::Clean
        );
    }

    #[test]
    fn parse_rollback_only() {
        let out = concat!(
            "* default abcdef1234567890.0\n",
            "  default abcdef1234567891.0 (rollback)\n",
        );
        assert_eq!(
            parse_ostree_status_for_pending(out),
            PendingTransactionStatus::Clean
        );
    }

    #[test]
    fn pending_status_display_clean() {
        assert_eq!(
            format!("{}", PendingTransactionStatus::Clean),
            "no pending transaction"
        );
    }

    #[test]
    fn pending_status_display_staged() {
        assert_eq!(
            format!("{}", PendingTransactionStatus::StagedDeployment),
            "staged deployment (next boot will apply)"
        );
    }

    #[test]
    fn pending_status_display_pending() {
        assert_eq!(
            format!("{}", PendingTransactionStatus::PendingDeployment),
            "pending deployment (update in progress)"
        );
    }

    #[test]
    fn pending_status_display_stale() {
        assert_eq!(
            format!("{}", PendingTransactionStatus::StaleTransactionFiles),
            "stale transaction temp files in OSTree repo"
        );
    }

    // ---- composefs object counting tests ----

    #[test]
    fn count_composefs_empty_dir() {
        let dir = tempdir().unwrap();
        assert_eq!(count_composefs_files(dir.path()), 0);
    }

    #[test]
    fn count_composefs_no_objects() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("ab")).unwrap();
        fs::create_dir_all(dir.path().join("cd")).unwrap();
        // Two hex prefix dirs but no files
        assert_eq!(count_composefs_files(dir.path()), 0);
    }

    #[test]
    fn count_composefs_with_objects() {
        let dir = tempdir().unwrap();
        let d1 = dir.path().join("ab");
        let d2 = dir.path().join("cd");
        fs::create_dir_all(&d1).unwrap();
        fs::create_dir_all(&d2).unwrap();
        fs::write(d1.join("cdef1234567890"), b"obj1").unwrap();
        fs::write(d1.join("cdef1234567891"), b"obj2").unwrap();
        fs::write(d2.join("ef1234567890ab"), b"obj3").unwrap();
        assert_eq!(count_composefs_files(dir.path()), 3);
    }

    #[test]
    fn count_composefs_ignores_root_files() {
        // Files at the root of the objects dir (e.g. meta.json) are not counted;
        // only files inside subdirectories (the two-level prefix layout) are.
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("ab")).unwrap();
        fs::write(dir.path().join("meta.json"), b"meta").unwrap();
        fs::write(dir.path().join("ab/cdef1234567890"), b"obj1").unwrap();
        assert_eq!(count_composefs_files(dir.path()), 1);
    }

    #[test]
    fn count_composefs_nonexistent_dir() {
        assert_eq!(count_composefs_files(Path::new("/nonexistent/path")), 0);
    }
}
