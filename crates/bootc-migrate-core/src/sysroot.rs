//! Make `/sysroot` writable for as long as a step writes into it, without
//! touching the filesystem under it.
//!
//! On an OSTree boot `/sysroot` is read-only through its own mount flag,
//! while the filesystem under it stays writable: the deployment's `/etc` and
//! the stateroot `/var` are bind mounts of it. A plain `remount,ro` changes
//! the filesystem instead, so on a quiet system it can make `/`, `/etc` and
//! `/var` read-only under the running OS (on a busy one it fails with EBUSY).
//! `bind` limits each change to the `/sysroot` mount itself, as ostree does.

use anyhow::{Context, Result, anyhow};
use std::fs;
use std::process::Command;

/// The physical root on an OSTree or composefs boot.
pub const SYSROOT: &str = "/sysroot";

/// `mount` arguments that flip only the read-only flag of the mount at
/// `path`.
pub(crate) fn remount_args(path: &str, read_only: bool) -> [&str; 3] {
    let opts = if read_only {
        "remount,bind,ro"
    } else {
        "remount,bind,rw"
    };
    ["-o", opts, path]
}

/// Whether the mount at `path` in a `/proc/mounts` listing is read-only.
/// The last entry for `path` wins, as it is the one on top.
pub(crate) fn mount_is_read_only(mounts: &str, path: &str) -> bool {
    mounts
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            (parts.len() >= 4 && parts[1] == path).then(|| parts[3].split(',').any(|o| o == "ro"))
        })
        .next_back()
        .unwrap_or(false)
}

/// Holds `/sysroot` writable and puts its read-only flag back on drop.
///
/// The routes that write into the staged deployment after `bootc switch`
/// (OstreeDeploy, ImageSwap) take one. They used to depend on the preflight
/// reflink probe leaving `/sysroot` writable, which it only did when its
/// plain `remount,ro` failed.
#[derive(Debug)]
pub struct SysrootWritable {
    path: &'static str,
    restore: bool,
}

impl SysrootWritable {
    /// Make `path` writable if its mount is read-only. A mount that is
    /// already writable is left as it is, and stays so on drop.
    pub fn acquire(path: &'static str) -> Result<Self> {
        let mounts = fs::read_to_string("/proc/mounts").context("failed to read /proc/mounts")?;
        if !mount_is_read_only(&mounts, path) {
            return Ok(Self {
                path,
                restore: false,
            });
        }
        let status = Command::new("mount")
            .args(remount_args(path, false))
            .status()
            .with_context(|| format!("failed to run mount for {path}"))?;
        if !status.success() {
            return Err(anyhow!(
                "failed to make {path} writable (mount -o remount,bind,rw)"
            ));
        }
        Ok(Self {
            path,
            restore: true,
        })
    }
}

impl Drop for SysrootWritable {
    fn drop(&mut self) {
        if !self.restore {
            return;
        }
        let ok = Command::new("mount")
            .args(remount_args(self.path, true))
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            eprintln!(
                "Warning: could not make {} read-only again; it stays writable until the next boot",
                self.path
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remount_only_touches_the_mount_flag() {
        for (read_only, opts) in [(false, "remount,bind,rw"), (true, "remount,bind,ro")] {
            assert_eq!(
                remount_args("/sysroot", read_only),
                ["-o", opts, "/sysroot"]
            );
        }
    }

    #[test]
    fn read_only_is_read_from_the_topmost_mount() {
        let cases = [
            ("/dev/vda3 /sysroot xfs ro,relatime 0 0\n", true),
            ("/dev/vda3 /sysroot xfs rw,relatime 0 0\n", false),
            // "ro" must be a whole option, not a prefix of another one.
            ("/dev/vda3 /sysroot xfs rw,rootcontext=x 0 0\n", false),
            // The later (topmost) entry wins.
            (
                "/dev/vda3 /sysroot xfs rw,relatime 0 0\n/dev/vda3 /sysroot xfs ro,relatime 0 0\n",
                true,
            ),
            // Another mount point does not count.
            ("/dev/vda3 /sysroot/ostree xfs ro 0 0\n", false),
            ("", false),
        ];
        for (mounts, want) in cases {
            assert_eq!(mount_is_read_only(mounts, "/sysroot"), want, "{mounts:?}");
        }
    }
}
