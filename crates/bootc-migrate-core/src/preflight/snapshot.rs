//! Persist a [`super::PreflightReport`] to disk so a support case or a bug
//! report can be diagnosed from the same structured state
//! [`super::readiness::print_report`] renders for a human, instead of
//! terminal prose with no stable field names.
//!
//! This is the machine-readable half of the pair the README promises: the
//! run log captures rendered text, this captures the same data as JSON.
//!
//! **Best-effort by design** (bootc-migrate#229): a migration must not be
//! blocked because `/var/log` is full or read-only, so every failure here is
//! a warning, never a propagated error — exactly how `bootc-migrate`'s
//! `main.rs` treats its own run-log file.

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::PreflightReport;

/// Where preflight snapshots are written. A directory (rather than a single
/// overwritten file) so concurrent or repeated runs do not clobber each
/// other's diagnostic state — matching what the README originally promised.
pub const SNAPSHOT_DIR: &str = "/var/log/bootc-migrate";

/// [`PreflightReport`] plus the identity fields that tie a snapshot to a
/// specific run and its log line, so a support case can line the two up.
#[derive(Debug, Serialize)]
struct PreflightSnapshot<'a> {
    /// Name of the binary that ran preflight (`bootc-migrate`, `bootc-rebase`).
    binary: &'a str,
    /// Build/version identifier — a git hash where the binary has one
    /// (`bootc-migrate`), otherwise its crate version.
    version: &'a str,
    /// Command-line arguments the run was invoked with, `argv[0]` included.
    args: &'a [String],
    captured_at_unix_secs: u64,
    #[serde(flatten)]
    report: &'a PreflightReport,
}

/// Seconds since the epoch, or 0 if the clock is before it (a badly-set RTC
/// is the only way this happens — a useless-but-harmless filename beats
/// refusing to write the snapshot at all).
fn now_unix_secs() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs(),
        Err(e) => {
            eprintln!(
                "Warning: system clock is before the Unix epoch ({e}); \
                 naming this preflight snapshot 0."
            );
            0
        }
    }
}

/// Snapshot filename for a given capture time.
fn snapshot_filename(captured_at_unix_secs: u64) -> String {
    format!("preflight-{captured_at_unix_secs}.json")
}

/// Serialize `report` and write it under [`SNAPSHOT_DIR`], creating the
/// directory if it does not exist yet.
///
/// Best-effort: on any failure (directory creation, serialization, write)
/// this prints a warning and returns `None` rather than an error — the same
/// treatment `bootc-migrate`'s run log gets when `/var/log` is unwritable.
/// Callers should not `?` this; it is not allowed to abort a migration.
pub fn write_snapshot(
    binary: &str,
    version: &str,
    args: &[String],
    report: &PreflightReport,
) -> Option<PathBuf> {
    write_snapshot_to(Path::new(SNAPSHOT_DIR), binary, version, args, report)
}

/// [`write_snapshot`] against an arbitrary directory — split out so tests
/// can exercise real I/O without touching `/var/log`.
fn write_snapshot_to(
    dir: &Path,
    binary: &str,
    version: &str,
    args: &[String],
    report: &PreflightReport,
) -> Option<PathBuf> {
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!(
            "Warning: could not create {} for preflight snapshot: {e}",
            dir.display()
        );
        return None;
    }

    let snapshot = PreflightSnapshot {
        binary,
        version,
        args,
        captured_at_unix_secs: now_unix_secs(),
        report,
    };

    let json = match serde_json::to_string_pretty(&snapshot) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("Warning: could not serialize preflight report: {e}");
            return None;
        }
    };

    let path = dir.join(snapshot_filename(snapshot.captured_at_unix_secs));
    if let Err(e) = std::fs::write(&path, json) {
        eprintln!(
            "Warning: could not write preflight snapshot to {}: {e}",
            path.display()
        );
        return None;
    }

    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preflight::PendingTransactionStatus;
    use crate::rebase_plan::Backend;

    fn sample_report() -> PreflightReport {
        PreflightReport {
            booted_backend: Some(Backend::Ostree),
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
        }
    }

    #[test]
    fn snapshot_filename_embeds_timestamp() {
        assert_eq!(
            snapshot_filename(1_700_000_000),
            "preflight-1700000000.json"
        );
    }

    #[test]
    fn snapshot_serializes_identity_and_report_fields() {
        // write_snapshot itself always targets SNAPSHOT_DIR, so exercise the
        // serialization shape directly rather than duplicating its I/O
        // against a real path.
        let report = sample_report();
        let args = vec!["bootc-migrate".to_string(), "--target-image".to_string()];
        let snapshot = PreflightSnapshot {
            binary: "bootc-migrate",
            version: "abc123",
            args: &args,
            captured_at_unix_secs: 42,
            report: &report,
        };
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(json.contains("\"binary\":\"bootc-migrate\""));
        assert!(json.contains("\"version\":\"abc123\""));
        assert!(json.contains("\"captured_at_unix_secs\":42"));
        // #[serde(flatten)] on `report` should put its fields at the top
        // level, not nested under a "report" key.
        assert!(json.contains("\"booted_backend\":\"Ostree\""));
        assert!(!json.contains("\"report\":"));
    }

    #[test]
    fn write_snapshot_to_creates_dir_and_file() {
        let tmp = tempfile::tempdir().unwrap();
        // Nested, not-yet-existing dir: write_snapshot_to must create it,
        // same as write_snapshot does for /var/log/bootc-migrate.
        let dir = tmp.path().join("bootc-migrate");
        let report = sample_report();
        let args = vec!["bootc-rebase".to_string(), "--target-image".to_string()];

        let path = write_snapshot_to(&dir, "bootc-rebase", "0.6.0", &args, &report)
            .expect("write_snapshot_to should succeed against a writable temp dir");

        assert!(path.starts_with(&dir));
        assert!(
            path.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("preflight-")
        );
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("\"binary\": \"bootc-rebase\""));
        assert!(contents.contains("\"booted_backend\": \"Ostree\""));
    }

    #[test]
    fn write_snapshot_targets_the_documented_directory() {
        assert_eq!(SNAPSHOT_DIR, "/var/log/bootc-migrate");
    }
}
