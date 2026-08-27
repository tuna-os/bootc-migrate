//! Host-side ComposeFS storage preparation: how much room the migration
//! needs, and the ext4 loopback store that filesystems without fs-verity
//! (XFS) require.
//!
//! Split out of `migration::mod` (#176) so the migration root asks for
//! prepared storage through one call instead of carrying sizing arithmetic
//! and `mkfs`/`mount` invocations inline.
//!
//! The sizing and filesystem-selection *decisions* are pure functions with
//! their own tests. That split is not cosmetic here: the loopback size
//! formula has been wrong three separate times (#42), each time in a way no
//! unit test could have caught because the arithmetic was welded to the
//! commands that acted on it.

use anyhow::{Context, Result, anyhow};
use std::fs;
use std::path::Path;
use std::process::Command;

use super::MountGuard;
use crate::preflight::PreflightReport;

/// Whether this root filesystem needs the ext4 loopback store.
///
/// btrfs and ext4 support fs-verity, which `cfs pull` requires; XFS does not
/// (as of kernel 6.12), so its composefs store lives in a loopback image.
/// An unknown filesystem is treated as verity-capable — the pull will fail
/// loudly rather than this silently building a loopback nobody asked for.
pub fn needs_loopback(fs_type: Option<&str>) -> bool {
    fs_type.unwrap_or("unknown") == "xfs"
}

/// Nominal size, in GB, for the ext4 loopback composefs store.
///
/// Sizing this off the *source* ostree repo alone badly undersizes it: Phase 2
/// pulls the *target* image into this same loopback regardless of whether
/// Phase 1's reflink import (the only thing the repo size measures) runs at
/// all. With `--skip-import`, or a small source migrating to a much larger
/// target, the old fixed 10-30 GB clamp left no room for the pull and
/// ENOSPC'd mid-Phase-2 (#42).
///
/// The image is sparse — ext4 allocates blocks on demand — so a generous
/// nominal size costs nothing until used. It is bounded by what the
/// underlying filesystem actually has, not by an arbitrary ceiling.
pub fn loopback_size_gb(ostree_repo_bytes: u64, composefs_free_bytes: u64) -> u64 {
    let ostree_gb = ostree_repo_bytes as f64 / 1e9;
    let free_gb = composefs_free_bytes as f64 / 1e9;
    let desired_gb = (ostree_gb * 1.5 + 25.0).ceil() as u64;
    let max_gb = ((free_gb * 0.9) as u64).max(30);
    desired_gb.clamp(30, max_gb)
}

/// Bytes of free space the object import needs, given reflink availability.
///
/// With reflink the copies are CoW clones and barely grow the store; without
/// it every object is duplicated, hence the larger multiplier.
pub fn required_free_bytes(ostree_repo_bytes: u64, reflink_available: bool) -> u64 {
    let multiplier: f64 = if reflink_available { 1.1 } else { 1.5 };
    (ostree_repo_bytes as f64 * multiplier) as u64
}

pub fn check_free_space(reflink_available: bool) -> Result<()> {
    let ostree_repo = "/sysroot/ostree/repo";
    if !Path::new(ostree_repo).exists() {
        return Ok(());
    }

    let du = Command::new("/usr/bin/du")
        .args(["-sb", ostree_repo])
        .output()
        .context("failed to run du")?;
    let du_stdout = String::from_utf8_lossy(&du.stdout);
    let ostree_size: u64 = du_stdout
        .split_whitespace()
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let free = crate::preflight::get_free_space("/sysroot/composefs")
        .or_else(|_| crate::preflight::get_free_space("/sysroot"))?;
    let needed = required_free_bytes(ostree_size, reflink_available);

    println!(
        "Free space check: ostree repo = {:.2} GB, free = {:.2} GB, needed ≈ {:.2} GB (reflink: {})",
        ostree_size as f64 / 1e9,
        free as f64 / 1e9,
        needed as f64 / 1e9,
        reflink_available,
    );

    if free < needed {
        return Err(anyhow!(
            "Insufficient free space: need ~{:.2} GB, have {:.2} GB. Free up space or use a larger disk.",
            needed as f64 / 1e9,
            free as f64 / 1e9,
        ));
    }
    Ok(())
}

/// XFS does not support fs-verity (required by cfs pull). When the /sysroot
/// filesystem lacks verity, create a loopback ext4 image, mount it at
/// /sysroot/composefs, and migrate the composefs store onto it.
/// composefs repository metadata (`meta.json`) as written by `cfsctl init`:
/// format version 1 with sha512 fs-verity digests. Required by `bootc status`
/// and cfsctl; our hand-built XFS-loopback repo must carry it.
const COMPOSEFS_REPO_META_JSON: &str = "{\n  \"version\": 1,\n  \"algorithm\": \"fsverity-sha512-12\",\n  \"features\": {\n    \"compatible\": [],\n    \"read-only-compatible\": [],\n    \"incompatible\": []\n  }\n}\n";

pub(crate) fn prepare_composefs_storage(report: &PreflightReport) -> Result<Option<MountGuard>> {
    if needs_loopback(report.fs_type.as_deref()) {
        let target = "/sysroot/composefs";
        let img_path = "/sysroot/composefs-loopback.ext4";

        // Don't recreate if already set up (e.g. re-run after crash).
        if Path::new(img_path).exists() {
            // Check if already mounted at target.
            let mount_out = Command::new("findmnt")
                .args(["-n", "-o", "SOURCE", target])
                .output()
                .ok();
            if let Some(out) = mount_out {
                let src = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if src.contains("composefs-loopback") {
                    println!("ComposeFS loopback already active at {target} (source: {src}).");
                    return Ok(None);
                }
            }
            // Image exists but not mounted — remove stale and recreate.
            let _ = fs::remove_file(img_path);
        }

        // Sizing this off the *source* ostree repo alone badly undersizes it:
        // Phase 2 pulls the *target* image into this same loopback regardless
        // of whether Phase 1's reflink import (the only thing ostree_gb
        // actually measures) runs at all — with --skip-import, or a small
        // source migrating to a much larger target, the old 10-30 GB clamp
        // left no room for the pull and ENOSPC'd mid-Phase-2 (#42). The
        // loopback is a sparse file (ext4 allocates blocks on demand), so a
        // generous nominal size is free — bound only by what the underlying
        // filesystem actually has (composefs_free_bytes, already measured by
        // preflight), not by an arbitrary fixed ceiling.
        let size_gb = loopback_size_gb(report.ostree_repo_size_bytes, report.composefs_free_bytes);
        println!(
            "XFS detected — setting up {size_gb} GB ext4 loopback for composefs verity support.",
        );

        // Create sparse file (ext4 will allocate blocks on demand).
        let status = Command::new("truncate")
            .args(["-s", &format!("{size_gb}G"), img_path])
            .status()
            .context("failed to truncate composefs loopback image")?;
        if !status.success() {
            return Err(anyhow!("truncate failed for composefs loopback image"));
        }

        // Format as ext4 with verity support.
        let status = Command::new("/usr/sbin/mkfs.ext4")
            .args(["-F", "-O", "verity", img_path])
            .status()
            .context("failed to format composefs loopback as ext4")?;
        if !status.success() {
            return Err(anyhow!("mkfs.ext4 failed for composefs loopback"));
        }

        // Mount.
        fs::create_dir_all(target).context("failed to create /sysroot/composefs")?;
        let status = Command::new("/usr/bin/mount")
            .args(["-o", "loop", img_path, target])
            .status()
            .context("failed to mount composefs loopback")?;
        if !status.success() {
            return Err(anyhow!("mount failed for composefs loopback"));
        }

        // Initialize the composefs repository metadata. Migration populates
        // objects/images/streams by hand; without meta.json `bootc status` and
        // cfsctl reject the repo ("must be initialized with `cfsctl init`").
        // Matches what `cfsctl init` writes (format v1, sha512 fs-verity).
        fs::write(
            Path::new(target).join("meta.json"),
            COMPOSEFS_REPO_META_JSON,
        )
        .context("failed to write composefs repo meta.json")?;

        println!("ComposeFS loopback mounted at {target} ({size_gb} GB ext4, fs-verity enabled).");
        Ok(Some(MountGuard::new(Path::new(target))))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_xfs_needs_the_loopback_store() {
        assert!(needs_loopback(Some("xfs")));
        assert!(!needs_loopback(Some("btrfs")));
        assert!(!needs_loopback(Some("ext4")));
        // Unknown is treated as verity-capable on purpose: the pull fails
        // loudly rather than silently building a loopback nobody asked for.
        assert!(!needs_loopback(None));
        assert!(!needs_loopback(Some("unknown")));
    }

    /// #42 sized this wrong three times. The property that matters is that
    /// the result leaves room for the Phase-2 pull of the TARGET image, which
    /// the source repo size does not measure at all.
    #[test]
    fn loopback_sizing_table() {
        const GB: u64 = 1_000_000_000;

        // Tiny source, plenty of room: still gets the 25 GB headroom floor,
        // not something proportional to a small repo. This is the
        // --skip-import / small-source-to-large-target case that ENOSPC'd.
        assert_eq!(loopback_size_gb(GB, 500 * GB), 30);

        // Large source: scales with it (20 * 1.5 + 25 = 55).
        assert_eq!(loopback_size_gb(20 * GB, 500 * GB), 55);

        // Constrained filesystem: clamped to 90% of what is actually free,
        // never to a fixed ceiling.
        assert_eq!(loopback_size_gb(20 * GB, 50 * GB), 45);

        // Never below 30 GB even on a nearly full filesystem — a smaller
        // store cannot hold any realistic target image, so failing at mkfs
        // beats failing halfway through the pull.
        assert_eq!(loopback_size_gb(GB, 5 * GB), 30);
        assert_eq!(loopback_size_gb(0, 0), 30);
    }

    #[test]
    fn free_space_requirement_reflects_reflink_availability() {
        const GB: u64 = 1_000_000_000;
        // Reflink clones are CoW: the store barely grows.
        assert_eq!(required_free_bytes(100 * GB, true), 110 * GB);
        // Without it every object is duplicated.
        assert_eq!(required_free_bytes(100 * GB, false), 150 * GB);
        assert!(
            required_free_bytes(100 * GB, false) > required_free_bytes(100 * GB, true),
            "no-reflink must always need more room"
        );
        assert_eq!(required_free_bytes(0, false), 0);
    }
}
