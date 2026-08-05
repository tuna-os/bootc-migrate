//! Two-phase apply: finalize (`commit`) or roll back (`undo`) a staged
//! migration.
//!
//! The migration pipeline stages a composefs deployment next to the existing
//! OSTree one and leaves both bootable. This module supplies the two terminal
//! operations of that transaction:
//!
//! - [`commit`] — after a successful composefs boot, make it permanent:
//!   remove the OSTree fallback boot entry, delete the OSTree object store
//!   and deploys, and leave the on-disk layout matching a fresh install of
//!   the target image. One-way.
//! - [`undo`] — remove composefs boot artifacts and staged deployments while
//!   preserving the OSTree deployment (and, unless `full`, the composefs
//!   object store, which is expensive to rebuild across retries).
//!
//! Both support `dry_run` previews. Callers are responsible for privilege
//! checks; these functions assume root.

use crate::migration;
use crate::motd;
use anyhow::{Context, Result};
use std::path::{Component, Path, PathBuf};

pub fn commit(dry_run: bool) -> Result<()> {
    println!("=== Committing composefs deployment as permanent default ===");
    if dry_run {
        println!("*** DRY RUN — no changes will be made ***");
    }

    // Sanity check: refuse to run if booted via the OSTree side. Committing
    // would delete the rootfs we're currently mounted on top of.
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    if !cmdline.contains("composefs=") {
        anyhow::bail!(
            "/proc/cmdline does not contain composefs= — current boot looks like OSTree. \
             Reboot into the composefs entry before running commit.\n\
             cmdline: {}",
            cmdline.trim()
        );
    }

    // Detect bootloader — check ESP entries for systemd-boot first.
    let esp_candidates = ["/boot/efi", "/efi"];
    let mut entries_dir = PathBuf::from("/boot/loader/entries");
    let mut is_systemd_boot = false;

    for esp in &esp_candidates {
        let esp_entries = Path::new(esp).join("loader/entries");
        if esp_entries.exists() {
            // Check if there are bootc_ entries on the ESP.
            if let Ok(mut rd) = std::fs::read_dir(&esp_entries)
                && rd.any(|e| {
                    e.map(|en| en.file_name().to_string_lossy().starts_with("bootc_"))
                        .unwrap_or(false)
                })
            {
                entries_dir = esp_entries;
                is_systemd_boot = true;
                break;
            }
        }
    }

    let mut composefs_entries: Vec<_> = Vec::new();
    if entries_dir.exists() {
        for entry in std::fs::read_dir(&entries_dir)? {
            let entry = entry?;
            let name_str = entry.file_name().to_string_lossy().into_owned();
            if name_str.starts_with("bootc_") {
                composefs_entries.push(name_str);
            }
        }
    }
    // If we found bootc_ entries at the default /boot location but not
    // on the ESP, we're still on composefs — just without an auto-mounted
    // ESP at /boot/efi or /efi (common in E2E QEMU runs).
    if !composefs_entries.is_empty() {
        is_systemd_boot = true;
    }

    // Fallback: the ESP may be unmounted or at a non-standard path
    // (e.g. after LUKS migration where Phase 5 auto-mounted it at
    // /var/tmp/esp-migration and the boot-time fstab doesn't know about
    // it). Try auto-mounting the ESP by partition type GUID.
    if composefs_entries.is_empty()
        && let Ok(esp_path) = migration::find_esp_or_mount()
    {
        let esp_entries = Path::new(&esp_path).join("loader/entries");
        if esp_entries.exists() {
            for entry in std::fs::read_dir(&esp_entries)? {
                let entry = entry?;
                let name_str = entry.file_name().to_string_lossy().into_owned();
                if name_str.starts_with("bootc_") {
                    composefs_entries.push(name_str);
                }
            }
            if !composefs_entries.is_empty() {
                entries_dir = esp_entries;
                is_systemd_boot = true;
                println!(
                    "Found composefs BLS entries on auto-mounted ESP at {}",
                    esp_path
                );
            }
        }
    }

    if composefs_entries.is_empty() {
        if is_systemd_boot {
            println!("No composefs BLS entries found on ESP. Nothing to commit.");
            println!(
                "Note: for systemd-boot, the composefs entry should already be the default if it has the lowest sort-key."
            );
        } else {
            println!("No composefs BLS entries found. Nothing to commit.");
        }
        return Ok(());
    }

    // Sort by priority (higher first) and pick the highest
    composefs_entries.sort();
    composefs_entries.reverse();
    let primary = composefs_entries[0].trim_end_matches(".conf");

    // /sysroot is typically read-only on a composefs-booted system. Prepare
    // a writable view of the same filesystem before retiring the rollback
    // entry, so a failed cleanup cannot be reported as a completed commit.
    let needs_sysroot_cleanup = has_legacy_ostree_content(Path::new("/sysroot/ostree"))?
        || Path::new("/sysroot/.bootc-aleph.json").exists();
    let sysroot_cleanup_mount = if dry_run || !needs_sysroot_cleanup {
        None
    } else {
        Some(mount_sysroot_for_cleanup()?)
    };

    if is_systemd_boot {
        // Remove the OSTree fallback entry + its kernel/initrd from the ESP so the next
        // boot menu only shows the composefs entry. The composefs entry remains the
        // loader.conf default; nothing else needs to change.
        let esp_root = entries_dir.parent().and_then(|p| p.parent());
        if let Some(esp_root) = esp_root {
            finalize_systemd_boot_commit(esp_root, &entries_dir, primary, dry_run)?;
        }
        println!(
            "Composefs deployment '{}' committed as the permanent systemd-boot default.",
            primary
        );
    } else {
        if !dry_run {
            let status = std::process::Command::new("grub2-set-default")
                .arg(primary)
                .status();
            if !matches!(status, Ok(s) if s.success()) {
                anyhow::bail!("failed to set GRUB default");
            }
            // Drop GRUB2-side OSTree fallback artifacts too.
            let _ = std::fs::remove_file("/boot/loader/entries/ostree-fallback-0.conf");
            let _ = std::fs::remove_dir_all("/boot/ostree-fallback");
        }
        println!(
            "Composefs deployment '{}' is now the permanent default.",
            primary
        );
    }

    // --- Full OSTree-side cleanup so the on-disk layout matches a fresh
    //     bootc install of the target image. ---
    let mut total_freed: u64 = 0;
    let sysroot_cleanup_root = sysroot_cleanup_mount
        .as_ref()
        .map_or_else(|| Path::new("/sysroot"), SysrootCleanupMount::root);

    // 1. /sysroot/ostree — remove the legacy OSTree object store + deploys,
    //    including the leaked Bluefin /var copy under
    //    ostree/deploy/<n>/var. The target bootc installation owns
    //    ostree/bootc/storage, so preserve that runtime state.
    total_freed += remove_legacy_ostree_content(
        &sysroot_cleanup_root.join("ostree"),
        "legacy OSTree object store + deploys (incl. leaked pre-migration /var)",
        dry_run,
    )?;
    total_freed += remove_path_with_size(
        &sysroot_cleanup_root.join(".bootc-aleph.json"),
        "stale Bluefin install-provenance marker",
        dry_run,
    )?;

    // /boot may be read-only under composefs (e.g. separate ext4 /boot partition
    // on LUKS where the initramfs mounts it ro). Remount rw before cleanup.
    if !dry_run {
        let _ = std::process::Command::new("mount")
            .args(["-o", "remount,rw", "/boot"])
            .status();
    }

    // 2. Stale OSTree BLS entries under /boot/loader/entries. The ESP-side
    //    ostree-fallback was removed above; /boot/loader/entries/ostree-*.conf
    //    is the GRUB-side equivalent.
    if let Ok(rd) = std::fs::read_dir("/boot/loader/entries") {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("ostree-") && name.ends_with(".conf") {
                total_freed += remove_path_with_size(
                    &entry.path(),
                    &format!("stale OSTree BLS entry: {}", name),
                    dry_run,
                )?;
            }
        }
    }

    // 3. The legacy OSTree boot layout makes /boot/loader a symlink to a
    // generation directory (for example loader.1). systemd's bootctl
    // intentionally refuses to operate through that symlink, which leaves
    // systemd-boot-update.service failed on a successfully migrated target.
    // It is the rollback path until commit, so clean it only now and only
    // when it contains no non-OSTree content.
    if is_systemd_boot {
        total_freed += remove_path_with_size(
            Path::new("/boot/ostree"),
            "legacy OSTree kernel and initramfs artifacts",
            dry_run,
        )?;
        match cleanup_legacy_ostree_loader(Path::new("/boot"), dry_run)? {
            LegacyOstreeLoaderCleanup::Removed => {
                println!(
                    "Removed legacy OSTree /boot/loader generation so systemd-boot can maintain $BOOT."
                );
            }
            LegacyOstreeLoaderCleanup::RetainedNonOstreeContent => {
                eprintln!(
                    "warning: retained non-OSTree content under the legacy /boot/loader generation; \
                     bootctl may require manual cleanup before it can update $BOOT metadata."
                );
            }
            LegacyOstreeLoaderCleanup::NotLegacy => {}
        }
    }

    // 4. When we migrated to systemd-boot, drop the GRUB2 bits the user no
    //    longer needs. Keep them when --bootloader grub2 was used.
    if is_systemd_boot {
        for path in &["/boot/grub2", "/boot/efi/EFI/fedora"] {
            total_freed += remove_path_with_size(
                Path::new(path),
                "GRUB2 boot artifacts (migrated to systemd-boot)",
                dry_run,
            )?;
        }
    }

    // 5. Drop ostree-remount.service enablement. On a composefs-booted
    //    system OSTree bind mounts are irrelevant; the symlink may be
    //    re-created during boot by the target image's presets even though
    //    Phase 4 removed it from the deploy /etc.
    let remount_link =
        Path::new("/etc/systemd/system/local-fs.target.wants/ostree-remount.service");
    if remount_link.exists() || remount_link.is_symlink() {
        if dry_run {
            println!(
                "[DRY RUN] Would remove ostree-remount.service enablement (composefs doesn't need OSTree bind mounts)."
            );
        } else {
            std::fs::remove_file(remount_link)
                .with_context(|| format!("failed to remove {}", remount_link.display()))?;
            println!(
                "Removed ostree-remount.service enablement (composefs doesn't need OSTree bind mounts)."
            );
        }
    }

    // 6. Drop OSTree-only bookkeeping state under /var (issue #17).
    for var_path in &[
        "/var/lib/sysimage/ostree",
        "/var/lib/rpm-ostree",
        "/var/cache/rpm-ostree",
        "/var/lib/ostree-boot",
    ] {
        total_freed += remove_path_with_size(
            Path::new(var_path),
            "OSTree-only bookkeeping state under /var",
            dry_run,
        )?;
    }

    let human = format_bytes(total_freed);

    if dry_run {
        println!("\nWould reclaim: {} ({} bytes)", human, total_freed);
        println!("Re-run without --dry-run to apply.");
    } else {
        println!("\nReclaimed: {} ({} bytes)", human, total_freed);
        println!(
            "On-disk layout is now consistent with a fresh '{}' install.",
            if is_systemd_boot {
                "systemd-boot"
            } else {
                "GRUB2"
            }
        );
    }
    if !dry_run && let Err(e) = motd::clear_migration_reminder() {
        eprintln!("Warning: failed to clear login reminder: {e:#}");
    }
    Ok(())
}

fn finalize_systemd_boot_commit(
    esp_root: &Path,
    entries_dir: &Path,
    primary: &str,
    dry_run: bool,
) -> Result<()> {
    let fallback_entry = entries_dir.join("ostree-fallback-0.conf");
    if fallback_entry.exists() {
        if dry_run {
            println!(
                "[dry-run] would remove OSTree fallback BLS entry from ESP: {}",
                fallback_entry.display()
            );
        } else {
            std::fs::remove_file(&fallback_entry)
                .with_context(|| format!("failed to remove {}", fallback_entry.display()))?;
            println!("Removed OSTree fallback BLS entry from ESP.");
        }
    }

    let fallback_dir = esp_root.join("EFI/Linux/ostree-fallback");
    if fallback_dir.exists() {
        if dry_run {
            println!(
                "[dry-run] would remove OSTree fallback kernel/initrd from ESP: {}",
                fallback_dir.display()
            );
        } else {
            std::fs::remove_dir_all(&fallback_dir)
                .with_context(|| format!("failed to remove {}", fallback_dir.display()))?;
            println!("Removed OSTree fallback kernel/initrd from ESP.");
        }
    }

    // Drop the timeout now that composefs is the only entry.
    let loader_conf = esp_root.join("loader/loader.conf");
    if loader_conf.exists() {
        let body = format!("default {primary}\ntimeout 0\nconsole-mode keep\n");
        if dry_run {
            println!(
                "[dry-run] would update {} to make {} the sole default.",
                loader_conf.display(),
                primary
            );
        } else {
            std::fs::write(&loader_conf, body)
                .with_context(|| format!("failed to rewrite {}", loader_conf.display()))?;
        }
    }

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LegacyOstreeLoaderCleanup {
    NotLegacy,
    RetainedNonOstreeContent,
    Removed,
}

fn is_legacy_ostree_loader_target(target: &Path) -> bool {
    let mut components = target.components();
    let Some(Component::Normal(name)) = components.next() else {
        return false;
    };
    if components.next().is_some() {
        return false;
    }
    let Some(name) = name.to_str() else {
        return false;
    };
    name.strip_prefix("loader.").is_some_and(|generation| {
        !generation.is_empty() && generation.bytes().all(|b| b.is_ascii_digit())
    })
}

fn legacy_ostree_loader_target(boot_dir: &Path) -> Result<Option<PathBuf>> {
    let loader_link = boot_dir.join("loader");
    let metadata = match std::fs::symlink_metadata(&loader_link) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(e).with_context(|| format!("failed to inspect {}", loader_link.display()));
        }
    };
    if !metadata.file_type().is_symlink() {
        return Ok(None);
    }

    let target = std::fs::read_link(&loader_link)
        .with_context(|| format!("failed to read {}", loader_link.display()))?;
    if !is_legacy_ostree_loader_target(&target) {
        return Ok(None);
    }
    Ok(Some(boot_dir.join(target)))
}

fn only_ostree_entries(entries_dir: &Path) -> Result<bool> {
    for entry in std::fs::read_dir(entries_dir)
        .with_context(|| format!("failed to read {}", entries_dir.display()))?
    {
        let entry =
            entry.with_context(|| format!("failed to read entry in {}", entries_dir.display()))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let metadata = std::fs::symlink_metadata(entry.path())
            .with_context(|| format!("failed to inspect {}", entry.path().display()))?;
        if !metadata.is_file() || !name.starts_with("ostree-") || !name.ends_with(".conf") {
            return Ok(false);
        }
    }
    Ok(true)
}

fn cleanup_legacy_ostree_loader(
    boot_dir: &Path,
    dry_run: bool,
) -> Result<LegacyOstreeLoaderCleanup> {
    let loader_link = boot_dir.join("loader");
    let Some(loader_dir) = legacy_ostree_loader_target(boot_dir)? else {
        return Ok(LegacyOstreeLoaderCleanup::NotLegacy);
    };
    let metadata = match std::fs::symlink_metadata(&loader_dir) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if dry_run {
                println!(
                    "[dry-run] would remove broken legacy OSTree loader link: {}",
                    loader_link.display()
                );
            } else {
                std::fs::remove_file(&loader_link)
                    .with_context(|| format!("failed to remove {}", loader_link.display()))?;
            }
            return Ok(LegacyOstreeLoaderCleanup::Removed);
        }
        Err(e) => {
            return Err(e).with_context(|| format!("failed to inspect {}", loader_dir.display()));
        }
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Ok(LegacyOstreeLoaderCleanup::RetainedNonOstreeContent);
    }

    let entries_dir = loader_dir.join("entries");
    let staged_entries_dir = loader_dir.join("entries.staged");
    let mut removable_dirs = Vec::new();
    for entry in std::fs::read_dir(&loader_dir)
        .with_context(|| format!("failed to read {}", loader_dir.display()))?
    {
        let entry =
            entry.with_context(|| format!("failed to read entry in {}", loader_dir.display()))?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)
            .with_context(|| format!("failed to inspect {}", path.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Ok(LegacyOstreeLoaderCleanup::RetainedNonOstreeContent);
        }
        if path == entries_dir {
            if !only_ostree_entries(&path)? {
                return Ok(LegacyOstreeLoaderCleanup::RetainedNonOstreeContent);
            }
        } else if path == staged_entries_dir {
            if std::fs::read_dir(&path)
                .with_context(|| format!("failed to read {}", path.display()))?
                .next()
                .is_some()
            {
                return Ok(LegacyOstreeLoaderCleanup::RetainedNonOstreeContent);
            }
        } else {
            return Ok(LegacyOstreeLoaderCleanup::RetainedNonOstreeContent);
        }
        removable_dirs.push(path);
    }

    if dry_run {
        println!(
            "[dry-run] would remove legacy OSTree loader link and empty generation: {} -> {}",
            loader_link.display(),
            loader_dir.display()
        );
        return Ok(LegacyOstreeLoaderCleanup::Removed);
    }

    if entries_dir.is_dir() {
        for entry in std::fs::read_dir(&entries_dir)
            .with_context(|| format!("failed to read {}", entries_dir.display()))?
        {
            let entry = entry
                .with_context(|| format!("failed to read entry in {}", entries_dir.display()))?;
            std::fs::remove_file(entry.path())
                .with_context(|| format!("failed to remove {}", entry.path().display()))?;
        }
    }
    std::fs::remove_file(&loader_link)
        .with_context(|| format!("failed to remove {}", loader_link.display()))?;
    for dir in removable_dirs {
        std::fs::remove_dir(&dir).with_context(|| format!("failed to remove {}", dir.display()))?;
    }
    std::fs::remove_dir(&loader_dir)
        .with_context(|| format!("failed to remove {}", loader_dir.display()))?;

    Ok(LegacyOstreeLoaderCleanup::Removed)
}

fn dir_size(path: &Path) -> u64 {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    if meta.file_type().is_symlink() || meta.is_file() {
        return meta.len();
    }
    if !meta.is_dir() {
        return 0;
    }
    let mut total = 0u64;
    let rd = match std::fs::read_dir(path) {
        Ok(r) => r,
        Err(_) => return 0,
    };
    for entry in rd.flatten() {
        total += dir_size(&entry.path());
    }
    total
}

fn remove_path_with_size(path: &Path, label: &str, dry_run: bool) -> Result<u64> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => {
            return Err(e).with_context(|| format!("failed to inspect {}", path.display()));
        }
    };
    let size = dir_size(path);
    let human = format_bytes(size);
    if dry_run {
        println!(
            "[dry-run] would remove {} — {} ({})",
            path.display(),
            label,
            human
        );
        return Ok(size);
    }
    let res = if meta.is_dir() && !meta.file_type().is_symlink() {
        // On OSTree/bootc systems, /sysroot/ostree is typically a btrfs
        // subvolume — rm -rf returns EPERM. Try `btrfs subvolume delete`
        // first; if that fails, clear the immutable flag (chattr -i) and
        // fall back to remove_dir_all.
        //
        // A normal directory is not a Btrfs subvolume. Keep the probe quiet
        // because that expected fallback must not look like a commit error.
        let btrfs_ok = std::process::Command::new("btrfs")
            .args(["subvolume", "delete"])
            .arg(path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if btrfs_ok {
            Ok(())
        } else {
            // Clear immutable flag — OSTree often sets chattr +i on
            // /sysroot/ostree to prevent accidental deletion. Suppress
            // stderr: chattr on OSTree deploy checkouts (symlink farms)
            // produces thousands of "Operation not supported" lines.
            let _ = std::process::Command::new("chattr")
                .args(["-R", "-i"])
                .arg(path)
                .stderr(std::process::Stdio::null())
                .status();
            std::fs::remove_dir_all(path)
        }
    } else {
        std::fs::remove_file(path)
    };
    match res {
        Ok(()) => {
            println!("Removed {} — {} ({})", path.display(), label, human);
            Ok(size)
        }
        Err(e) => Err(e).with_context(|| format!("failed to remove {}", path.display())),
    }
}

/// True when `/sysroot/ostree` contains data that belongs to the source
/// OSTree deployment rather than the target bootc runtime.
fn has_legacy_ostree_content(ostree_dir: &Path) -> Result<bool> {
    let metadata = match std::fs::symlink_metadata(ostree_dir) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => {
            return Err(e).with_context(|| format!("failed to inspect {}", ostree_dir.display()));
        }
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Ok(true);
    }

    for entry in std::fs::read_dir(ostree_dir)
        .with_context(|| format!("failed to inspect {}", ostree_dir.display()))?
    {
        let entry = entry.with_context(|| format!("failed to inspect {}", ostree_dir.display()))?;
        if entry.file_name() != "bootc" {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Remove source OSTree data without deleting `/sysroot/ostree/bootc`, which
/// is owned by the target bootc installation for its container storage.
fn remove_legacy_ostree_content(ostree_dir: &Path, label: &str, dry_run: bool) -> Result<u64> {
    let metadata = match std::fs::symlink_metadata(ostree_dir) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => {
            return Err(e).with_context(|| format!("failed to inspect {}", ostree_dir.display()));
        }
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return remove_path_with_size(ostree_dir, label, dry_run);
    }

    let mut total_freed = 0;
    for entry in std::fs::read_dir(ostree_dir)
        .with_context(|| format!("failed to inspect {}", ostree_dir.display()))?
    {
        let entry = entry.with_context(|| format!("failed to inspect {}", ostree_dir.display()))?;
        if entry.file_name() == "bootc" {
            continue;
        }
        total_freed += remove_path_with_size(&entry.path(), label, dry_run)?;
    }
    Ok(total_freed)
}

fn format_bytes(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} {}", n, UNITS[0])
    } else {
        format!("{:.2} {}", v, UNITS[i])
    }
}

#[derive(Debug, PartialEq, Eq)]
struct SysrootMount {
    device: String,
    fstype: String,
    options: String,
}

/// A private, writable mount of the same subvolume currently mounted at
/// `/sysroot`. Its Drop implementation ensures a failed cleanup cannot leave
/// the backing filesystem mounted under /var/tmp.
struct SysrootCleanupMount {
    tempdir: tempfile::TempDir,
}

impl SysrootCleanupMount {
    fn root(&self) -> &Path {
        self.tempdir.path()
    }
}

impl Drop for SysrootCleanupMount {
    fn drop(&mut self) {
        match std::process::Command::new("umount")
            .arg(self.root())
            .output()
        {
            Ok(output) if output.status.success() => {}
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                eprintln!(
                    "warning: failed to unmount temporary sysroot cleanup mount {}: {}",
                    self.root().display(),
                    stderr.trim()
                );
            }
            Err(e) => {
                eprintln!(
                    "warning: failed to execute umount for temporary sysroot cleanup mount {}: {e}",
                    self.root().display()
                );
            }
        }
    }
}

/// Parse the `/sysroot` record from `/proc/mounts` into an independent,
/// writable mount. Btrfs needs its active subvolume carried over; otherwise a
/// remount lands at the top level and does not expose `/sysroot/ostree`.
fn parse_sysroot_mount(mounts: &str) -> Option<SysrootMount> {
    let fields = mounts
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>())
        .find(|fields| fields.len() >= 4 && fields[1] == "/sysroot")?;
    let device = fields[0]
        .split_once('[')
        .map_or(fields[0], |(device, _)| device);
    let fstype = fields[2];
    let mount_options = fields[3];

    let mut options = vec!["rw".to_string()];
    if fstype == "btrfs" {
        if let Some(subvolume) = mount_options
            .split(',')
            .find_map(|option| option.strip_prefix("subvol="))
        {
            options.push(format!("subvol={subvolume}"));
        } else if let Some(subvolume_id) = mount_options
            .split(',')
            .find_map(|option| option.strip_prefix("subvolid="))
        {
            options.push(format!("subvolid={subvolume_id}"));
        }
    }

    Some(SysrootMount {
        device: device.to_string(),
        fstype: fstype.to_string(),
        options: options.join(","),
    })
}

/// Mount the device backing `/sysroot` at a private temporary location,
/// bypassing the composefs EROFS overlay so that old OSTree paths can be
/// mutated directly on the underlying filesystem.
fn mount_sysroot_for_cleanup() -> Result<SysrootCleanupMount> {
    let mounts = std::fs::read_to_string("/proc/mounts").context("failed to read /proc/mounts")?;
    let sysroot_mount = parse_sysroot_mount(&mounts)
        .context("could not find mount for /sysroot in /proc/mounts")?;
    let tempdir = tempfile::Builder::new()
        .prefix("bootc-migrate-commit-")
        .tempdir_in("/var/tmp")
        .context("failed to create temporary sysroot cleanup mountpoint")?;

    let output = std::process::Command::new("mount")
        .args(["-t", &sysroot_mount.fstype, "-o", &sysroot_mount.options])
        .arg(&sysroot_mount.device)
        .arg(tempdir.path())
        .output()
        .context("failed to execute mount for alt-root cleanup")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "mount {} at {} with {} failed: {}",
            sysroot_mount.device,
            tempdir.path().display(),
            sysroot_mount.options,
            stderr.trim()
        );
    }

    let cleanup_mount = SysrootCleanupMount { tempdir };
    if Path::new("/sysroot/ostree").exists() && !cleanup_mount.root().join("ostree").exists() {
        anyhow::bail!(
            "temporary sysroot mount {} does not expose the existing /sysroot/ostree",
            cleanup_mount.root().display()
        );
    }
    Ok(cleanup_mount)
}

/// Undo a partial or failed migration. Removes all composefs artifacts
/// (staged deployments, boot artifacts, BLS entries, loopback images,
/// composefs object store) while leaving the OSTree deployment intact.
pub fn undo(dry_run: bool, full: bool) -> Result<()> {
    // Always release the migration lock so a subsequent run doesn't fail
    // with "already running". The lock guard drops automatically at process
    // exit, but if the previous run crashed mid-phase the lock can linger.
    let lock_path = "/var/run/bootc-migrate.lock";
    if !dry_run {
        let _ = std::fs::remove_file(lock_path);
    }

    println!("=== Undoing composefs migration ===");
    if dry_run {
        println!("*** DRY RUN — no changes will be made ***");
    }

    // /sysroot is mounted read-only on an OSTree-booted system (composefs or
    // classic). Remount rw so we can delete staged deployments and loopback
    // images that live there. Ignore the error — if it's already rw this is
    // a no-op; if it genuinely can't be made rw the subsequent removes will
    // surface the real error.
    if !dry_run {
        let _ = std::process::Command::new("mount")
            .args(["-o", "remount,rw", "/sysroot"])
            .status();
    }

    let mut removed = 0usize;
    let mut skipped = 0usize;

    // 1. Remove staged composefs deployments.
    let deploy_dir = Path::new("/sysroot/state/deploy");
    if deploy_dir.exists()
        && let Ok(rd) = std::fs::read_dir(deploy_dir)
    {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            // Skip the OSTree deploy dir (numeric or short names).
            // Composefs deploy dirs are long hex strings (64+ chars).
            if name.len() < 40 {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                println!("Removing staged deployment: {}", path.display());
                if !dry_run {
                    std::fs::remove_dir_all(&path)
                        .with_context(|| format!("failed to remove {}", path.display()))?;
                }
                removed += 1;
            }
        }
    }
    if removed == 0 {
        println!("No composefs deployments found in /sysroot/state/deploy/.");
    }

    // 2. Remove composefs boot artifacts.
    let boot_dir = Path::new("/boot");
    if let Ok(rd) = std::fs::read_dir(boot_dir) {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("bootc_composefs-") {
                let path = entry.path();
                println!("Removing boot artifacts: {}", path.display());
                if !dry_run {
                    std::fs::remove_dir_all(&path)
                        .with_context(|| format!("failed to remove {}", path.display()))?;
                }
                removed += 1;
            }
        }
    }

    // 3. Remove composefs BLS entries from /boot/loader/entries.
    let bls_dir = Path::new("/boot/loader/entries");
    if bls_dir.exists()
        && let Ok(rd) = std::fs::read_dir(bls_dir)
    {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("bootc_") || name.starts_with("ostree-fallback-") {
                let path = entry.path();
                println!("Removing BLS entry: {}", path.display());
                if !dry_run {
                    std::fs::remove_file(&path)
                        .with_context(|| format!("failed to remove {}", path.display()))?;
                }
                removed += 1;
            }
        }
    }

    // 4. Remove composefs BLS entries from ESP.
    for esp in &["/boot/efi", "/efi"] {
        let esp_entries = Path::new(esp).join("loader/entries");
        if esp_entries.exists() {
            if let Ok(rd) = std::fs::read_dir(&esp_entries) {
                for entry in rd.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.starts_with("bootc_") || name.starts_with("ostree-fallback-") {
                        let path = entry.path();
                        println!("Removing ESP BLS entry: {}", path.display());
                        if !dry_run {
                            std::fs::remove_file(&path)
                                .with_context(|| format!("failed to remove {}", path.display()))?;
                        }
                        removed += 1;
                    }
                }
            }
            // Remove loader.conf if we wrote one.
            let loader_conf = Path::new(esp).join("loader/loader.conf");
            if loader_conf.exists() {
                println!("Removing ESP loader.conf: {}", loader_conf.display());
                if !dry_run {
                    std::fs::remove_file(&loader_conf)?;
                    removed += 1;
                }
            }
        }
        // Remove systemd-boot from ESP.
        let sd_dir = Path::new(esp).join("EFI/systemd");
        if sd_dir.exists() {
            println!("Removing systemd-boot from ESP: {}", sd_dir.display());
            if !dry_run {
                std::fs::remove_dir_all(&sd_dir)?;
                removed += 1;
            }
        }
        let esp_linux = Path::new(esp).join("EFI/Linux");
        if esp_linux.exists()
            && let Ok(rd) = std::fs::read_dir(&esp_linux)
        {
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with("bootc_composefs-") {
                    let path = entry.path();
                    println!("Removing ESP EFI/Linux entry: {}", path.display());
                    if !dry_run {
                        std::fs::remove_dir_all(&path)
                            .with_context(|| format!("failed to remove {}", path.display()))?;
                    }
                    removed += 1;
                }
            }
        }
    }

    // 5. Remove composefs loopback image (only with --full).
    if full {
        let loopback = Path::new("/sysroot/composefs-loopback.ext4");
        if loopback.exists() {
            println!("Removing composefs loopback image: {}", loopback.display());
            if !dry_run {
                std::fs::remove_file(loopback)?;
                removed += 1;
            }
        }

        // 6. Remove composefs object store (only with --full).
        let composefs_dir = Path::new("/sysroot/composefs");
        if composefs_dir.exists() {
            let has_objects = composefs_dir.join("objects").exists();
            let has_images = composefs_dir.join("images").exists();
            if has_objects || has_images {
                println!(
                    "Removing composefs object store: {}",
                    composefs_dir.display()
                );
                if !dry_run {
                    for sub in &["objects", "images", "streams", "tmp"] {
                        let p = composefs_dir.join(sub);
                        if p.exists() {
                            std::fs::remove_dir_all(&p)
                                .with_context(|| format!("failed to remove {}", p.display()))?;
                        }
                    }
                    removed += 1;
                }
            } else {
                println!("Composefs directory exists but is empty (no objects/images).");
                skipped += 1;
            }
        }
    } else {
        println!("Composefs object store and loopback preserved (re-run --full to clean).");
    }

    // 7. Optionally warn about NVRAM entries (can't clean those from userspace easily).
    println!();
    if dry_run {
        println!("Would remove {} artifact(s).", removed);
        println!("Re-run without --dry-run to apply.");
    } else {
        println!("Removed {} composefs artifact(s).", removed);
        if skipped > 0 {
            println!("{} path(s) skipped (empty or already clean).", skipped);
        }
        println!("The system is now in its pre-migration OSTree state.");
        println!("Run 'bootc-migrate --target-image <image>' to try again.");
    }
    if !dry_run && let Err(e) = motd::clear_migration_reminder() {
        eprintln!("Warning: failed to clear login reminder: {e:#}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(2048), "2.00 KiB");
        assert_eq!(format_bytes(10 * 1024 * 1024), "10.00 MiB");
        assert_eq!(format_bytes(5 * 1024 * 1024 * 1024), "5.00 GiB");
    }

    #[test]
    fn parse_sysroot_mount_preserves_the_active_btrfs_subvolume() {
        struct Case {
            mounts: &'static str,
            expected: Option<SysrootMount>,
        }

        let cases = [
            Case {
                mounts: "/dev/nvme0n1p3 /sysroot btrfs ro,relatime,subvolid=256,subvol=/root 0 0\n",
                expected: Some(SysrootMount {
                    device: "/dev/nvme0n1p3".into(),
                    fstype: "btrfs".into(),
                    options: "rw,subvol=/root".into(),
                }),
            },
            Case {
                mounts: "/dev/vda2[/root] /sysroot btrfs ro,subvolid=256 0 0\n",
                expected: Some(SysrootMount {
                    device: "/dev/vda2".into(),
                    fstype: "btrfs".into(),
                    options: "rw,subvolid=256".into(),
                }),
            },
            Case {
                mounts: "/dev/vda2 /sysroot ext4 ro,relatime 0 0\n",
                expected: Some(SysrootMount {
                    device: "/dev/vda2".into(),
                    fstype: "ext4".into(),
                    options: "rw".into(),
                }),
            },
            Case {
                mounts: "/dev/vda2 / btrfs rw,subvol=/root 0 0\n",
                expected: None,
            },
        ];

        for case in cases {
            assert_eq!(parse_sysroot_mount(case.mounts), case.expected);
        }
    }

    #[test]
    fn legacy_ostree_cleanup_preserves_target_bootc_storage() {
        let temp = tempdir().unwrap();
        let ostree = temp.path().join("ostree");
        let bootc_storage = ostree.join("bootc/storage");
        let legacy_repo = ostree.join("repo/objects");
        let legacy_deploy = ostree.join("deploy/default/deploy");
        std::fs::create_dir_all(&bootc_storage).unwrap();
        std::fs::create_dir_all(&legacy_repo).unwrap();
        std::fs::create_dir_all(&legacy_deploy).unwrap();
        std::fs::write(bootc_storage.join("target-image"), "keep\n").unwrap();
        std::fs::write(legacy_repo.join("object"), "remove\n").unwrap();
        std::fs::write(legacy_deploy.join("deployment"), "remove\n").unwrap();

        assert!(has_legacy_ostree_content(&ostree).unwrap());
        assert!(remove_legacy_ostree_content(&ostree, "legacy OSTree data", true).unwrap() > 0);
        assert!(legacy_repo.exists());
        assert!(bootc_storage.exists());

        assert!(remove_legacy_ostree_content(&ostree, "legacy OSTree data", false).unwrap() > 0);
        assert!(!legacy_repo.exists());
        assert!(!legacy_deploy.exists());
        assert!(bootc_storage.exists());
        assert!(!has_legacy_ostree_content(&ostree).unwrap());
    }

    #[test]
    fn systemd_boot_finalization_honors_dry_run() {
        let temp = tempdir().unwrap();
        let esp = temp.path().join("esp");
        let entries = esp.join("loader/entries");
        let fallback_entry = entries.join("ostree-fallback-0.conf");
        let fallback_dir = esp.join("EFI/Linux/ostree-fallback");
        let loader_conf = esp.join("loader/loader.conf");
        std::fs::create_dir_all(&entries).unwrap();
        std::fs::create_dir_all(&fallback_dir).unwrap();
        std::fs::write(&fallback_entry, "legacy entry\n").unwrap();
        std::fs::write(fallback_dir.join("vmlinuz"), "legacy kernel\n").unwrap();
        std::fs::write(&loader_conf, "default old\ntimeout 5\n").unwrap();

        finalize_systemd_boot_commit(&esp, &entries, "bootc_target", true).unwrap();
        assert!(fallback_entry.exists());
        assert!(fallback_dir.exists());
        assert_eq!(
            std::fs::read_to_string(&loader_conf).unwrap(),
            "default old\ntimeout 5\n"
        );

        finalize_systemd_boot_commit(&esp, &entries, "bootc_target", false).unwrap();
        assert!(!fallback_entry.exists());
        assert!(!fallback_dir.exists());
        assert_eq!(
            std::fs::read_to_string(&loader_conf).unwrap(),
            "default bootc_target\ntimeout 0\nconsole-mode keep\n"
        );
    }

    #[test]
    fn legacy_ostree_loader_target_is_a_single_generation_directory() {
        struct Case {
            target: &'static str,
            expected: bool,
        }

        let cases = [
            Case {
                target: "loader.0",
                expected: true,
            },
            Case {
                target: "loader.12",
                expected: true,
            },
            Case {
                target: "loader.",
                expected: false,
            },
            Case {
                target: "loader.current",
                expected: false,
            },
            Case {
                target: "../loader.1",
                expected: false,
            },
            Case {
                target: "/boot/loader.1",
                expected: false,
            },
            Case {
                target: "loader.1/entries",
                expected: false,
            },
        ];

        for case in cases {
            assert_eq!(
                is_legacy_ostree_loader_target(Path::new(case.target)),
                case.expected,
                "{}",
                case.target
            );
        }
    }

    #[test]
    fn legacy_ostree_loader_cleanup_is_dry_run_safe_and_removes_only_ostree_entries() {
        let temp = tempdir().unwrap();
        let boot = temp.path().join("boot");
        let loader_dir = boot.join("loader.1");
        let entries = loader_dir.join("entries");
        std::fs::create_dir_all(&entries).unwrap();
        std::fs::write(entries.join("ostree-0.conf"), "current\n").unwrap();
        std::fs::write(entries.join("ostree-1.conf"), "previous\n").unwrap();
        std::os::unix::fs::symlink("loader.1", boot.join("loader")).unwrap();

        assert_eq!(
            cleanup_legacy_ostree_loader(&boot, true).unwrap(),
            LegacyOstreeLoaderCleanup::Removed
        );
        assert!(boot.join("loader").is_symlink());
        assert!(entries.join("ostree-0.conf").exists());

        assert_eq!(
            cleanup_legacy_ostree_loader(&boot, false).unwrap(),
            LegacyOstreeLoaderCleanup::Removed
        );
        assert!(!boot.join("loader").exists());
        assert!(!boot.join("loader").is_symlink());
        assert!(!loader_dir.exists());
    }

    #[test]
    fn legacy_ostree_loader_cleanup_preserves_non_ostree_entries() {
        let temp = tempdir().unwrap();
        let boot = temp.path().join("boot");
        let loader_dir = boot.join("loader.1");
        let entries = loader_dir.join("entries");
        std::fs::create_dir_all(&entries).unwrap();
        std::fs::write(entries.join("custom-kernel.conf"), "custom\n").unwrap();
        std::os::unix::fs::symlink("loader.1", boot.join("loader")).unwrap();

        assert_eq!(
            cleanup_legacy_ostree_loader(&boot, false).unwrap(),
            LegacyOstreeLoaderCleanup::RetainedNonOstreeContent
        );
        assert!(boot.join("loader").is_symlink());
        assert!(entries.join("custom-kernel.conf").exists());
    }
}
