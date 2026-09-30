//! Process lifecycle for a migration run: the exclusive lock, the sleep
//! inhibitor, and the read-write remounts that make the system mutable.
//!
//! [`MigrationLifecycle`] owns all three as one value, so acquisition and
//! cleanup have a single place. A dry run acquires nothing and remounts
//! nothing — it only says what it would have done.

use anyhow::{Context, Result, anyhow};
use rustix::fs::{FlockOperation, flock};
use rustix::io::Errno;
use std::fs::File;
use std::io::Write;
use std::process::Command;

const LOCK_PATH: &str = "/var/run/bootc-migrate.lock";

/// Filesystems that must be writable for a migration to proceed.
const REMOUNT_RW_TARGETS: [&str; 2] = ["/sysroot", "/boot"];

fn acquire_lock() -> Result<File> {
    acquire_exclusive_lock_at(std::path::Path::new(LOCK_PATH))
}

/// The exclusive migration lock on its own, for routes that manage their own
/// sleep guard and remounts ([`crate::ostree_install`]). Same lock file as
/// [`MigrationLifecycle`], so the two binaries exclude each other (#303).
/// The message names bootc-migrate either way: that text is frozen output.
pub(crate) fn acquire_exclusive_lock() -> Result<File> {
    acquire_exclusive_lock_at(std::path::Path::new(LOCK_PATH))
}

fn acquire_exclusive_lock_at(path: &std::path::Path) -> Result<File> {
    let lock = File::create(path).context("failed to create lock file")?;
    // Non-blocking exclusive advisory lock, released when this fd is closed
    // (i.e. on process exit). Guards against concurrent migration runs.
    match flock(&lock, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {}
        Err(Errno::WOULDBLOCK | Errno::ACCESS) => {
            return Err(anyhow!(
                "Another instance of bootc-migrate is already running (lock held at {}).",
                path.display()
            ));
        }
        Err(e) => return Err(e).context("failed to acquire lock"),
    }
    // Write PID so admins can inspect.
    let _ = writeln!(&lock, "{}", std::process::id());
    Ok(lock)
}

/// Remount `target` read-write. Migration cannot proceed on a read-only
/// `/sysroot` or `/boot`, so a failure here is fatal rather than a warning.
fn remount_rw(target: &str) -> Result<()> {
    let status = Command::new("/usr/bin/mount")
        .args(["-o", "remount,rw", target])
        .status()
        .with_context(|| format!("failed to execute mount remount,rw {target}"))?;
    if !status.success() {
        return Err(anyhow!(
            "failed to remount {target} read-write — cannot proceed with migration"
        ));
    }
    Ok(())
}

/// Inhibits system sleep/suspend during migration using systemd-inhibit if available (issue #27).
#[derive(Debug)]
pub struct SleepGuard {
    child: Option<std::process::Child>,
}

impl SleepGuard {
    pub fn new(why: &str) -> Self {
        let child = Command::new("systemd-inhibit")
            .args([
                "--what=sleep",
                &format!("--why={why}"),
                "--mode=block",
                "sleep",
                "infinity",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok();

        if child.is_some() {
            println!("Acquired systemd sleep inhibitor lock.");
        } else {
            eprintln!("Note: systemd-inhibit unavailable; sleep inhibitor lock was not acquired.");
        }

        SleepGuard { child }
    }
}

impl Drop for SleepGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
            println!("Released systemd sleep inhibitor lock.");
        }
    }
}

/// The mutation guards held for the duration of a migration run.
///
/// Both guards release on drop, so holding this value for the length of
/// [`super::run_migration`] is the whole contract. A dry run holds neither.
pub(crate) struct MigrationLifecycle {
    _lock: Option<File>,
    _sleep: Option<SleepGuard>,
}

impl MigrationLifecycle {
    /// Take the exclusive lock and sleep inhibitor, then remount `/sysroot`
    /// and `/boot` read-write.
    ///
    /// A dry run acquires no guards and remounts nothing; it reports what a
    /// real run would have done and returns an inert value.
    pub(crate) fn acquire(dry_run: bool) -> Result<Self> {
        if dry_run {
            println!("[DRY RUN] Would execute migration phases without making changes.");
            println!("[DRY RUN] Would remount /sysroot and /boot read-write.");
            return Ok(Self {
                _lock: None,
                _sleep: None,
            });
        }

        let lock = acquire_lock()?;
        let sleep = SleepGuard::new("OSTree to ComposeFS migration in progress");
        for target in REMOUNT_RW_TARGETS {
            remount_rw(target)?;
        }
        Ok(Self {
            _lock: Some(lock),
            _sleep: Some(sleep),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sleep_guard_creation_and_drop() {
        let guard = SleepGuard::new("unit test migration");
        drop(guard);
    }

    /// A dry run must not take the lock, spawn an inhibitor, or remount
    /// anything — it is the one mode that is safe to run on a live system.
    #[test]
    fn dry_run_lifecycle_acquires_nothing() {
        let lifecycle = MigrationLifecycle::acquire(true).unwrap();
        assert!(lifecycle._lock.is_none(), "dry run must not take the lock");
        assert!(
            lifecycle._sleep.is_none(),
            "dry run must not hold a sleep inhibitor"
        );
    }

    /// Two opens of the same lock file conflict: the second run refuses
    /// instead of racing the first (#303). Uses a tempdir file, never the
    /// production lock — tests must not fence out a live migration.
    #[test]
    fn exclusive_lock_refuses_a_second_holder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test-migrate.lock");
        let _first = acquire_exclusive_lock_at(&path).unwrap();
        let err = acquire_exclusive_lock_at(&path).unwrap_err();
        assert!(
            format!("{err:#}").contains("already running"),
            "unexpected error: {err:#}"
        );
    }
}
