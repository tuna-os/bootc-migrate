//! Dedicated `/var` layout: discovering whether `/var` is its own volume,
//! and the mount argument that keeps it attached across upgrades.
//!
//! Split out of `migration::mod` (#177) so `kernel_options` and Phase 4 both
//! consume one typed [`VarMount`] rather than reaching for private helpers
//! through `super::*`.
//!
//! Parsing is kept apart from the `findmnt`/`blkid` calls that feed it: the
//! three cases that matter — a whole filesystem, a direct Btrfs subvolume,
//! and an OSTree bind that must be *rejected* — are decided by pure functions
//! over strings, so they are table-tested without a live mount.

use anyhow::{Context, Result, anyhow};
use std::fs;
use std::process::Command;

/// stateroot path before `bootc-root-setup.service` assembles the deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VarMount {
    pub(crate) uuid: String,
    pub(crate) fstype: String,
    pub(crate) options: String,
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
pub(crate) fn detect_separate_var() -> Result<Option<VarMount>> {
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
pub(crate) fn stateroot_var_mount_kernel_arg(var: &VarMount) -> String {
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
}
