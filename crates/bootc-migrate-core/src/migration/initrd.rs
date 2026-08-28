//! Initrd reconstruction for Phase 5: the dracut include trees that carry
//! the ComposeFS loopback and dedicated-`/var` mount units into the initramfs,
//! and the rebuild itself.
//!
//! Split out of `migration::mod` (#178) so `boot.rs` makes one reconstruction
//! request instead of assembling dracut invocations inline.
//!
//! Unit *rendering* is separated from the rebuild that consumes it, so the
//! text of each unit — and in particular its mount options and its ordering
//! around `bootc-root-setup.service` — is testable without invoking dracut.
//! That matters more here than the line count: a wrong mount option in one of
//! these units produced a system that migrated, booted, and could then never
//! take an update (see `prepare_composefs_loopback_include`).

use anyhow::{Context, Result, anyhow};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::registry::extract_kernel_modules_via_registry;

use super::boot::copy_kernel_modules_from_mount;
use super::var_layout;

/// Detect whether LVM volumes are active on the running system.
fn detect_lvm() -> bool {
    match fs::read_dir("/dev/mapper") {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy() != "control"),
        Err(_) => false,
    }
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
///
/// Mounted **rw** even though the initrd itself only reads the EROFS image:
/// this mount is established under /sysroot, and /sysroot *becomes* / at
/// switch-root, so it survives into the running system. The runtime unit that
/// [`deploy::write_runtime_composefs_loopback_mount`] installs cannot correct
/// it — systemd treats an already-mounted target as active and never issues a
/// second mount — so whatever options are chosen here are the options the
/// booted system keeps. With `ro`, day-2 updates fail against the store:
///
/// ```text
/// error: Upgrading composefs: Performing Upgrade Operation: Pulling composefs
/// repository: Pulling image into composefs repository: Repository is not
/// writable: read-only file system
/// ```
///
/// i.e. the migration produces a system that can never update. `rw` is also
/// the more correct choice for an ext4 image on its own terms: recovering an
/// unclean journal requires a writable mount.
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
         Options=loop,rw\n\
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
fn prepare_stateroot_var_include(var: &var_layout::VarMount) -> Result<tempfile::TempDir> {
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

pub(crate) fn rebuild_initrd_with_lvm_if_needed(
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
    let separate_var = var_layout::detect_separate_var()?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// This is the binding copy of the mount options. The unit is established
    /// under /sysroot, which becomes / at switch-root, so the mount survives
    /// into the running system and the runtime unit
    /// (`deploy::write_runtime_composefs_loopback_mount`) never gets to issue
    /// a second mount — systemd sees the target already mounted and marks it
    /// active. `ro` here therefore means a migrated XFS system can never take
    /// a day-2 update, which both loopback E2E cells reproduced as
    /// "Repository is not writable: read-only file system".
    #[test]
    fn initrd_composefs_loopback_mount_is_writable() {
        let tmp = prepare_composefs_loopback_include().unwrap();

        let unit = tmp
            .path()
            .join("etc/systemd/system/sysroot-composefs.mount");
        let body = std::fs::read_to_string(&unit).unwrap();
        let opts = body
            .lines()
            .find_map(|l| l.trim().strip_prefix("Options="))
            .expect("mount unit has no Options= line");
        assert!(
            opts.split(',').any(|o| o == "rw"),
            "initrd composefs loopback mount must be rw — it is the mount the \
             booted system keeps; got Options={opts}"
        );
        assert!(body.contains("Where=/sysroot/composefs"));

        let link = tmp
            .path()
            .join("etc/systemd/system/initrd-root-fs.target.wants/sysroot-composefs.mount");
        assert!(link.is_symlink(), "mount unit was written but not enabled");

        // bootc-root-setup must not run before the store is mounted.
        let dropin = tmp
            .path()
            .join("etc/systemd/system/bootc-root-setup.service.d/RequiresLoopback.conf");
        let dropin_body = std::fs::read_to_string(&dropin).unwrap();
        assert!(dropin_body.contains("Requires=sysroot-composefs.mount"));
        assert!(dropin_body.contains("After=sysroot-composefs.mount"));
    }
}
