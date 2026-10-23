//! Persistent data carried from a package-managed root into an OSTree target.
//!
//! Package hosts do not have bootc's `/var` contract yet.  The target still
//! expects the bootc layout, however: user homes are under `/var/home`, and
//! durable application state belongs below the stateroot `/var`.  This module
//! keeps that translation explicit and testable.  Home data is mounted or
//! bind-mounted; it is never copied.  Selected `/var` trees, `/opt`, and
//! `/usr/local` use the existing reflink-aware copy helpers.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::time::Instant;

use crate::xattr::{self, CopyCounts};

/// The relative paths under `/var/lib` that are user or application state.
const VAR_LIB_ALLOWLIST: &[&str] = &[
    "containers",
    "flatpak",
    "libvirt",
    "NetworkManager",
    "bluetooth",
    "AccountsService",
    "systemd/linger",
];

/// Package-manager state must remain owned by the target image.
const PACKAGE_STATE_DIRS: &[&str] = &["rpm", "dnf", "apt", "dpkg", "pacman", "yum", "zypper"];

/// A package host's `/home` location as seen by the new deployment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HomeLayout {
    /// `/home` is an ordinary directory on the package host's root.
    RootDirectory,
    /// `/home` is a dedicated filesystem or Btrfs subvolume.
    Filesystem {
        source: String,
        fs_type: String,
        options: String,
    },
}

impl HomeLayout {
    /// Human-readable layout label used in reports and logs.
    pub fn description(&self) -> &'static str {
        match self {
            Self::RootDirectory => "root directory (bind-mounted)",
            Self::Filesystem { fs_type, .. } if fs_type == "btrfs" => "Btrfs subvolume/filesystem",
            Self::Filesystem { .. } => "dedicated filesystem",
        }
    }

    /// The deployed fstab line that exposes the source home at `/var/home`.
    pub fn fstab_entry(&self) -> String {
        match self {
            Self::RootDirectory => {
                "/sysroot/home /var/home none bind,x-systemd.requires-mounts-for=/sysroot/home 0 0\n"
                    .to_string()
            }
            Self::Filesystem {
                source,
                fs_type,
                options,
            } => format!(
                "{source} /var/home {fs_type} {options} 0 0\n"
            ),
        }
    }
}

/// Decode mountinfo's octal path escapes (`\\040`, `\\011`, `\\134`).
fn decode_mountinfo_path(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if i + 3 < bytes.len() && bytes[i] == b'\\' {
            let octal = &bytes[i + 1..i + 4];
            if octal.iter().all(|b| (b'0'..=b'7').contains(b)) {
                let value = (octal[0] - b'0') * 64 + (octal[1] - b'0') * 8 + octal[2] - b'0';
                out.push(value);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MountInfoHome {
    source: String,
    fs_type: String,
    mount_options: String,
    super_options: String,
}

/// Parse the visible `/home` mount from `/proc/self/mountinfo`.
fn parse_home_mount(mountinfo: &str) -> Option<MountInfoHome> {
    mountinfo.lines().rev().find_map(|line| {
        let (left, right) = line.split_once(" - ")?;
        let fields: Vec<_> = left.split_whitespace().collect();
        if fields.len() < 6 || decode_mountinfo_path(fields[4]) != "/home" {
            return None;
        }
        let right: Vec<_> = right.split_whitespace().collect();
        if right.len() < 3 {
            return None;
        }
        Some(MountInfoHome {
            source: decode_mountinfo_path(right[1]),
            fs_type: right[0].to_string(),
            mount_options: fields[5].to_string(),
            super_options: right[2..].join(","),
        })
    })
}

/// Return the active, non-comment fstab row for `mountpoint`.
fn fstab_row(fstab: &str, mountpoint: &str) -> Option<Vec<String>> {
    fstab.lines().find_map(|line| {
        let fields: Vec<_> = line.split_whitespace().map(str::to_string).collect();
        (fields.len() >= 4 && !line.trim_start().starts_with('#') && fields[1] == mountpoint)
            .then_some(fields)
    })
}

/// Discover how a package host's `/home` must be exposed after installation.
///
/// An ordinary directory has no `/home` mountinfo row and becomes a bind mount
/// from the physical root.  A visible mount is retained in place and only its
/// mountpoint changes to `/var/home`; Btrfs `subvol=home` is therefore reused
/// without copying its contents.
pub fn detect_home_layout(mountinfo: &str, fstab: &str) -> HomeLayout {
    if let Some(row) = fstab_row(fstab, "/home") {
        return HomeLayout::Filesystem {
            source: row[0].clone(),
            fs_type: row[2].clone(),
            options: row[3].clone(),
        };
    }

    let Some(mount) = parse_home_mount(mountinfo) else {
        return HomeLayout::RootDirectory;
    };

    let mut options = mount.mount_options;
    if !mount.super_options.is_empty() {
        if !options.is_empty() {
            options.push(',');
        }
        options.push_str(&mount.super_options);
    }
    HomeLayout::Filesystem {
        source: mount.source,
        fs_type: mount.fs_type,
        options,
    }
}

/// Whether a relative path below `/var` is package-managed or ephemeral state
/// that MUST NOT be copied from the source.
pub fn is_excluded_var_path(relative: &Path) -> bool {
    let mut components = relative.components();
    let Some(first) = components.next().and_then(|c| c.as_os_str().to_str()) else {
        return true;
    };
    if matches!(first, "cache" | "tmp" | "run" | "lock" | "home") {
        return true;
    }
    if first != "lib" {
        return false;
    }
    let Some(second) = components.next().and_then(|c| c.as_os_str().to_str()) else {
        return true;
    };
    PACKAGE_STATE_DIRS.contains(&second)
        || !VAR_LIB_ALLOWLIST
            .iter()
            .any(|allowed| allowed.split('/').next() == Some(second))
}
/// Whether a path below `/var/lib` is one of the explicitly carried trees.
pub fn is_allowed_var_lib_path(relative: &Path, target_ships_docker: bool) -> bool {
    let mut components = relative.components();
    let Some(first) = components.next().and_then(|c| c.as_os_str().to_str()) else {
        return false;
    };
    if first == "docker" {
        return target_ships_docker;
    }
    let rel = relative.to_string_lossy();
    VAR_LIB_ALLOWLIST
        .iter()
        .any(|allowed| rel == *allowed || rel.starts_with(&format!("{allowed}/")))
}

/// A single tree carried into the target state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CarriedTree {
    pub source: String,
    pub destination: String,
    pub action: String,
    pub files: u64,
    pub bytes: u64,
    pub elapsed_ms: u64,
    pub special_files_skipped: u64,
}

/// The report written before reboot.  `home` is always present and has zero
/// copied bytes because its data is mounted or bind-mounted instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageDataReport {
    pub home: HomeReport,
    pub trees: Vec<CarriedTree>,
    pub total_files: u64,
    pub total_bytes: u64,
    pub elapsed_ms: u64,
}

/// The no-copy home operation recorded in [`PackageDataReport`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HomeReport {
    pub source: String,
    pub destination: String,
    pub layout: String,
    pub action: String,
    pub fstab_entry: String,
    pub copied_bytes: u64,
}

impl PackageDataReport {
    fn new(home: &HomeLayout) -> Self {
        Self {
            home: HomeReport {
                source: "/home".to_string(),
                destination: "/var/home".to_string(),
                layout: home.description().to_string(),
                action: match home {
                    HomeLayout::RootDirectory => "bind-mount".to_string(),
                    HomeLayout::Filesystem { .. } => "mount-in-place".to_string(),
                },
                fstab_entry: home.fstab_entry(),
                copied_bytes: 0,
            },
            trees: Vec::new(),
            total_files: 0,
            total_bytes: 0,
            elapsed_ms: 0,
        }
    }

    fn add_tree(&mut self, tree: CarriedTree) {
        self.total_files += tree.files;
        self.total_bytes += tree.bytes;
        self.trees.push(tree);
    }
}

/// Stage package-host data into a target OSTree deployment.
///
/// `source_root` is the package host's physical root and `target_var` is the
/// target's stateroot `/var`.  The target image's `/var` skeleton must already
/// be present in `target_var`; source package databases and caches are never
/// copied over it.
pub fn stage(
    source_root: &Path,
    target_var: &Path,
    deploy_root: &Path,
    home: &HomeLayout,
    target_ships_docker: bool,
) -> Result<PackageDataReport> {
    let started = Instant::now();
    fs::create_dir_all(target_var)
        .with_context(|| format!("creating target stateroot var {}", target_var.display()))?;
    let mut report = PackageDataReport::new(home);

    rewrite_home_fstab(&deploy_root.join("etc/fstab"), &home.fstab_entry())?;

    let source_var = source_root.join("var");
    if source_var.is_dir() {
        for entry in fs::read_dir(&source_var)
            .with_context(|| format!("reading {}", source_var.display()))?
        {
            let entry = entry?;
            let name = entry.file_name();
            let relative = Path::new(&name);
            if relative == Path::new("lib") || is_excluded_var_path(relative) {
                continue;
            }
            let source = entry.path();
            let destination = target_var.join(relative);
            if let Some(tree) = copy_tree(
                &source,
                &destination,
                &format!("/var/{}", name.to_string_lossy()),
            )? {
                report.add_tree(tree);
            }
        }

        let source_lib = source_var.join("lib");
        if source_lib.is_dir() {
            for entry in fs::read_dir(&source_lib)
                .with_context(|| format!("reading {}", source_lib.display()))?
            {
                let entry = entry?;
                let name = entry.file_name();
                let relative = Path::new(&name);
                let mut selected = Vec::new();
                if is_allowed_var_lib_path(relative, target_ships_docker) {
                    selected.push(relative.to_path_buf());
                }
                for allowed in VAR_LIB_ALLOWLIST {
                    let allowed_path = Path::new(allowed);
                    if allowed_path.parent() == Some(relative)
                        && is_allowed_var_lib_path(allowed_path, target_ships_docker)
                    {
                        selected.push(allowed_path.to_path_buf());
                    }
                }
                for relative in selected {
                    let source = source_lib.join(&relative);
                    let destination = target_var.join("lib").join(&relative);
                    if let Some(tree) = copy_tree(
                        &source,
                        &destination,
                        &format!("/var/lib/{}", relative.display()),
                    )? {
                        report.add_tree(tree);
                    }
                }
            }
        }
    }

    for (source_rel, destination_rel) in [("opt", "opt"), ("usr/local", "usrlocal")] {
        let source = source_root.join(source_rel);
        let destination = target_var.join(destination_rel);
        if source.exists() || source.is_symlink() {
            if let Some(tree) = copy_tree(&source, &destination, &format!("/{source_rel}"))? {
                report.add_tree(tree);
            }
            ensure_persistent_path(deploy_root, source_rel, destination_rel)?;
        }
    }

    report.elapsed_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
    Ok(report)
}

fn rewrite_home_fstab(path: &Path, entry: &str) -> Result<()> {
    let current = fs::read_to_string(path).unwrap_or_default();
    let mut out = String::new();
    let mut replaced = false;
    for line in current.lines() {
        let mountpoint = line.split_whitespace().nth(1);
        if !line.trim_start().starts_with('#') && matches!(mountpoint, Some("/home" | "/var/home"))
        {
            if !replaced {
                out.push_str(entry);
                replaced = true;
            }
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !replaced {
        out.push_str(entry);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, out).with_context(|| format!("writing {}", path.display()))
}

fn append_fstab_entry(path: &Path, entry: &str) -> Result<()> {
    let current = fs::read_to_string(path).unwrap_or_default();
    let mountpoint = entry.split_whitespace().nth(1).unwrap_or_default();
    if current.lines().any(|line| {
        let line = line.trim();
        !line.is_empty()
            && !line.starts_with('#')
            && line.split_whitespace().nth(1) == Some(mountpoint)
    }) {
        return Ok(());
    }
    let mut out = current;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(entry);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, out).with_context(|| format!("writing {}", path.display()))
}

fn ensure_persistent_path(
    deploy_root: &Path,
    source_rel: &str,
    destination_rel: &str,
) -> Result<()> {
    let path = deploy_root.join(source_rel);
    let target = format!("/var/{destination_rel}");
    if path.is_symlink() {
        if fs::read_link(&path).ok().as_deref() == Some(Path::new(&target)) {
            return Ok(());
        }
        fs::remove_file(&path)?;
        std::os::unix::fs::symlink(&target, &path)
            .with_context(|| format!("linking {} to {target}", path.display()))?;
        return Ok(());
    } else if !path.exists() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        std::os::unix::fs::symlink(&target, &path)
            .with_context(|| format!("linking {} to {target}", path.display()))?;
        return Ok(());
    } else if path.is_dir() && fs::read_dir(&path)?.next().is_none() {
        fs::remove_dir(&path)?;
        std::os::unix::fs::symlink(&target, &path)
            .with_context(|| format!("linking {} to {target}", path.display()))?;
        return Ok(());
    }

    // A target image may intentionally ship a real directory.  A bind entry
    // still makes the carried package-host tree visible without deleting the
    // target's files while staging.
    append_fstab_entry(
        &deploy_root.join("etc/fstab"),
        &format!("{target} /{source_rel} none bind 0 0\n"),
    )
}

fn copy_tree(source: &Path, destination: &Path, source_label: &str) -> Result<Option<CarriedTree>> {
    if !source.exists() && !source.is_symlink() {
        return Ok(None);
    }
    let started = Instant::now();
    let mut last_tick = started;
    let mut progress = |counts: CopyCounts| {
        if last_tick.elapsed() >= std::time::Duration::from_secs(5) {
            last_tick = Instant::now();
            println!(
                "[package-data] {source_label}: {} files, {} MB, still copying…",
                counts.files,
                counts.bytes / (1024 * 1024)
            );
        }
    };
    let counts = match source.symlink_metadata()?.file_type() {
        ty if ty.is_dir() => {
            xattr::copy_dir_all_with_xattrs_progress(source, destination, &mut progress)?
        }
        ty if ty.is_file() => {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            xattr::copy_file_with_xattrs(source, destination)?;
            CopyCounts {
                files: 1,
                bytes: fs::metadata(source)?.len(),
                specials_skipped: 0,
            }
        }
        ty if ty.is_symlink() => {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            if destination.exists() || destination.is_symlink() {
                fs::remove_file(destination)?;
            }
            std::os::unix::fs::symlink(fs::read_link(source)?, destination)?;
            CopyCounts {
                files: 1,
                bytes: 0,
                specials_skipped: 0,
            }
        }
        _ => return Ok(None),
    };
    let destination_label = destination.display().to_string();
    Ok(Some(CarriedTree {
        source: source_label.to_string(),
        destination: destination_label,
        action: "reflink-copy".to_string(),
        files: counts.files,
        bytes: counts.bytes,
        elapsed_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
        special_files_skipped: counts.specials_skipped,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const MOUNTINFO_HOME: &str =
        "30 25 0:29 /home /home rw,relatime - btrfs /dev/vda3 rw,subvol=home,subvolid=256\n";

    #[test]
    fn home_layout_table_keeps_mounts_and_avoids_copying() {
        let cases = [
            ("", "", HomeLayout::RootDirectory),
            (
                "",
                "UUID=home /home ext4 defaults 0 0\n",
                HomeLayout::Filesystem {
                    source: "UUID=home".to_string(),
                    fs_type: "ext4".to_string(),
                    options: "defaults".to_string(),
                },
            ),
            (
                MOUNTINFO_HOME,
                "UUID=home /home btrfs subvol=home,compress=zstd 0 0\n",
                HomeLayout::Filesystem {
                    source: "UUID=home".to_string(),
                    fs_type: "btrfs".to_string(),
                    options: "subvol=home,compress=zstd".to_string(),
                },
            ),
            (
                "40 25 0:30 / /home rw,relatime - ext4 /dev/vda4 rw\n",
                "",
                HomeLayout::Filesystem {
                    source: "/dev/vda4".to_string(),
                    fs_type: "ext4".to_string(),
                    options: "rw,relatime,rw".to_string(),
                },
            ),
        ];
        for (mountinfo, fstab, expected) in cases {
            let got = detect_home_layout(mountinfo, fstab);
            assert_eq!(got, expected);
            assert!(got.fstab_entry().contains("/var/home"));
        }
        assert!(matches!(
            detect_home_layout("", ""),
            HomeLayout::RootDirectory
        ));
    }

    #[test]
    fn home_root_directory_is_a_bind_not_a_copy() {
        let home = HomeLayout::RootDirectory;
        assert_eq!(
            home.fstab_entry(),
            "/sysroot/home /var/home none bind,x-systemd.requires-mounts-for=/sysroot/home 0 0\n"
        );
    }

    #[test]
    fn var_policy_excludes_caches_and_package_databases() {
        for path in [
            "cache",
            "cache/dnf",
            "lib/rpm",
            "lib/dnf",
            "lib/apt/lists",
            "lib/dpkg/status",
            "lib/pacman/local",
            "tmp",
            "run",
            "home",
        ] {
            assert!(is_excluded_var_path(Path::new(path)), "{path}");
        }
        for path in [
            "log",
            "rebase-test",
            "lib/containers",
            "lib/flatpak",
            "lib/systemd/linger",
        ] {
            assert!(!is_excluded_var_path(Path::new(path)), "{path}");
        }
        assert!(is_allowed_var_lib_path(Path::new("containers"), false));
        assert!(!is_allowed_var_lib_path(Path::new("docker"), false));
        assert!(is_allowed_var_lib_path(Path::new("docker"), true));
        assert!(!is_allowed_var_lib_path(Path::new("rpm"), true));
    }

    #[test]
    fn stage_carries_selected_trees_and_records_zero_copy_home() {
        let root = tempdir().unwrap();
        let source = root.path().join("source");
        let target_var = root.path().join("target/ostree/deploy/default/var");
        let deploy = root
            .path()
            .join("target/ostree/deploy/default/deploy/abc.0");
        fs::create_dir_all(source.join("var/lib/containers/storage")).unwrap();
        fs::create_dir_all(source.join("var/lib/systemd/linger")).unwrap();
        fs::create_dir_all(source.join("var/lib/rpm")).unwrap();
        fs::create_dir_all(source.join("var/rebase-test")).unwrap();
        fs::create_dir_all(source.join("var/cache")).unwrap();
        fs::create_dir_all(source.join("home/alice")).unwrap();
        fs::create_dir_all(source.join("opt/bin")).unwrap();
        fs::create_dir_all(source.join("usr/local/bin")).unwrap();
        fs::write(source.join("var/lib/systemd/linger/user"), "").unwrap();
        fs::write(source.join("var/lib/containers/storage/image"), "podman").unwrap();
        fs::write(source.join("var/lib/rpm/Packages"), "must stay out").unwrap();
        fs::write(source.join("var/rebase-test/marker"), "carry").unwrap();
        fs::write(source.join("var/cache/cached"), "must stay out").unwrap();
        fs::write(source.join("home/alice/marker"), "mount only").unwrap();
        fs::write(source.join("opt/bin/tool"), "opt").unwrap();
        fs::write(source.join("usr/local/bin/tool"), "local").unwrap();
        fs::create_dir_all(deploy.join("etc")).unwrap();
        fs::write(
            deploy.join("etc/fstab"),
            "UUID=old-home /home ext4 defaults 0 0\n",
        )
        .unwrap();
        let report = stage(
            &source,
            &target_var,
            &deploy,
            &HomeLayout::RootDirectory,
            false,
        )
        .unwrap();

        assert_eq!(report.home.copied_bytes, 0);
        assert_eq!(report.home.action, "bind-mount");
        assert!(target_var.join("lib/containers/storage/image").is_file());
        assert!(target_var.join("lib/systemd/linger/user").is_file());
        assert!(target_var.join("rebase-test/marker").is_file());
        assert!(target_var.join("opt/bin/tool").is_file());
        assert!(target_var.join("usrlocal/bin/tool").is_file());
        assert!(!target_var.join("lib/rpm/Packages").exists());
        assert!(!target_var.join("cache/cached").exists());
        assert!(!target_var.join("home/alice/marker").exists());
        let fstab = fs::read_to_string(deploy.join("etc/fstab")).unwrap();
        assert!(fstab.contains("/sysroot/home /var/home"));
        assert!(
            !fstab
                .lines()
                .any(|line| line.split_whitespace().nth(1) == Some("/home"))
        );
        assert!(report.total_bytes >= "podman".len() as u64 + 5);
        assert!(report.trees.iter().any(|tree| tree.source == "/opt"));
        assert!(report.trees.iter().any(|tree| tree.source == "/usr/local"));
    }
}
