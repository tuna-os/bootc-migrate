//! Discovery of bootable deployments for standalone bootloader migration.
//!
//! This module describes what can be migrated. Bootloader-specific planning
//! and mutation remain in [`super::boot`].

use super::*;

/// A deployment found on the running system — either OSTree or composefs.
#[derive(Debug)]
pub(super) struct BootDeployment {
    pub(super) root: PathBuf,
    pub(super) checksum: String,
    pub(super) kver: String,
    pub(super) vmlinuz: PathBuf,
    pub(super) initrd: PathBuf,
    pub(super) is_composefs: bool,
}

/// Enumerate all bootable deployments (OSTree + composefs) on the running system.
pub(super) fn enumerate_deployments() -> Result<Vec<BootDeployment>> {
    enumerate_deployments_from(
        Path::new("/sysroot/ostree/deploy/default/deploy"),
        Path::new("/sysroot/state/os/default"),
    )
}

fn enumerate_deployments_from(
    ostree_base: &Path,
    composefs_base: &Path,
) -> Result<Vec<BootDeployment>> {
    let mut deployments = Vec::new();

    if ostree_base.exists() {
        for entry in fs::read_dir(ostree_base)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".0") || !entry.path().is_dir() {
                continue;
            }
            let checksum = name.trim_end_matches(".0").to_string();
            let modules_dir = entry.path().join("usr/lib/modules");
            let Some(kver) = find_kver_in_modules(&modules_dir) else {
                continue;
            };
            let vmlinuz = modules_dir.join(&kver).join("vmlinuz");
            if vmlinuz.exists() {
                deployments.push(BootDeployment {
                    root: entry.path(),
                    checksum,
                    initrd: modules_dir.join(&kver).join("initramfs.img"),
                    kver,
                    vmlinuz,
                    is_composefs: false,
                });
            }
        }
    }

    if composefs_base.exists() {
        for entry in fs::read_dir(composefs_base)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let checksum = entry.file_name().to_string_lossy().into_owned();
            if checksum == "state"
                || checksum.len() < 12
                || !checksum.chars().all(|c| c.is_ascii_hexdigit())
                || !path.join(format!("{checksum}.origin")).exists()
            {
                continue;
            }
            let modules_dir = path.join("usr/lib/modules");
            let Some(kver) = find_kver_in_modules(&modules_dir) else {
                continue;
            };
            deployments.push(BootDeployment {
                root: path,
                checksum,
                initrd: modules_dir.join(&kver).join("initramfs.img"),
                vmlinuz: modules_dir.join(&kver).join("vmlinuz"),
                kver,
                is_composefs: true,
            });
        }
    }

    Ok(deployments)
}

fn find_kver_in_modules(modules_dir: &Path) -> Option<String> {
    fs::read_dir(modules_dir)
        .ok()?
        .filter_map(|entry| entry.ok())
        .find(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn discovers_only_bootable_ostree_and_composefs_deployments() {
        let temp = tempdir().unwrap();
        let ostree = temp.path().join("ostree");
        let composefs = temp.path().join("composefs");

        let ostree_modules = ostree.join("ostree-checksum.0/usr/lib/modules/6.15.0");
        fs::create_dir_all(&ostree_modules).unwrap();
        fs::write(ostree_modules.join("vmlinuz"), "kernel").unwrap();
        fs::create_dir_all(ostree.join("ignored-no-suffix/usr/lib/modules/6.15.0")).unwrap();

        let composefs_deployment = composefs.join("0123456789abcdef");
        let composefs_modules = composefs_deployment.join("usr/lib/modules/6.16.0");
        fs::create_dir_all(&composefs_modules).unwrap();
        fs::write(
            composefs_deployment.join("0123456789abcdef.origin"),
            "[origin]",
        )
        .unwrap();
        fs::create_dir_all(composefs.join("not-a-digest")).unwrap();

        let deployments = enumerate_deployments_from(&ostree, &composefs).unwrap();

        assert_eq!(deployments.len(), 2);
        assert_eq!(deployments[0].checksum, "ostree-checksum");
        assert_eq!(deployments[0].kver, "6.15.0");
        assert!(!deployments[0].is_composefs);
        assert_eq!(deployments[1].checksum, "0123456789abcdef");
        assert_eq!(deployments[1].kver, "6.16.0");
        assert!(deployments[1].is_composefs);
    }
}
