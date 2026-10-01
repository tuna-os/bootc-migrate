//! Process lifecycle for a migration run: the exclusive lock, the sleep
//! inhibitor, and the read-write remounts that make the system mutable.
//!
//! [`MigrationLifecycle`] owns all three as one value, so acquisition and
//! cleanup have a single place. A dry run acquires nothing and remounts
//! nothing — it only says what it would have done.

use anyhow::{Context, Result, anyhow};
use rustix::fs::{FlockOperation, flock};
use rustix::io::Errno;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::process::Command;

const LOCK_PATH: &str = "/var/run/bootc-migrate.lock";

/// Filesystems that must be writable for a migration to proceed.
const REMOUNT_RW_TARGETS: [&str; 2] = ["/sysroot", "/boot"];

/// Who holds the run lock, as recorded in the lock file by the holder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LockHolder {
    pub pid: u32,
    /// Unix time (seconds) at which the holder took the lock.
    pub started: u64,
}

impl LockHolder {
    fn current() -> Self {
        let started = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self {
            pid: std::process::id(),
            started,
        }
    }

    /// The lock file's contents: `pid=<pid> started=<unix seconds>`.
    fn record(&self) -> String {
        format!("pid={} started={}\n", self.pid, self.started)
    }

    /// Parse a lock file's contents. A bare PID (the format before #303)
    /// parses with `started == 0`; anything else is `None`.
    pub(crate) fn parse(contents: &str) -> Option<Self> {
        let contents = contents.trim();
        if let Ok(pid) = contents.parse::<u32>() {
            return Some(Self { pid, started: 0 });
        }
        let mut pid = None;
        let mut started = None;
        for field in contents.split_whitespace() {
            match field.split_once('=') {
                Some(("pid", v)) => pid = v.parse().ok(),
                Some(("started", v)) => started = v.parse().ok(),
                _ => {}
            }
        }
        Some(Self {
            pid: pid?,
            started: started?,
        })
    }

    fn describe(&self) -> String {
        if self.started == 0 {
            format!("pid {}", self.pid)
        } else {
            format!("pid {}, started at unix time {}", self.pid, self.started)
        }
    }
}

/// Take the process-wide run lock at [`LOCK_PATH`]. Every mutating route
/// (the forward migration and `Strategy::OstreeInstall`) holds it for its
/// whole run, so two invocations cannot interleave their phases.
pub(crate) fn acquire_lock() -> Result<RunLock> {
    acquire_lock_at(Path::new(LOCK_PATH))
}

/// Take a non-blocking exclusive `flock` on `path` and record the holder.
///
/// The kernel drops the lock when the holder's fd closes — on exit, on a
/// crash or on `kill -9` — so a dead holder can never wedge a re-run: its
/// record stays in the file, but the lock is free and the next run takes
/// it and overwrites the record. A refusal therefore always names a live
/// holder. A clean exit clears the record (see [`RunLock`]).
fn acquire_lock_at(path: &Path) -> Result<RunLock> {
    // Open without truncating: the file may hold the live holder's record,
    // and the refusal below reads it.
    let mut lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("failed to open lock file {}", path.display()))?;
    match flock(&lock, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {}
        Err(Errno::WOULDBLOCK | Errno::ACCESS) => {
            let mut contents = String::new();
            let _ = lock.read_to_string(&mut contents);
            let holder = match LockHolder::parse(&contents) {
                Some(h) => h.describe(),
                None => "holder not yet recorded".to_string(),
            };
            return Err(anyhow!(
                "Another instance of bootc-migrate is already running ({holder}; lock held \
                 at {}). Wait for it to finish; this run made no changes.",
                path.display()
            ));
        }
        Err(e) => return Err(e).context("failed to acquire lock"),
    }
    let mut previous = String::new();
    let _ = lock.read_to_string(&mut previous);
    if let Some(stale) = LockHolder::parse(&previous) {
        eprintln!(
            "Note: reclaiming the lock at {} from a previous run that is no longer \
             running ({}).",
            path.display(),
            stale.describe()
        );
    }
    // Record the holder so a concurrent run can name it.
    lock.set_len(0).context("failed to truncate lock file")?;
    lock.seek(SeekFrom::Start(0))?;
    lock.write_all(LockHolder::current().record().as_bytes())
        .context("failed to record lock holder")?;
    Ok(RunLock { file: lock })
}

/// A held run lock. Dropping it clears the holder record and closes the
/// fd, which releases the `flock`; a killed holder skips the clear, and the
/// next run reports the record it reclaims.
#[derive(Debug)]
pub(crate) struct RunLock {
    file: File,
}

impl Drop for RunLock {
    fn drop(&mut self) {
        let _ = self.file.set_len(0);
    }
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
    _lock: Option<RunLock>,
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

    #[test]
    fn lock_holder_record_round_trips() {
        let cases: &[(&str, Option<LockHolder>)] = &[
            (
                "pid=4242 started=1790000000\n",
                Some(LockHolder {
                    pid: 4242,
                    started: 1790000000,
                }),
            ),
            // The bare-PID record written before #303.
            (
                "4242\n",
                Some(LockHolder {
                    pid: 4242,
                    started: 0,
                }),
            ),
            ("", None),
            ("pid=4242", None),
            ("garbage", None),
        ];
        for (contents, want) in cases {
            assert_eq!(LockHolder::parse(contents), *want, "{contents:?}");
        }
        let holder = LockHolder::current();
        assert_eq!(LockHolder::parse(&holder.record()), Some(holder));
    }

    /// A live holder's lock refuses a second acquisition, and the refusal
    /// names the holder's pid and start time. `flock` locks belong to the
    /// open file description, so a second open in this process conflicts
    /// exactly like a second process would.
    #[test]
    fn live_holder_refuses_second_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.lock");
        let held = acquire_lock_at(&path).unwrap();
        let holder = LockHolder::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(holder.pid, std::process::id());

        let err = acquire_lock_at(&path).unwrap_err().to_string();
        assert!(err.contains("already running"), "{err}");
        assert!(err.contains(&format!("pid {}", holder.pid)), "{err}");
        assert!(err.contains(&holder.started.to_string()), "{err}");
        // The refused run must not have erased the holder's record.
        assert_eq!(
            LockHolder::parse(&std::fs::read_to_string(&path).unwrap()),
            Some(holder)
        );

        // A clean release clears the record and frees the lock.
        drop(held);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
        acquire_lock_at(&path).unwrap();
    }

    /// A record left by a killed run, with no fd holding the lock, must not
    /// wedge the next run: it takes the lock and overwrites the record.
    #[test]
    fn stale_holder_record_is_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.lock");
        std::fs::write(&path, "pid=999999 started=1\n").unwrap();

        let _held = acquire_lock_at(&path).unwrap();
        let holder = LockHolder::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(holder.pid, std::process::id());
        assert_ne!(holder.started, 1);
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
}
