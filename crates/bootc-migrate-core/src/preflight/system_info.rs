//! Generic system introspection — no knowledge of migration direction.
//!
//! [`SystemInfo::gather`] probes the running system (backend, firmware, ESP,
//! root filesystem, reflink support, bootloader tooling, free space, pending
//! OSTree transactions). Direction-specific judgments live in
//! [`super::validate`].

use crate::rebase_plan::Backend;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs;
use std::path::Path;
use std::process::Command;

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct BootcStatus {
    pub api_version: String,
    pub kind: String,
    pub status: HostStatus,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct HostStatus {
    pub booted: Option<BootedStatus>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct BootedStatus {
    pub ostree: Option<serde_json::Value>,
    pub composefs: Option<serde_json::Value>,
}

impl BootedStatus {
    /// Which storage backend the running deployment uses.
    ///
    /// `None` means neither key was present: not a bootc deployment at all,
    /// which is the only case that has ever been a genuine blocker. Reading
    /// the `composefs` key is what distinguishes "already converted" from
    /// "not bootc" — before this both looked the same and both were refused.
    pub fn backend(&self) -> Option<Backend> {
        if self.ostree.is_some() {
            Some(Backend::Ostree)
        } else if self.composefs.is_some() {
            Some(Backend::Composefs)
        } else {
            None
        }
    }
}

/// Result of checking for a pending OSTree transaction.
///
/// A pending transaction means an update (rpm-ostree / bootc upgrade) was
/// started but not completed, or a staged deployment is waiting for the next
/// boot. Running the migration in this state can produce an incomplete
/// composefs image — objects referenced by the EROFS may be missing or stale,
/// causing switch-root failure on the next boot.
#[derive(Debug, Clone, PartialEq)]
pub enum PendingTransactionStatus {
    /// No pending transaction detected — migration is safe to proceed.
    Clean,
    /// A staged deployment exists (prepared by bootc upgrade for next boot).
    StagedDeployment,
    /// A pending deployment exists (created by rpm-ostree but not yet booted).
    PendingDeployment,
    /// Stale transaction temp files found in the OSTree repo.
    StaleTransactionFiles,
}

impl std::fmt::Display for PendingTransactionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PendingTransactionStatus::Clean => write!(f, "no pending transaction"),
            PendingTransactionStatus::StagedDeployment => {
                write!(f, "staged deployment (next boot will apply)")
            }
            PendingTransactionStatus::PendingDeployment => {
                write!(f, "pending deployment (update in progress)")
            }
            PendingTransactionStatus::StaleTransactionFiles => {
                write!(f, "stale transaction temp files in OSTree repo")
            }
        }
    }
}

/// Parse the output of `ostree admin status` to detect pending or staged
/// deployments. Pure function — no I/O, trivially testable.
///
/// Looks for lines containing "(staged)" (a deployment prepared for next boot)
/// or "(pending)" (an update in progress that hasn't been booted).
pub fn parse_ostree_status_for_pending(status_output: &str) -> PendingTransactionStatus {
    for line in status_output.lines() {
        let trimmed = line.trim();
        if trimmed.contains("(staged)") {
            return PendingTransactionStatus::StagedDeployment;
        }
        if trimmed.contains("(pending)") {
            return PendingTransactionStatus::PendingDeployment;
        }
    }
    PendingTransactionStatus::Clean
}

/// Count the number of files in a composefs object store directory (two-level
/// hex prefix layout: `objects/<xx>/<rest>`). Pure function — caller provides
/// the directory path.
pub fn count_composefs_files(objects_dir: &Path) -> usize {
    let mut total = 0usize;
    if let Ok(rd) = fs::read_dir(objects_dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir()
                && let Ok(sub) = fs::read_dir(&path)
            {
                total += sub.flatten().filter(|e| e.path().is_file()).count();
            }
        }
    }
    total
}

/// Detect a pending OSTree transaction by checking:
/// 1. `/run/ostree/staged-deployment` — staged deployment file
/// 2. `ostree admin status` output for "(pending)" or "(staged)" markers
/// 3. Stale temp files in `/sysroot/ostree/repo/tmp/`
pub fn check_pending_ostree_transaction() -> PendingTransactionStatus {
    // 1. Check for staged deployment file first (most definitive).
    if Path::new("/run/ostree/staged-deployment").exists() {
        return PendingTransactionStatus::StagedDeployment;
    }

    // 2. Parse ostree admin status for (pending) or (staged).
    if let Ok(output) = Command::new("ostree").args(["admin", "status"]).output()
        && output.status.success()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let parsed = parse_ostree_status_for_pending(&stdout);
        if parsed != PendingTransactionStatus::Clean {
            return parsed;
        }
    }

    // 3. Check for stale repo temp files.
    // Only count files (not subdirectories like "cache") that look like
    // transaction residue (e.g. `.stale-transaction-*`, `staging-*`).
    let repo_tmp = Path::new("/sysroot/ostree/repo/tmp");
    if repo_tmp.exists()
        && let Ok(rd) = fs::read_dir(repo_tmp)
    {
        let stale_count = rd
            .filter_map(|e| e.ok())
            .filter(|e| {
                // Only count regular files, not subdirs like "cache".
                if !e.path().is_file() {
                    return false;
                }
                let name = e.file_name();
                let name_str = name.to_string_lossy();
                // Known stale-transaction patterns from OSTree internals.
                name_str.starts_with(".stale-transaction")
                    || name_str.starts_with("staging-")
                    || name_str.starts_with("ostree-txn")
            })
            .count();
        if stale_count > 0 {
            return PendingTransactionStatus::StaleTransactionFiles;
        }
    }

    PendingTransactionStatus::Clean
}

pub fn get_free_space<P: AsRef<Path>>(path: P) -> Result<u64> {
    let stats = rustix::fs::statvfs(path.as_ref()).context("statvfs failed")?;
    let block_size = if stats.f_frsize > 0 {
        stats.f_frsize
    } else {
        stats.f_bsize
    };
    Ok(block_size * stats.f_bavail)
}

/// Candidate paths for podman's container storage, most specific first.
///
/// Phase 2 pulls the target image through podman, which bind-mounts
/// `/var/lib/containers/storage` from the host, so this — not `/sysroot` — is
/// where the migration's largest single write lands.
const CONTAINER_STORAGE_CANDIDATES: [&str; 3] = ["/var/lib/containers/storage", "/var", "/"];

/// Pick the most specific container-storage path that exists.
///
/// Split from the `statvfs` call so the selection order is testable without
/// touching the host's real `/var`.
pub fn select_container_storage_path<F>(exists: F) -> &'static str
where
    F: Fn(&str) -> bool,
{
    CONTAINER_STORAGE_CANDIDATES
        .into_iter()
        .find(|p| exists(p))
        .unwrap_or("/")
}

/// Whether `/var` is its own mount point rather than part of the root
/// filesystem, read from `/proc/mounts`.
///
/// This is the layout where the existing `/sysroot`-based free-space check is
/// most misleading: root can have tens of gigabytes free while a dedicated
/// `/var` volume is far too small for the pull (bootc-migrate#185).
pub fn parse_var_is_separate_mount(proc_mounts: &str) -> bool {
    proc_mounts
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1))
        .any(|mount_point| mount_point == "/var")
}

pub fn check_reflink_support<P: AsRef<Path>>(dir: P) -> bool {
    let src = dir.as_ref().join(".reflink_test_src");
    let dest = dir.as_ref().join(".reflink_test_dest");
    let _ = fs::remove_file(&src);
    let _ = fs::remove_file(&dest);
    let result = (|| -> Result<()> {
        fs::write(&src, b"test")?;
        crate::reflink::reflink(&src, &dest)?;
        Ok(())
    })();
    let _ = fs::remove_file(&src);
    let _ = fs::remove_file(&dest);
    result.is_ok()
}

fn get_ostree_repo_size() -> u64 {
    let ostree_repo = "/sysroot/ostree/repo";
    if !Path::new(ostree_repo).exists() {
        return 0;
    }
    match Command::new("du").args(["-sb", ostree_repo]).output() {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            stdout
                .split_whitespace()
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0)
        }
        Err(_) => 0,
    }
}

/// Generic snapshot of the running system, with no migration-direction
/// judgments — those belong to [`super::validate`]. Fields mirror
/// [`super::PreflightReport`] minus the per-direction verdicts.
#[derive(Debug)]
pub struct SystemInfo {
    /// The backend the running deployment boots from, or `None` if this is not
    /// a bootc deployment.
    pub booted_backend: Option<Backend>,
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
    /// pull of the target image actually lands. Distinct from
    /// `composefs_free_bytes`, which measures the composefs store's filesystem.
    pub container_storage_free_bytes: u64,
    /// Which path `container_storage_free_bytes` was measured on, so the
    /// readiness message can name the mount the user has to enlarge.
    pub container_storage_path: String,
    /// Whether `/var` is a separate mount from the root filesystem.
    pub var_is_separate_mount: bool,
    /// Whether the systemd-boot EFI binaries are installed in the running deployment
    /// (i.e. `/usr/lib/systemd/boot/efi` exists). `bootctl install` requires this.
    pub systemd_boot_binaries_present: bool,
    /// Whether grub2-reboot / grub2-editenv are available.
    pub grub_tools_available: bool,
    pub sysroot_was_ro: bool,
}

impl SystemInfo {
    /// Probe the running system: bootc backend, firmware mode, ESP location
    /// and free space, root filesystem, reflink support, bootloader tooling,
    /// and pending OSTree transactions.
    pub fn gather() -> Result<Self> {
        // 1. Check bootc status
        let output = Command::new("bootc")
            .args(["status", "--json"])
            .output()
            .context("failed to run bootc status")?;
        let booted_backend = if output.status.success() {
            let status: BootcStatus = serde_json::from_slice(&output.stdout)
                .context("failed to parse bootc status json")?;
            status
                .status
                .booted
                .as_ref()
                .and_then(BootedStatus::backend)
        } else {
            None
        };

        // 2. Check UEFI mode
        let is_uefi = Path::new("/sys/firmware/efi").exists();
        let nvram_writable = Path::new("/sys/firmware/efi/efivars").exists();

        // 3. Locate ESP — check mounted first, then try to find by partition GUID
        let mut esp_path = None;
        let mut esp_free_space_bytes = 0u64;
        let mut esp_fs_type = None;
        let mut esp_tmp_mounted = false;

        for path in ["/boot/efi", "/efi", "/boot"] {
            if Path::new(path).exists()
                && let Ok(mounts) = fs::read_to_string("/proc/mounts")
            {
                for line in mounts.lines() {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() >= 3
                        && parts[1] == path
                        && (parts[2] == "vfat" || parts[2] == "msdos")
                    {
                        esp_path = Some(path.to_string());
                        esp_fs_type = Some(parts[2].to_string());
                        if let Ok(free_space) = get_free_space(path) {
                            esp_free_space_bytes = free_space;
                        }
                        break;
                    }
                }
                if esp_path.is_some() {
                    break;
                }
            }
        }

        // ESP not auto-mounted — try to find it by partition type GUID.
        if esp_path.is_none()
            && let Ok(output) = Command::new("lsblk")
                .args(["-o", "NAME,PARTTYPE,FSTYPE,SIZE", "-l", "-n", "-b"])
                .output()
        {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                let parts: Vec<&str> = line.split_whitespace().collect();
                // ESP partition type GUID: C12A7328-F81F-11D2-BA4B-00A0C93EC93B
                if parts.len() >= 2
                    && parts[1].to_lowercase() == "c12a7328-f81f-11d2-ba4b-00a0c93ec93b"
                {
                    let device = format!("/dev/{}", parts[0]);
                    // Temporarily mount to check free space.
                    let tmp_mount = "/var/tmp/esp-preflight";
                    let _ = fs::create_dir_all(tmp_mount);
                    let mount_status = Command::new("mount")
                        .args(["-t", "vfat", &device, tmp_mount])
                        .status();
                    if let Ok(s) = mount_status
                        && s.success()
                    {
                        if let Ok(free_space) = get_free_space(tmp_mount) {
                            esp_free_space_bytes = free_space;
                        }
                        esp_fs_type = Some("vfat".to_string());
                        esp_path = Some(tmp_mount.to_string());
                        esp_tmp_mounted = true;
                        break;
                    }
                }
            }
        }

        let esp_detected = esp_tmp_mounted || esp_path.is_some();

        // Clean up temp mount if we mounted it.
        if esp_tmp_mounted {
            if let Some(ref path) = esp_path {
                let _ = Command::new("umount").arg(path).status();
            }
            esp_path = None; // Not a permanent mount, but esp_detected is still true.
        }

        // 4. Filesystem type
        let sysroot = "/sysroot";
        let mut is_btrfs = false;
        let mut fs_type: Option<String> = None;
        if let Ok(mounts) = fs::read_to_string("/proc/mounts") {
            for line in mounts.lines() {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 3 && parts[1] == sysroot {
                    fs_type = Some(parts[2].to_string());
                    is_btrfs = parts[2] == "btrfs";
                    break;
                }
            }
        }

        // 5. Reflink check — remount /sysroot rw first if needed (OSTree default is ro)
        let sysroot_was_ro = if let Ok(mounts) = fs::read_to_string("/proc/mounts") {
            mounts.lines().any(|line| {
                let parts: Vec<&str> = line.split_whitespace().collect();
                parts.len() >= 4 && parts[1] == sysroot && parts[3].split(',').any(|o| o == "ro")
            })
        } else {
            false
        };

        let supports_reflink = if sysroot_was_ro {
            let _ = Command::new("mount")
                .args(["-o", "remount,rw", sysroot])
                .status();
            let ok = check_reflink_support(sysroot);
            let _ = Command::new("mount")
                .args(["-o", "remount,ro", sysroot])
                .status();
            ok
        } else {
            check_reflink_support(sysroot)
        };

        // 6. GRUB tool availability
        let grub_tools_available = {
            let rb = Command::new("grub2-reboot").arg("--help").output();
            let ee = Command::new("grub2-editenv").arg("--help").output();
            let sd = Command::new("grub2-set-default").arg("--help").output();
            matches!(rb, Ok(o) if o.status.success())
                || matches!(ee, Ok(o) if o.status.success())
                || matches!(sd, Ok(o) if o.status.success())
        };

        // 7. Free-space data
        let ostree_repo_size_bytes = get_ostree_repo_size();
        let pending_transaction = check_pending_ostree_transaction();
        let composefs_free_bytes = {
            let base = if Path::new("/sysroot/composefs").exists() {
                "/sysroot/composefs"
            } else {
                "/sysroot"
            };
            get_free_space(base).unwrap_or(0)
        };

        // Where the Phase-2 podman pull lands — a different filesystem from
        // the composefs store whenever /var is its own volume.
        let container_storage_path =
            select_container_storage_path(|p| Path::new(p).exists()).to_string();
        let container_storage_free_bytes = get_free_space(&container_storage_path).unwrap_or(0);
        let var_is_separate_mount = fs::read_to_string("/proc/mounts")
            .map(|m| parse_var_is_separate_mount(&m))
            .unwrap_or(false);

        let systemd_boot_binaries_present = Path::new("/usr/lib/systemd/boot/efi").exists();

        Ok(SystemInfo {
            booted_backend,
            pending_transaction,
            is_uefi,
            nvram_writable,
            esp_path,
            esp_free_space_bytes,
            esp_fs_type,
            supports_reflink,
            is_btrfs,
            fs_type,
            ostree_repo_size_bytes,
            composefs_free_bytes,
            container_storage_free_bytes,
            container_storage_path,
            var_is_separate_mount,
            esp_detected,
            systemd_boot_binaries_present,
            grub_tools_available,
            sysroot_was_ro,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pull follows podman's storage path, so the most specific existing
    /// candidate wins — measuring "/" when /var is a small separate volume is
    /// exactly the mistake bootc-migrate#185 describes.
    #[test]
    fn container_storage_path_prefers_the_most_specific_existing_candidate() {
        assert_eq!(
            select_container_storage_path(|_| true),
            "/var/lib/containers/storage"
        );
        assert_eq!(
            select_container_storage_path(|p| p != "/var/lib/containers/storage"),
            "/var"
        );
        assert_eq!(select_container_storage_path(|p| p == "/"), "/");
        // Nothing exists (shouldn't happen on a real host): fall back, no panic.
        assert_eq!(select_container_storage_path(|_| false), "/");
    }

    #[test]
    fn separate_var_mount_is_detected_from_proc_mounts() {
        let with_var = "/dev/mapper/vg-root / xfs rw,relatime 0 0\n\
                        /dev/mapper/vg-var /var xfs rw,relatime 0 0\n\
                        proc /proc proc rw 0 0\n";
        assert!(parse_var_is_separate_mount(with_var));

        let without_var = "/dev/mapper/vg-root / xfs rw,relatime 0 0\n\
                           proc /proc proc rw 0 0\n";
        assert!(!parse_var_is_separate_mount(without_var));

        // A nested path under /var is not itself a /var mount.
        let nested_only = "/dev/sda1 /var/lib/containers xfs rw 0 0\n";
        assert!(!parse_var_is_separate_mount(nested_only));

        assert!(!parse_var_is_separate_mount(""));
    }

    // --- parse_ostree_status_for_pending (#153) ---

    #[test]
    fn ostree_status_pending_detection_table() {
        let cases: &[(&str, PendingTransactionStatus, &str)] = &[
            ("", PendingTransactionStatus::Clean, "empty output"),
            (
                "* bluefin abc123.0\n    Version: 41.20250101\n",
                PendingTransactionStatus::Clean,
                "a booted deployment alone is clean",
            ),
            (
                "  bluefin abc123.0 (staged)\n",
                PendingTransactionStatus::StagedDeployment,
                "staged marker",
            ),
            (
                "  bluefin abc123.0 (pending)\n",
                PendingTransactionStatus::PendingDeployment,
                "pending marker",
            ),
            (
                "* bluefin abc.0\n  bluefin def.1 (staged)\n",
                PendingTransactionStatus::StagedDeployment,
                "marker found on a later line",
            ),
            (
                "      bluefin abc123.0 (staged)      \n",
                PendingTransactionStatus::StagedDeployment,
                "leading/trailing whitespace is trimmed",
            ),
            (
                "* bluefin abc123.0\n    Version: 41 (staged elsewhere)\n",
                PendingTransactionStatus::Clean,
                "substring must be the literal marker, not merely similar",
            ),
        ];
        for (input, want, why) in cases {
            let got = parse_ostree_status_for_pending(input);
            assert_eq!(
                std::mem::discriminant(&got),
                std::mem::discriminant(want),
                "{why}: got {got:?} for {input:?}"
            );
        }
    }

    /// Precedence is by LINE ORDER, not by severity — the scan returns on the
    /// first marker it meets. Pinned because it is a real behavioral choice
    /// that the docstring does not state, and either ordering would look
    /// plausible to someone editing this later.
    #[test]
    fn ostree_status_precedence_is_first_line_wins() {
        let pending_first = "  a (pending)\n  b (staged)\n";
        assert!(matches!(
            parse_ostree_status_for_pending(pending_first),
            PendingTransactionStatus::PendingDeployment
        ));

        let staged_first = "  a (staged)\n  b (pending)\n";
        assert!(matches!(
            parse_ostree_status_for_pending(staged_first),
            PendingTransactionStatus::StagedDeployment
        ));
    }

    /// These strings reach users in the preflight report and in the refusal
    /// that blocks a migration, so they are API.
    #[test]
    fn pending_status_display_strings() {
        assert_eq!(
            PendingTransactionStatus::Clean.to_string(),
            "no pending transaction"
        );
        assert_eq!(
            PendingTransactionStatus::StagedDeployment.to_string(),
            "staged deployment (next boot will apply)"
        );
        assert_eq!(
            PendingTransactionStatus::PendingDeployment.to_string(),
            "pending deployment (update in progress)"
        );
        assert_eq!(
            PendingTransactionStatus::StaleTransactionFiles.to_string(),
            "stale transaction temp files in OSTree repo"
        );
    }

    // --- count_composefs_files (#153) ---

    #[test]
    fn composefs_object_count_walks_the_two_level_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let objects = tmp.path().join("objects");
        for (prefix, n) in [("ab", 3), ("cd", 2), ("ef", 1)] {
            let d = objects.join(prefix);
            fs::create_dir_all(&d).unwrap();
            for i in 0..n {
                fs::write(d.join(format!("obj{i}")), b"x").unwrap();
            }
        }
        assert_eq!(count_composefs_files(&objects), 6);
    }

    #[test]
    fn composefs_object_count_ignores_loose_files_and_deeper_nesting() {
        let tmp = tempfile::tempdir().unwrap();
        let objects = tmp.path().join("objects");
        fs::create_dir_all(objects.join("ab")).unwrap();
        fs::write(objects.join("ab").join("real"), b"x").unwrap();
        // A file directly under objects/ is not in the two-level layout.
        fs::write(objects.join("stray"), b"x").unwrap();
        // A third level is not counted either — only files one level down.
        fs::create_dir_all(objects.join("ab").join("deeper")).unwrap();
        fs::write(objects.join("ab").join("deeper").join("nested"), b"x").unwrap();

        assert_eq!(count_composefs_files(&objects), 1);
    }

    /// A missing store is the normal pre-migration state, not an error — the
    /// count is used for sizing, and returning 0 keeps the caller simple.
    #[test]
    fn composefs_object_count_on_missing_root_is_zero_not_a_panic() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(count_composefs_files(&tmp.path().join("absent")), 0);
        // An empty store is likewise zero.
        let empty = tmp.path().join("empty");
        fs::create_dir_all(&empty).unwrap();
        assert_eq!(count_composefs_files(&empty), 0);
    }

    // --- get_free_space (#153) ---

    #[test]
    fn free_space_reports_a_plausible_figure_and_errors_on_missing_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let free = get_free_space(tmp.path()).expect("statvfs on a real dir");
        assert!(free > 0, "a writable tempdir should report free space");

        assert!(
            get_free_space(tmp.path().join("does-not-exist")).is_err(),
            "statvfs on a missing path must be an error, not a silent 0 — \
             callers use unwrap_or(0) and a silent 0 would read as 'full'"
        );
    }

    // --- check_reflink_support (#153) ---

    /// Whatever the filesystem answers, the probe must not leave its scratch
    /// files behind: it runs against the user's real /sysroot.
    #[test]
    fn reflink_probe_cleans_up_after_itself() {
        let tmp = tempfile::tempdir().unwrap();
        let _ = check_reflink_support(tmp.path());

        let leftovers: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            leftovers.is_empty(),
            "probe left scratch files behind: {leftovers:?}"
        );
    }

    #[test]
    fn reflink_probe_on_an_unwritable_location_is_false_not_a_panic() {
        assert!(!check_reflink_support(Path::new("/proc/nonexistent-dir")));
    }

    /// The detection this replaced inferred "composefs" from "not ostree",
    /// which also matched a host with no bootc deployment at all. These parse
    /// the shapes `bootc status --json` actually emits.
    fn backend_of(json: &str) -> Option<Backend> {
        let status: BootcStatus = serde_json::from_str(json).expect("parse");
        status
            .status
            .booted
            .as_ref()
            .and_then(BootedStatus::backend)
    }

    #[test]
    fn ostree_deployment_reports_ostree() {
        assert_eq!(
            backend_of(
                r#"{"apiVersion":"org.containers.bootc/v1","kind":"BootcHost",
                    "status":{"booted":{"ostree":{"checksum":"abc"}}}}"#
            ),
            Some(Backend::Ostree)
        );
    }

    #[test]
    fn composefs_deployment_reports_composefs_not_none() {
        assert_eq!(
            backend_of(
                r#"{"apiVersion":"org.containers.bootc/v1","kind":"BootcHost",
                    "status":{"booted":{"composefs":{"verity":"abc"}}}}"#
            ),
            Some(Backend::Composefs)
        );
    }

    /// The case the old check could not distinguish from a composefs host.
    #[test]
    fn deployment_with_neither_key_is_not_a_bootc_host() {
        assert_eq!(
            backend_of(
                r#"{"apiVersion":"org.containers.bootc/v1","kind":"BootcHost",
                    "status":{"booted":{}}}"#
            ),
            None
        );
    }

    #[test]
    fn nothing_booted_is_not_a_bootc_host() {
        assert_eq!(
            backend_of(
                r#"{"apiVersion":"org.containers.bootc/v1","kind":"BootcHost",
                    "status":{"booted":null}}"#
            ),
            None
        );
    }

    /// Both keys present: ostree wins, because a deployment that still has an
    /// ostree checksum has an ostree repo to convert.
    #[test]
    fn ostree_wins_when_both_keys_are_present() {
        assert_eq!(
            backend_of(
                r#"{"apiVersion":"org.containers.bootc/v1","kind":"BootcHost",
                    "status":{"booted":{"ostree":{"checksum":"a"},"composefs":{"verity":"b"}}}}"#
            ),
            Some(Backend::Ostree)
        );
    }
}
