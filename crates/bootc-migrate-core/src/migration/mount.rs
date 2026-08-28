//! RAII guards for mounts held during a migration.
//!
//! Both unmount on drop, so a failed phase cannot leave a stale mount behind.
//! They live here rather than in [`super`] so the migration module keeps no
//! mount implementation of its own.

use anyhow::{Context, Result, anyhow};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Unmounts `mount_path` on drop. Used for TempDir-backed mounts and for the
/// ext4 loopback that carries the composefs store on XFS roots.
pub(crate) struct MountGuard {
    mount_path: PathBuf,
}

impl MountGuard {
    pub(crate) fn new(mount_path: &Path) -> Self {
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

/// RAII guard around `podman image mount`. Mounts a locally-cached OCI image and
/// exposes its merged rootfs at `path`, unmounting on drop. Used as the Phase 5
/// fallback when the composefs overlay mount yields no usable content (bootc
/// mounts in a private namespace that does not persist to our process). Because
/// Phase 2 also `podman pull`s the image, this needs no network.
pub(crate) struct PodmanImageMount {
    image: String,
    pub(crate) path: PathBuf,
}

impl PodmanImageMount {
    pub(crate) fn new(image: &str) -> Result<Self> {
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
