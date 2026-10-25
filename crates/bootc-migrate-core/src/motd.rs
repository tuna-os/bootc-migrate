//! Login reminder nudging the user to run `commit` after a migration.
//!
//! Written as a single fragment under `/etc/motd.d/` (picked up by
//! `pam_motd` on every login, per `motd(5)`) so people who migrate and then
//! never come back to `commit` don't stay indefinitely in the dual-boot
//! "limbo" state — OSTree fallback kept around, ~14 GiB unreclaimed. The
//! fragment is self-clearing: `commit` deletes it as its last step.

use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

const MOTD_DIR: &str = "/etc/motd.d";
const FRAGMENT_NAME: &str = "85-bootc-migrate";

/// Write the reminder fragment after a successful (non-dry-run) migration.
/// Best-effort: failures are the caller's to decide whether to surface,
/// since this runs after the migration itself has already succeeded.
pub fn write_migration_reminder(verity_hex: &str) -> Result<()> {
    write_migration_reminder_at(Path::new(MOTD_DIR), verity_hex)
}
/// Write the package host migration reminder fragment after a successful migration.
pub fn write_package_migration_reminder(target_image: &str) -> Result<()> {
    write_package_migration_reminder_at(Path::new(MOTD_DIR), target_image)
}

/// Write the package host migration reminder fragment to an explicit `motd_dir`.
pub fn write_package_migration_reminder_at(motd_dir: &Path, target_image: &str) -> Result<()> {
    fs::create_dir_all(motd_dir)
        .with_context(|| format!("failed to create {}", motd_dir.display()))?;
    let contents = format!(
        "\n\
         *** bootc-migrate: this system was migrated from a package host to bootc ({target_image}) ***\n\
         Once you've confirmed it boots and works, run:\n\
         \n\
         \tsudo bootc-rebase commit\n\
         \n\
         to finalize the switch and reclaim the old operating system.\n\
         This reminder clears itself once you do.\n"
    );
    fs::write(motd_dir.join(FRAGMENT_NAME), contents)
        .with_context(|| format!("failed to write reminder to {}", motd_dir.display()))
}

/// Remove the reminder fragment. Idempotent — a missing file is not an
/// error, since `commit` may run without a prior reminder ever having been
/// written (e.g. an already-committed system, or one migrated before this
/// feature existed).
pub fn clear_migration_reminder() -> Result<()> {
    clear_migration_reminder_at(Path::new(MOTD_DIR))
}

fn write_migration_reminder_at(motd_dir: &Path, verity_hex: &str) -> Result<()> {
    fs::create_dir_all(motd_dir)
        .with_context(|| format!("failed to create {}", motd_dir.display()))?;
    let contents = format!(
        "\n\
         *** bootc-migrate: this system was migrated to ComposeFS ({verity_hex}) ***\n\
         Once you've confirmed it boots and works, run:\n\
         \n\
         \tsudo bootc-migrate commit\n\
         \n\
         to finalize the switch and reclaim the OSTree object store (~several GiB).\n\
         This reminder clears itself once you do.\n"
    );
    fs::write(motd_dir.join(FRAGMENT_NAME), contents)
        .with_context(|| format!("failed to write reminder to {}", motd_dir.display()))
}

fn clear_migration_reminder_at(motd_dir: &Path) -> Result<()> {
    match fs::remove_file(motd_dir.join(FRAGMENT_NAME)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).context("failed to remove migration reminder"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_clear_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let motd_dir = tmp.path().join("motd.d");

        write_migration_reminder_at(&motd_dir, "deadbeef").unwrap();
        let fragment = motd_dir.join(FRAGMENT_NAME);
        assert!(fragment.exists());
        let contents = fs::read_to_string(&fragment).unwrap();
        assert!(contents.contains("deadbeef"));
        assert!(contents.contains("bootc-migrate commit"));

        clear_migration_reminder_at(&motd_dir).unwrap();
        assert!(!fragment.exists());
    }
    #[test]
    fn write_package_migration_reminder_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let motd_dir = tmp.path().join("motd.d");

        write_package_migration_reminder_at(&motd_dir, "quay.io/fedora/fedora-bootc:43").unwrap();
        let fragment = motd_dir.join(FRAGMENT_NAME);
        assert!(fragment.exists());
        let contents = fs::read_to_string(&fragment).unwrap();
        assert!(contents.contains("quay.io/fedora/fedora-bootc:43"));
        assert!(contents.contains("bootc-rebase commit"));

        clear_migration_reminder_at(&motd_dir).unwrap();
        assert!(!fragment.exists());
    }

    #[test]
    fn clear_is_idempotent_when_no_fragment_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let motd_dir = tmp.path().join("motd.d");
        // Never written — clearing a nonexistent fragment must not error.
        clear_migration_reminder_at(&motd_dir).unwrap();
    }

    #[test]
    fn write_creates_motd_dir_if_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let motd_dir = tmp.path().join("nested").join("motd.d");
        assert!(!motd_dir.exists());
        write_migration_reminder_at(&motd_dir, "abc123").unwrap();
        assert!(motd_dir.join(FRAGMENT_NAME).exists());
    }
}
