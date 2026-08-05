pub mod boot;
pub mod bootloader;
pub mod deploy;
pub mod import;
pub mod kernel_options;
pub mod os_release;
pub mod pull;
pub mod rollback;
pub mod seal;

pub use boot::phase5_setup_bootloader;
pub use deploy::phase4_stage_deploy;
pub use import::phase1_import_objects;
pub use pull::{PulledImage, phase2_pull_image};
pub use rollback::run_rollback;
pub use seal::phase3_create_image;

pub use boot::find_esp_or_mount;

pub(crate) use boot::copy_kernel_modules_from_mount;
pub(crate) use seal::{build_origin_content, patch_boot_digest_in_content};

use crate::VerityDigest;
use crate::preflight::PreflightReport;
use crate::registry::{
    extract_files_via_registry, extract_kernel_modules_via_registry, extract_subtree_via_registry,
};
use crate::xattr;
use anyhow::{Context, Result, anyhow};
use kernel_options::get_kernel_options;
use os_release::{bls_entry_filename, bls_entry_title, read_os_release};
use rustix::fs::{FlockOperation, flock};
use rustix::io::Errno;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

// ---- Lock file ----

const LOCK_PATH: &str = "/var/run/bootc-migrate.lock";

fn acquire_lock() -> Result<File> {
    let lock = File::create(LOCK_PATH).context("failed to create lock file")?;
    // Non-blocking exclusive advisory lock, released when this fd is closed
    // (i.e. on process exit). Guards against concurrent migration runs.
    match flock(&lock, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {}
        Err(Errno::WOULDBLOCK | Errno::ACCESS) => {
            return Err(anyhow!(
                "Another instance of bootc-migrate is already running (lock held at {}).",
                LOCK_PATH
            ));
        }
        Err(e) => return Err(e).context("failed to acquire lock"),
    }
    // Write PID so admins can inspect.
    let _ = writeln!(&lock, "{}", std::process::id());
    Ok(lock)
}

// ---- Mount guard (Optional: safe cleanup of TempDir-backed mounts) ----

struct MountGuard {
    mount_path: PathBuf,
}

impl MountGuard {
    fn new(mount_path: &Path) -> Self {
        MountGuard {
            mount_path: mount_path.to_path_buf(),
        }
    }
}

impl Drop for MountGuard {
    fn drop(&mut self) {
        let status = Command::new("umount").arg(&self.mount_path).status();
        match status {
            Ok(s) if s.success() => {}
            _ => eprintln!(
                "Warning: failed to unmount {} — a stale mount may remain. Use 'umount {}' manually.",
                self.mount_path.display(),
                self.mount_path.display(),
            ),
        }
    }
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

/// RAII guard around `podman image mount`. Mounts a locally-cached OCI image and
/// exposes its merged rootfs at `path`, unmounting on drop. Used as the Phase 5
/// fallback when the composefs overlay mount yields no usable content (bootc
/// mounts in a private namespace that does not persist to our process). Because
/// Phase 2 also `podman pull`s the image, this needs no network.
struct PodmanImageMount {
    image: String,
    path: PathBuf,
}

impl PodmanImageMount {
    fn new(image: &str) -> Result<Self> {
        let out = Command::new("podman")
            .args(["image", "mount", image])
            .output()
            .context("failed to execute podman image mount")?;
        if !out.status.success() {
            return Err(anyhow!(
                "podman image mount {} failed: {}",
                image,
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
        if !path.is_dir() {
            return Err(anyhow!(
                "podman image mount returned non-directory path: {}",
                path.display()
            ));
        }
        Ok(PodmanImageMount {
            image: image.to_string(),
            path,
        })
    }
}

impl Drop for PodmanImageMount {
    fn drop(&mut self) {
        let _ = Command::new("podman")
            .args(["image", "unmount", &self.image])
            .status();
    }
}

// ---- Public API ----

/// Check free space before pulling. Returns Ok(()) if sufficient, Err otherwise.
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
    let multiplier: f64 = if reflink_available { 1.1 } else { 1.5 };
    let needed = (ostree_size as f64 * multiplier) as u64;

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

fn setup_composefs_loopback_if_needed(report: &PreflightReport) -> Result<Option<MountGuard>> {
    let fs_type = report.fs_type.as_deref().unwrap_or("unknown");
    // btrfs and ext4 support fs-verity. xfs does not (as of kernel 6.12).
    if fs_type == "xfs" {
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
        let ostree_gb = report.ostree_repo_size_bytes as f64 / 1e9;
        let free_gb = report.composefs_free_bytes as f64 / 1e9;
        let desired_gb = (ostree_gb * 1.5 + 25.0).ceil() as u64;
        let max_gb = ((free_gb * 0.9) as u64).max(30);
        let size_gb = desired_gb.clamp(30, max_gb);
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

/// Detect whether LVM volumes are active on the running system.
fn detect_lvm() -> bool {
    match fs::read_dir("/dev/mapper") {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy() != "control"),
        Err(_) => false,
    }
}

/// Description of a `/var` mount that must be made available at the composefs
/// stateroot path before `bootc-root-setup.service` assembles the deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
struct VarMount {
    uuid: String,
    fstype: String,
    options: String,
}

/// Parsed, unresolved form of [`VarMount`]. Keeping findmnt parsing separate
/// from UUID resolution makes the layout rules unit-testable.
#[derive(Debug, Clone, PartialEq, Eq)]
struct VarMountCandidate {
    device: String,
    fstype: String,
    options: String,
}

/// Parse one `findmnt -no TARGET,SOURCE,FSTYPE,FSROOT,OPTIONS /var` row.
///
/// Whole filesystems mounted at `/var` are retained, as are direct Btrfs
/// subvolume mounts such as the common Anaconda layout `subvol=/var`. A bind of
/// an arbitrary directory (for example an OSTree `.../deploy/.../var` path) is
/// deliberately rejected: such a subtree cannot be reproduced by mounting the
/// backing block device at the composefs stateroot.
fn parse_var_mount(line: &str) -> Option<VarMountCandidate> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 5 || fields[0] != "/var" {
        return None;
    }

    let (source, fstype, fsroot, mount_options) = (fields[1], fields[2], fields[3], fields[4]);
    let options = if fsroot == "/" {
        "defaults".to_string()
    } else if fstype == "btrfs" {
        let subvol = mount_options
            .split(',')
            .find_map(|option| option.strip_prefix("subvol="))?;
        if subvol.trim_start_matches('/') != fsroot.trim_start_matches('/') {
            return None;
        }
        format!("defaults,subvol={subvol}")
    } else {
        return None;
    };

    // findmnt renders a mounted Btrfs subvolume as
    // `/dev/nvme0n1p3[/var]`; blkid needs the underlying device only.
    let device = source.split_once('[').map_or(source, |(device, _)| device);
    if !device.starts_with("/dev/") {
        return None;
    }

    Some(VarMountCandidate {
        device: device.to_string(),
        fstype: fstype.to_string(),
        options,
    })
}

/// Recover the administrator's canonical `/var` mount options from fstab.
///
/// `findmnt` includes runtime-derived flags and can omit policy options from
/// fstab (for example Btrfs compression level and commit interval). Only use an
/// entry whose filesystem type and direct subvolume agree with the active mount.
fn parse_var_fstab_options(fstab: &str, candidate: &VarMountCandidate) -> Option<String> {
    let expected_subvol = candidate
        .options
        .split(',')
        .find_map(|option| option.strip_prefix("subvol="));

    for line in fstab.lines() {
        let line = line.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 4
            || fields[1] != "/var"
            || (fields[2] != candidate.fstype && fields[2] != "auto")
        {
            continue;
        }

        let fstab_subvol = fields[3]
            .split(',')
            .find_map(|option| option.strip_prefix("subvol="));
        let same_subvol = match (expected_subvol, fstab_subvol) {
            (Some(expected), Some(actual)) => {
                expected.trim_start_matches('/') == actual.trim_start_matches('/')
            }
            (None, None) => true,
            _ => false,
        };
        if !same_subvol {
            continue;
        }

        // /var must be writable under composefs. noauto/nofail would also
        // undermine the explicit dependency on bootc-root-setup.
        let mut options = vec!["rw"];
        options.extend(fields[3].split(',').filter(|option| {
            !option.is_empty() && !matches!(*option, "ro" | "rw" | "noauto" | "nofail")
        }));
        return Some(options.join(","));
    }
    None
}

/// Detect a dedicated `/var` filesystem or directly-mounted Btrfs subvolume,
/// returning the exact mount needed to expose it through composefs.
///
/// bootc's composefs boot bind-mounts the per-stateroot var
/// (`/sysroot/state/os/default/var`, on the root fs) onto `/var` and *ignores*
/// any `/var` fstab entry — so on a system where `/var` lives on its own volume,
/// the composefs boot silently uses the empty stateroot var instead, losing the
/// user's home, flatpaks, etc. We detect that case so Phase 5 can mount the real
/// `/var` volume at the stateroot var path before bootc binds it (see
/// [`prepare_stateroot_var_include`]).
///
/// A direct Btrfs subvolume mount is just as load-bearing as a separate
/// partition here. Mounting `subvol=/var` at the stateroot reuses the existing
/// data in place and avoids an incomplete or very expensive recursive copy.
fn detect_separate_var() -> Result<Option<VarMount>> {
    let out = Command::new("findmnt")
        .args(["-no", "TARGET,SOURCE,FSTYPE,FSROOT,OPTIONS", "/var"])
        .output()
        .context("failed to inspect the active /var mount")?;
    if !out.status.success() {
        return Ok(None);
    }
    let line = String::from_utf8_lossy(&out.stdout);
    let Some(candidate) = parse_var_mount(&line) else {
        return Ok(None);
    };
    let uuid = blkid_uuid(&candidate.device).ok_or_else(|| {
        anyhow!(
            "recognized /var as a dedicated {} mount on {}, but could not resolve its filesystem UUID; refusing to copy or stage an ambiguous /var layout",
            candidate.fstype,
            candidate.device
        )
    })?;
    let options = fs::read_to_string("/etc/fstab")
        .ok()
        .and_then(|fstab| parse_var_fstab_options(&fstab, &candidate))
        .unwrap_or(candidate.options);
    Ok(Some(VarMount {
        uuid,
        fstype: candidate.fstype,
        options,
    }))
}

/// Build an initrd-only systemd mount specification for the live `/var`
/// filesystem at bootc's composefs stateroot path.
///
/// Keeping this in the BLS kernel arguments is load-bearing for day-2 updates:
/// bootc copies the current arguments to each upgraded deployment, whereas an
/// ad-hoc unit injected into only the first initrd would disappear as soon as
/// an updated image supplies a new initrd.
fn stateroot_var_mount_kernel_arg(var: &VarMount) -> String {
    // systemd.mount-extra uses ':' as its field separator and C-unescapes each
    // field. Escape colons in options such as `compress-force=zstd:1`.
    let options = var.options.replace('\\', "\\x5c").replace(':', "\\x3a");
    format!(
        "rd.systemd.mount-extra=/dev/disk/by-uuid/{}:/sysroot/state/os/default/var:{}:{},x-systemd.before=bootc-root-setup.service,x-systemd.required-by=bootc-root-setup.service",
        var.uuid, var.fstype, options
    )
}

/// Resolve a block device's filesystem UUID via `blkid`.
fn blkid_uuid(device: &str) -> Option<String> {
    let out = Command::new("blkid")
        .args(["-o", "value", "-s", "UUID", device])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let uuid = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if uuid.is_empty() { None } else { Some(uuid) }
}

/// Rebuild the staged initrd with LVM/DM support using the host's dracut and
/// Dakota's kernel modules from the composefs overlay mount.
///
/// Non-fatal: warns if dracut is absent or fails so migration still completes.
/// The user can rerun dracut manually from the OSTree fallback if the system
/// fails to boot (see the warning message for the exact command).
/// Build a scratch tree (for `dracut --include`) carrying the systemd units that
/// loop-mount the composefs ext4 store at /sysroot/composefs inside the initrd,
/// ordered after sysroot.mount and before bootc-root-setup.service. Returns the
/// tempdir guard; its contents are copied into the initrd by dracut.
fn prepare_composefs_loopback_include() -> Result<tempfile::TempDir> {
    let tmp = tempfile::Builder::new()
        .prefix("bootc-cfsloop-")
        .tempdir_in("/var/tmp")
        .context("failed to create scratch dir for composefs loopback unit")?;
    let unit_dir = tmp.path().join("etc/systemd/system");
    fs::create_dir_all(&unit_dir)?;
    fs::write(
        unit_dir.join("sysroot-composefs.mount"),
        "[Unit]\n\
         Description=ComposeFS Loopback Mount\n\
         After=sysroot.mount\n\
         Before=initrd-root-fs.target bootc-root-setup.service\n\
         DefaultDependencies=no\n\
         \n\
         [Mount]\n\
         What=/sysroot/composefs-loopback.ext4\n\
         Where=/sysroot/composefs\n\
         Type=ext4\n\
         Options=loop,ro\n\
         \n\
         [Install]\n\
         WantedBy=initrd-root-fs.target\n",
    )?;
    // Enable the mount unit and make bootc-root-setup require + order after it.
    let wants_dir = unit_dir.join("initrd-root-fs.target.wants");
    fs::create_dir_all(&wants_dir)?;
    std::os::unix::fs::symlink(
        "../sysroot-composefs.mount",
        wants_dir.join("sysroot-composefs.mount"),
    )
    .context("failed to enable sysroot-composefs.mount")?;
    let dropin_dir = unit_dir.join("bootc-root-setup.service.d");
    fs::create_dir_all(&dropin_dir)?;
    fs::write(
        dropin_dir.join("RequiresLoopback.conf"),
        "[Unit]\nRequires=sysroot-composefs.mount\nAfter=sysroot-composefs.mount\n",
    )?;
    Ok(tmp)
}

/// Build a scratch tree (for `dracut --include`) carrying a systemd mount unit
/// that mounts the dedicated `/var` volume at the composefs stateroot var path
/// (`/sysroot/state/os/default/var`) inside the initrd, ordered after
/// sysroot.mount and before bootc-root-setup.service.
///
/// bootc-root-setup bind-mounts that path onto the deployment's `/var`, so
/// overmounting it with the real `/var` volume here makes the user's data appear
/// at `/var` — working around bootc composefs ignoring the `/var` fstab entry on
/// systems with a dedicated `/var` partition/LV (see [`detect_separate_var`]).
/// `uuid`/`fstype` identify the volume and `options` retains a direct Btrfs
/// subvolume selection when needed. The LV is activated via the
/// `rd.lvm.lv=<vg>/<lv>` karg emitted by `get_kernel_options`.
fn prepare_stateroot_var_include(var: &VarMount) -> Result<tempfile::TempDir> {
    let tmp = tempfile::Builder::new()
        .prefix("bootc-statevar-")
        .tempdir_in("/var/tmp")
        .context("failed to create scratch dir for stateroot var unit")?;
    let unit_dir = tmp.path().join("etc/systemd/system");
    fs::create_dir_all(&unit_dir)?;
    // Mount path /sysroot/state/os/default/var → unit sysroot-state-os-default-var.mount
    let unit_name = "sysroot-state-os-default-var.mount";
    fs::write(
        unit_dir.join(unit_name),
        format!(
            "[Unit]\n\
             Description=Dedicated /var volume (composefs stateroot)\n\
             After=sysroot.mount\n\
             Before=initrd-root-fs.target bootc-root-setup.service\n\
             DefaultDependencies=no\n\
             \n\
             [Mount]\n\
             What=/dev/disk/by-uuid/{uuid}\n\
             Where=/sysroot/state/os/default/var\n\
             Type={fstype}\n\
             Options={options}\n\
             \n\
             [Install]\n\
             WantedBy=initrd-root-fs.target\n",
            uuid = var.uuid,
            fstype = var.fstype,
            options = var.options,
        ),
    )?;
    let wants_dir = unit_dir.join("initrd-root-fs.target.wants");
    fs::create_dir_all(&wants_dir)?;
    std::os::unix::fs::symlink(format!("../{unit_name}"), wants_dir.join(unit_name))
        .context("failed to enable sysroot-state-os-default-var.mount")?;
    let dropin_dir = unit_dir.join("bootc-root-setup.service.d");
    fs::create_dir_all(&dropin_dir)?;
    fs::write(
        dropin_dir.join("RequiresStaterootVar.conf"),
        format!("[Unit]\nRequires={unit_name}\nAfter={unit_name}\n"),
    )?;
    Ok(tmp)
}

fn rebuild_initrd_with_lvm_if_needed(
    kver: &str,
    mount_path: &Path,
    target_image: &str,
    initrd_dst: &Path,
) -> Result<()> {
    // LUKS roots appear as device-mapper nodes (detect_lvm), and XFS roots get
    // an ext4 loopback for the verity store. The stock Dakota initrd already
    // handles dm/crypt and composefs; for XFS it just lacks the xfs driver.
    let needs_dm = detect_lvm();
    let needs_xfs = Path::new("/sysroot/composefs-loopback.ext4").exists();
    // A dedicated /var volume needs a mount unit injected so bootc's composefs
    // boot exposes its data at /var (see prepare_stateroot_var_include).
    let separate_var = detect_separate_var()?;
    // A dedicated /var no longer requires an initrd rebuild on its own: its
    // rd.systemd.mount-extra BLS argument is interpreted by the image-provided
    // systemd-fstab-generator. If another reason does require a rebuild, retain
    // the injected unit as a redundant compatibility path.
    if !needs_dm && !needs_xfs {
        return Ok(());
    }
    let mut features: Vec<&str> = Vec::new();
    if needs_dm {
        features.push("LVM/DM/crypt");
    }
    if needs_xfs {
        features.push("XFS");
    }
    if separate_var.is_some() {
        features.push("dedicated /var");
    }
    let label = features.join(" + ");
    println!("[phase5] Rebuilding composefs initrd with {label} support...");
    if let Some(ref var) = separate_var {
        println!(
            "[phase5] dedicated /var detected ({}, UUID={}, options={}) — will mount it at the composefs stateroot var path",
            var.fstype, var.uuid, var.options
        );
    }

    // Source the target's kernel modules from the sealed composefs overlay mount
    // (real bytes, no network), falling back to registry streaming if they're
    // absent. `_modules_tmp` holds the writable copy alive until the rebuild ends
    // (depmod must write modules.dep.bin, so the read-only mount can't be used
    // directly).
    let (_modules_tmp, modules_src) = match copy_kernel_modules_from_mount(mount_path, kver) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("[phase5] kernel modules not available from mount ({e:#}); using registry");
            extract_kernel_modules_via_registry(target_image, kver)
                .context("failed to obtain target kernel modules for initrd rebuild")?
        }
    };

    // The target image ships no dracut binary — only its dracut *modules* — so we
    // run the *source* system's dracut, which carries the same 50ostree/51bootc
    // dracut modules. `--rebuild` then re-runs the target initrd's stored build
    // configuration (preserving the composefs root assembly, crypt, and dm
    // modules) and only ADDS the missing xfs driver (plus dm/crypt/lvm as
    // belt-and-suspenders for the LUKS root).
    //
    // The catch: dracut resolves the kernel module index from the standard
    // /lib/modules/<kver> path and ignores --kmoddir for it. On the source —
    // whose running kernel differs from the target's <kver> — that path is empty,
    // so every driver (erofs, overlay, dm, crypt, xfs) silently drops out and the
    // initrd is unbootable. We fix that by making /lib/modules/<kver> resolve to
    // the target's modules: a staging dir whose <kver> entry symlinks to the
    // mounted target modules is bind-mounted over /usr/lib/modules (= /lib/
    // modules) for the rebuild, then unmounted.
    let dracut_path = ["/usr/bin/dracut", "/usr/sbin/dracut", "dracut"]
        .iter()
        .find(|&&p| Path::new(p).exists())
        .copied()
        .ok_or_else(|| anyhow!("dracut not found on source; cannot rebuild initrd for {label}"))?;

    let modules_root = PathBuf::from("/usr/lib/modules");
    let staging = PathBuf::from("/var/tmp").join(format!("bootc-kmod-root-{}", std::process::id()));
    // staging/<kver> -> <mount>/usr/lib/modules/<kver>. The link target is an
    // absolute path *outside* /usr/lib/modules, so it stays valid after we bind
    // staging over /usr/lib/modules (no self-referential loop).
    let staged_kver = staging.join(kver);

    // For XFS roots the composefs verity store lives in an ext4 loopback file on
    // the XFS root. The initrd must loop-mount it at /sysroot/composefs after the
    // root mounts but before bootc assembles composefs, otherwise bootc-root-setup
    // fails with "Opening ref 'images/<hash>': No such file or directory". Inject
    // a systemd mount unit (+ ordering drop-in) via dracut --include; the ext4 and
    // loop drivers added below let the initrd actually mount it.
    let loop_include = if needs_xfs {
        Some(prepare_composefs_loopback_include()?)
    } else {
        None
    };
    let var_include = match separate_var {
        Some(ref var) => Some(prepare_stateroot_var_include(var)?),
        None => None,
    };

    let mut bound = false;
    let run_rebuild = |bound: &mut bool| -> Result<std::process::ExitStatus> {
        if staging.exists() {
            let _ = fs::remove_dir_all(&staging);
        }
        fs::create_dir_all(&staging)
            .with_context(|| format!("create kmod staging dir {}", staging.display()))?;
        std::os::unix::fs::symlink(&modules_src, &staged_kver).with_context(|| {
            format!(
                "symlink {} -> {}",
                staged_kver.display(),
                modules_src.display()
            )
        })?;

        let st = Command::new("mount")
            .arg("--bind")
            .arg(&staging)
            .arg(&modules_root)
            .status()
            .with_context(|| {
                format!("bind {} over {}", staging.display(), modules_root.display())
            })?;
        if !st.success() {
            return Err(anyhow!(
                "failed to bind kmod staging over {}",
                modules_root.display()
            ));
        }
        *bound = true;

        // /lib/modules/<kver> now resolves to the target modules (valid
        // modules.dep.bin); `--rebuild` preserves composefs and adds xfs.
        //
        // CRITICAL: the bootc dracut module has check() { return 255; } which
        // means `dracut --rebuild` will NOT include it unless we explicitly ask.
        // Without bootc-root-setup.service in the initrd, the composefs EROFS
        // image is never assembled and systemd tries to switch-root to the raw
        // /sysroot partition — which fails with "os-release file is missing".
        let mut cmd = Command::new(dracut_path);
        cmd.arg("--rebuild")
            .arg(initrd_dst)
            .arg("--kver")
            .arg(kver)
            .arg("--force")
            .arg("--add")
            .arg("bootc");
        if needs_dm {
            cmd.arg("--add").arg("lvm dm crypt");
        }
        if needs_xfs {
            cmd.arg("--add-drivers").arg("xfs ext4 loop");
            if let Some(ref inc) = loop_include {
                cmd.arg("--include").arg(inc.path()).arg("/");
            }
        }
        if let Some(ref inc) = var_include {
            // Ensure xfs/ext4 are present even when there's no composefs loopback
            // (the dedicated /var may be the only reason we rebuild).
            cmd.arg("--add-drivers").arg("xfs ext4");
            cmd.arg("--include").arg(inc.path()).arg("/");
        }
        cmd.status().context("failed to run dracut --rebuild")
    };

    let result = run_rebuild(&mut bound);

    // Restore the source's /usr/lib/modules and drop the staging dir, regardless
    // of the dracut outcome.
    if bound
        && let Ok(s) = Command::new("umount")
            .arg("--lazy")
            .arg(&modules_root)
            .status()
        && !s.success()
    {
        eprintln!(
            "[phase5] Warning: failed to unmount kmod staging from {}",
            modules_root.display()
        );
    }
    let _ = fs::remove_dir_all(&staging);

    match result {
        Ok(s) if s.success() => {
            println!(
                "[phase5] {label} initrd rebuilt and staged at {}.",
                initrd_dst.display()
            );
            Ok(())
        }
        Ok(s) => {
            eprintln!(
                "[phase5] Warning: dracut exited {:?} — composefs initrd left unchanged; it \
                 lacks {label} support and the composefs entry may not boot. Boot the OSTree \
                 fallback and rerun the migration to recover.",
                s.code()
            );
            Ok(())
        }
        Err(e) => {
            eprintln!(
                "[phase5] Warning: initrd rebuild failed ({e:#}) — composefs initrd left \
                 unchanged; boot the OSTree fallback to recover."
            );
            Ok(())
        }
    }
}

/// Main migration entry point. Orchestrates all 5 phases.
pub fn run_migration(
    report: &PreflightReport,
    target_image: &str,
    dry_run: bool,
    skip_import: bool,
    bootloader: &str,
    force: bool,
) -> Result<()> {
    // Acquire exclusive lock.
    let _lock = if !dry_run {
        Some(acquire_lock()?)
    } else {
        None
    };

    // Acquire systemd sleep inhibitor lock (issue #27).
    let _sleep_guard = if !dry_run {
        Some(SleepGuard::new("OSTree to ComposeFS migration in progress"))
    } else {
        None
    };

    if dry_run {
        println!("[DRY RUN] Would execute migration phases without making changes.");
    }

    // Mount /sysroot and /boot read-write.
    if !dry_run {
        let sysroot_status = Command::new("/usr/bin/mount")
            .args(["-o", "remount,rw", "/sysroot"])
            .status()
            .context("failed to execute mount remount,rw /sysroot")?;
        if !sysroot_status.success() {
            return Err(anyhow!(
                "failed to remount /sysroot read-write — cannot proceed with migration"
            ));
        }
        let boot_status = Command::new("/usr/bin/mount")
            .args(["-o", "remount,rw", "/boot"])
            .status()
            .context("failed to execute mount remount,rw /boot")?;
        if !boot_status.success() {
            return Err(anyhow!(
                "failed to remount /boot read-write — cannot proceed with migration"
            ));
        }
    } else {
        println!("[DRY RUN] Would remount /sysroot and /boot read-write.");
    }

    // ---- Phase 0: preflight free-space check ----
    println!("=== Phase 0: Free-space check ===");
    if !dry_run {
        check_free_space(report.supports_reflink)?;
    } else {
        println!("[DRY RUN] Would check free space on /sysroot/composefs.");
    }

    // ---- XFS workaround: ensure composefs store supports fs-verity ----
    let _loopback_guard: Option<MountGuard> = if !dry_run {
        setup_composefs_loopback_if_needed(report)?
    } else {
        let fs_type = report.fs_type.as_deref().unwrap_or("unknown");
        if fs_type == "xfs" {
            println!("[DRY RUN] Would set up ext4 loopback at /sysroot/composefs for fs-verity.");
        }
        None
    };

    // ---- Phase 1: Import OSTree objects (optional / deletable) ----
    // Ensure composefs repository directory exists before any phase touches it.
    if !dry_run {
        fs::create_dir_all("/sysroot/composefs")
            .context("failed to create composefs repository directory")?;
    }

    if !skip_import {
        phase1_import_objects(report, dry_run)?;
    } else {
        println!("=== Phase 1: Skipped (--skip-import) ===");
    }

    // ---- Phase 2: Pull OCI image ----
    let store = crate::composefs::BootcCliStore::default();
    let pulled_image = phase2_pull_image(&store, target_image, dry_run)?;

    // ---- Phase 3: Create and seal EROFS image ----
    let (verity, sealed_config) =
        phase3_create_image(&store, target_image, &pulled_image.config_digest, dry_run)?;

    // ---- Phase 4: Stage deployment state ----
    let _deploy_dir = phase4_stage_deploy(
        &verity,
        target_image,
        &pulled_image,
        &sealed_config,
        dry_run,
        force,
    )?;

    // ---- Phase 5: Setup bootloader ----
    phase5_setup_bootloader(
        report,
        &verity,
        &pulled_image.image_reference,
        &sealed_config,
        dry_run,
        bootloader,
        force,
    )?;

    println!("\n=== MIGRATION COMPLETED ===");
    println!("Staged ComposeFS deployment: {}", verity.as_hex());
    let use_systemd_boot = bootloader != "grub2" && report.is_uefi && report.nvram_writable;
    if use_systemd_boot {
        println!("Primary bootloader: systemd-boot");
    } else {
        println!("Primary bootloader: GRUB2 (BLS Type 1)");
    }
    println!("Please reboot the system to finalize the transition.");
    println!("After successful boot, run 'bootc-migrate commit' to make composefs permanent.");
    if !dry_run {
        // Best-effort: a login reminder is a courtesy, not a migration
        // requirement — don't fail an otherwise-successful migration over it.
        if let Err(e) = crate::motd::write_migration_reminder(verity.as_hex()) {
            eprintln!("Warning: failed to write login reminder: {e:#}");
        }
    }
    Ok(())
}

pub fn inspect_image(image_id: &str) -> Result<String> {
    let output = Command::new("bootc")
        .args(["internals", "cfs", "--system", "oci", "inspect", image_id])
        .output()
        .context("failed to execute bootc internals cfs oci inspect")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("inspect failed: {}", stderr));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Generation-aware composefs overlay mount for phases 4/5.
///
/// On a **legacy-CLI host** this is exactly [`mount_image`] with the sealed
/// config digest — byte-identical to historical behavior.
///
/// On a **new-generation host** (no `create-image`/`seal` — issue #72) the
/// sealed-config identifier resolves nothing (`oci mount` now takes a tag or
/// manifest digest), and a legacy-delegate-written store additionally lacks
/// the config-splitstream EROFS named ref that new-gen resolution requires.
/// Both are fixed by one free operation, verified empirically (see
/// docs/cfs-cli-generations.md): re-pull the image from `containers-storage:`
/// — 0 new objects, deduped, rewrites config+manifest splitstreams with the
/// EROFS ref, and the EROFS id is deterministic so existing BLS/`.origin`
/// digests stay valid — then mount by the pulled ref. Any failure falls back
/// to the legacy path (and its raw-EROFS + caller-side podman fallbacks).
pub fn mount_image_for(target_image: &str, sealed_config: &str, mount_path: &Path) -> Result<()> {
    if !crate::composefs::host_cfs_is_legacy() {
        let cs_ref = format!("containers-storage:{target_image}");
        let pulled = Command::new("bootc")
            .args(["internals", "cfs", "--system", "oci", "pull", &cs_ref])
            .output();
        match pulled {
            Ok(o) if o.status.success() => {
                if let Some(mount_str) = mount_path.to_str() {
                    let mnt = Command::new("bootc")
                        .args([
                            "internals",
                            "cfs",
                            "--system",
                            "oci",
                            "mount",
                            &cs_ref,
                            mount_str,
                        ])
                        .output();
                    match mnt {
                        Ok(m) if m.status.success() => return Ok(()),
                        Ok(m) => eprintln!(
                            "[mount] new-gen mount by ref failed ({}); trying legacy identifiers",
                            String::from_utf8_lossy(&m.stderr).trim()
                        ),
                        Err(e) => eprintln!(
                            "[mount] new-gen mount by ref failed ({e}); trying legacy identifiers"
                        ),
                    }
                }
            }
            Ok(o) => eprintln!(
                "[mount] new-gen containers-storage re-pull failed ({}); \
                 trying legacy identifiers",
                String::from_utf8_lossy(&o.stderr).trim()
            ),
            Err(e) => eprintln!(
                "[mount] new-gen containers-storage re-pull failed ({e}); \
                 trying legacy identifiers"
            ),
        }
    }
    mount_image(sealed_config, mount_path)
}

pub fn mount_image(image_id: &str, mount_path: &Path) -> Result<()> {
    let mount_str = mount_path
        .to_str()
        .ok_or_else(|| anyhow!("invalid mount path"))?;

    // Always prefer the bootc composefs overlay mount: it stacks the EROFS
    // metadata layer on top of the content-addressed object tree at
    // /sysroot/composefs/objects so files read back with their actual content.
    // A bare `mount -t erofs` returns metadata-only views (sizes look right but
    // file contents are zero-filled), which silently corrupts every artifact
    // Phase 5 copies out of the mount (kernel, initrd, systemd-bootx64.efi…).
    let output = Command::new("bootc")
        .args([
            "internals",
            "cfs",
            "--system",
            "oci",
            "mount",
            image_id,
            mount_str,
        ])
        .output()
        .context("failed to execute bootc internals cfs oci mount")?;
    if output.status.success() {
        return Ok(());
    }

    // Last-resort fallback: raw EROFS mount. This works only if every file
    // copied out of the mount happens to be inline (small enough to live in
    // the EROFS metadata). Reserved for environments where bootc is missing.
    let bootc_err = String::from_utf8_lossy(&output.stderr).into_owned();
    let image_path = Path::new("/sysroot/composefs/images").join(image_id);
    if image_path.exists() {
        let fallback = Command::new("/usr/bin/mount")
            .args([
                "-t",
                "erofs",
                "-o",
                "ro,loop",
                image_path.to_str().unwrap_or(""),
                mount_str,
            ])
            .output()
            .context("failed to mount erofs image (bootc cfs fallback)")?;
        if fallback.status.success() {
            eprintln!(
                "Warning: bootc cfs mount failed ({}), fell back to raw EROFS — \
                 file content beyond the inline threshold will read as zeros.",
                bootc_err.trim()
            );
            return Ok(());
        }
    }
    Err(anyhow!("mount failed: {}", bootc_err))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_whole_filesystem_var_mount() {
        let parsed =
            parse_var_mount("/var /dev/mapper/sys-var xfs / rw,relatime,attr2,inode64,noquota")
                .unwrap();
        assert_eq!(
            parsed,
            VarMountCandidate {
                device: "/dev/mapper/sys-var".into(),
                fstype: "xfs".into(),
                options: "defaults".into(),
            }
        );
    }

    #[test]
    fn parses_direct_btrfs_var_subvolume() {
        let parsed = parse_var_mount(
            "/var /dev/nvme0n1p3[/var] btrfs /var rw,noatime,subvolid=258,subvol=/var",
        )
        .unwrap();
        assert_eq!(
            parsed,
            VarMountCandidate {
                device: "/dev/nvme0n1p3".into(),
                fstype: "btrfs".into(),
                options: "defaults,subvol=/var".into(),
            }
        );
    }

    #[test]
    fn preserves_matching_var_fstab_policy_options() {
        let candidate = VarMountCandidate {
            device: "/dev/nvme0n1p3".into(),
            fstype: "btrfs".into(),
            options: "defaults,subvol=/var".into(),
        };
        let fstab = "\
UUID=root / btrfs subvol=root,ro 0 0\n\
UUID=root /var btrfs subvol=var,noatime,lazytime,commit=120,discard=async,compress-force=zstd:1,space_cache=v2 0 0\n";
        assert_eq!(
            parse_var_fstab_options(fstab, &candidate).as_deref(),
            Some(
                "rw,subvol=var,noatime,lazytime,commit=120,discard=async,compress-force=zstd:1,space_cache=v2"
            )
        );
    }

    #[test]
    fn rejects_mismatched_var_fstab_subvolume() {
        let candidate = VarMountCandidate {
            device: "/dev/nvme0n1p3".into(),
            fstype: "btrfs".into(),
            options: "defaults,subvol=/var".into(),
        };
        assert!(
            parse_var_fstab_options(
                "UUID=root /var btrfs subvol=other,compress=zstd:1 0 0\n",
                &candidate,
            )
            .is_none()
        );
    }

    #[test]
    fn rejects_ostree_var_bind_inside_another_btrfs_subvolume() {
        assert!(
            parse_var_mount(
                "/var /dev/nvme0n1p3[/root/ostree/deploy/default/var] btrfs /root/ostree/deploy/default/var rw,subvolid=256,subvol=/root",
            )
            .is_none()
        );
    }

    #[test]
    fn rejects_var_that_is_not_its_own_mount() {
        assert!(
            parse_var_mount("/ /dev/nvme0n1p3[/root] btrfs /root rw,subvolid=256,subvol=/root",)
                .is_none()
        );
    }

    #[test]
    fn builds_upgrade_persistent_stateroot_var_mount_arg() {
        let arg = stateroot_var_mount_kernel_arg(&VarMount {
            uuid: "09db668c-49ab-4e29-a634-6ff75ed8d107".into(),
            fstype: "btrfs".into(),
            options: "defaults,subvol=/var".into(),
        });
        assert_eq!(
            arg,
            "rd.systemd.mount-extra=/dev/disk/by-uuid/09db668c-49ab-4e29-a634-6ff75ed8d107:/sysroot/state/os/default/var:btrfs:defaults,subvol=/var,x-systemd.before=bootc-root-setup.service,x-systemd.required-by=bootc-root-setup.service"
        );
    }

    #[test]
    fn escapes_colons_in_stateroot_var_mount_options() {
        let arg = stateroot_var_mount_kernel_arg(&VarMount {
            uuid: "btrfs-uuid".into(),
            fstype: "btrfs".into(),
            options: "rw,subvol=var,compress-force=zstd:1".into(),
        });
        assert!(arg.contains("compress-force=zstd\\x3a1"));
        assert!(!arg.contains("compress-force=zstd:1"));
    }

    #[test]
    fn test_sleep_guard_creation_and_drop() {
        let guard = SleepGuard::new("unit test migration");
        drop(guard);
    }
}
