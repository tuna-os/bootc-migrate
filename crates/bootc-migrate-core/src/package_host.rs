//! Preflight for a package-managed source (issue #372, part of #370).
//!
//! A package-based install can have any disk layout, and
//! `bootc install to-existing-root` supports only some of them. Getting this
//! wrong leaves a machine that does not boot, so every check here answers
//! "ready", "warning" or "refused" before anything changes, and each refusal
//! names the fix or the issue that tracks support.
//!
//! [`PackageHostFacts`] is plain data; [`assess`] is pure, so the decision
//! table is tested without a machine. [`PackageHostFacts::gather`] reads the
//! live system.

use std::fmt::Write as _;
use std::path::Path;

use crate::source_host::{self, HostKind, PackageDb};

/// Root filesystems `bootc install to-existing-root` is used on
/// (docs/filesystem-support.md).
const SUPPORTED_ROOT_FS: &[&str] = &["btrfs", "xfs", "ext4"];

/// Processes that hold the package database while they run.
const PACKAGE_MANAGERS: &[&str] = &[
    "dnf",
    "dnf5",
    "dnf-3",
    "yum",
    "rpm",
    "packagekitd",
    "apt",
    "apt-get",
    "aptitude",
    "dpkg",
    "unattended-upgr",
    "pacman",
    "zypper",
];

/// Free space needed on the root filesystem, from the target image's
/// compressed size: the pulled image plus the deployment written beside the
/// old system, each about 2.5 × the compressed size, plus 5 GiB of margin.
pub fn required_free_bytes(compressed_image_bytes: u64) -> u64 {
    compressed_image_bytes.saturating_mul(5) + 5 * GIB
}

const GIB: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelinuxMode {
    Disabled,
    Permissive,
    Enforcing,
}

/// One mounted filesystem the checks look at.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MountFacts {
    pub fs_type: String,
    pub source: String,
    /// btrfs `subvol=` mount option, without the leading `/`.
    pub btrfs_subvol: Option<String>,
    /// The device stack under the mount has a dm-crypt (LUKS) layer.
    pub on_luks: bool,
    /// The device stack under the mount has an LVM logical volume.
    pub on_lvm: bool,
}

/// What the preflight knows about the host. Every field is plain data so
/// tests can build any machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageHostFacts {
    pub package_db: Option<PackageDb>,
    pub uefi: bool,
    pub root: Option<MountFacts>,
    /// `None`: `/home` is a directory on the root filesystem.
    pub home: Option<MountFacts>,
    /// `None`: `/boot` is a directory on the root filesystem.
    pub boot: Option<MountFacts>,
    pub root_read_only: bool,
    pub free_root_bytes: Option<u64>,
    /// Running package managers, as `name (pid N)`.
    pub package_managers_running: Vec<String>,
    /// pacman's lock file exists (pacman holds no process lock).
    pub pacman_lock: bool,
    pub selinux: SelinuxMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verdict {
    Ready,
    Warning,
    Refused,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: &'static str,
    pub verdict: Verdict,
    pub detail: String,
}

fn check(name: &'static str, verdict: Verdict, detail: impl Into<String>) -> Check {
    Check {
        name,
        verdict,
        detail: detail.into(),
    }
}

fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / GIB as f64)
}

/// Assess `facts` for the package → ostree install. `compressed_image_bytes`
/// is the target image's size in the registry, when known.
pub fn assess(facts: &PackageHostFacts, compressed_image_bytes: Option<u64>) -> Vec<Check> {
    use Verdict::*;
    let mut out = Vec::new();

    out.push(match facts.package_db {
        Some(db) => check("Package manager", Ready, format!("{db} database found")),
        None => check(
            "Package manager",
            Refused,
            "no rpm, dpkg or pacman database: this is not a package-managed system",
        ),
    });

    out.push(if facts.uefi {
        check("Boot mode", Ready, "UEFI")
    } else {
        check(
            "Boot mode",
            Refused,
            "legacy BIOS boot is not supported yet; UEFI comes first and BIOS follows \
             with its own E2E cell (#370)",
        )
    });

    let Some(root) = &facts.root else {
        out.push(check(
            "Root filesystem",
            Refused,
            "the root filesystem could not be read from /proc/self/mountinfo",
        ));
        return out;
    };
    out.push(if SUPPORTED_ROOT_FS.contains(&root.fs_type.as_str()) {
        check("Root filesystem", Ready, root.fs_type.clone())
    } else {
        check(
            "Root filesystem",
            Refused,
            format!(
                "{} is not supported; the root must be btrfs, xfs or ext4 \
                 (docs/filesystem-support.md)",
                root.fs_type
            ),
        )
    });

    // Encrypted and LVM roots need their own E2E cells first (#305 for LUKS).
    let stack = |m: &MountFacts| match (m.on_luks, m.on_lvm) {
        (true, true) => Some("LVM on LUKS"),
        (true, false) => Some("LUKS"),
        (false, true) => Some("LVM"),
        (false, false) => None,
    };
    out.push(match stack(root) {
        None => check("Root device", Ready, root.source.clone()),
        Some(kind) => check(
            "Root device",
            Refused,
            format!(
                "the root is on {kind} ({}); encrypted and LVM roots are not supported \
                 in the first slice (#305, #370)",
                root.source
            ),
        ),
    });

    if root.fs_type == "btrfs" {
        out.push(match &root.btrfs_subvol {
            Some(sv) if !sv.is_empty() => check(
                "btrfs subvolume",
                Ready,
                format!("root is subvolume `{sv}`"),
            ),
            _ => check(
                "btrfs subvolume",
                Refused,
                "the root is the top level of the btrfs filesystem, not a subvolume; \
                 the old and new systems cannot be kept apart (#370)",
            ),
        });
    }

    out.push(match &facts.boot {
        None if stack(root).is_some() => check(
            "/boot",
            Refused,
            "/boot is on the encrypted root; bootc needs a readable /boot (#305)",
        ),
        None => check("/boot", Ready, "a directory on the root filesystem"),
        Some(b) if stack(b).is_some() => check(
            "/boot",
            Refused,
            format!(
                "/boot is on {} ({}); it must be readable by the firmware's loader",
                stack(b).unwrap_or_default(),
                b.source
            ),
        ),
        Some(b) => check(
            "/boot",
            Ready,
            format!("separate {} filesystem ({})", b.fs_type, b.source),
        ),
    });

    out.push(match &facts.home {
        None => check(
            "/home",
            Ready,
            "a directory on the root filesystem; it moves to /var/home at commit",
        ),
        Some(h) if stack(h).is_some() => check(
            "/home",
            Refused,
            format!(
                "/home is on {} ({}); not supported in the first slice (#370)",
                stack(h).unwrap_or_default(),
                h.source
            ),
        ),
        Some(h) => match &h.btrfs_subvol {
            Some(sv) => check(
                "/home",
                Ready,
                format!("btrfs subvolume `{sv}`; mounted at /var/home after the migration"),
            ),
            None => check(
                "/home",
                Ready,
                format!(
                    "separate {} filesystem ({}); mounted at /var/home after the migration",
                    h.fs_type, h.source
                ),
            ),
        },
    });

    out.push(if facts.root_read_only {
        check(
            "Root writable",
            Refused,
            "the root filesystem is mounted read-only",
        )
    } else {
        check("Root writable", Ready, "read-write")
    });

    out.push(match (facts.free_root_bytes, compressed_image_bytes) {
        (Some(free), Some(img)) => {
            let need = required_free_bytes(img);
            if free >= need {
                check(
                    "Free space",
                    Ready,
                    format!("{} free, about {} needed", gib(free), gib(need)),
                )
            } else {
                check(
                    "Free space",
                    Refused,
                    format!(
                        "{} free on the root filesystem, about {} needed (the image is \
                         {} compressed; the deployment is written beside the old system). \
                         Free up space and run again.",
                        gib(free),
                        gib(need),
                        gib(img)
                    ),
                )
            }
        }
        (Some(free), None) => check(
            "Free space",
            Warning,
            format!(
                "{} free; the target image size is unknown, so the need is not checked",
                gib(free)
            ),
        ),
        (None, _) => check(
            "Free space",
            Warning,
            "the free space on the root filesystem could not be read",
        ),
    });

    let mut busy = facts.package_managers_running.clone();
    if facts.pacman_lock {
        busy.push("pacman lock /var/lib/pacman/db.lck".into());
    }
    out.push(if busy.is_empty() {
        check(
            "Package transaction",
            Ready,
            "no package manager is running",
        )
    } else {
        check(
            "Package transaction",
            Refused,
            format!(
                "a package manager is busy ({}); let it finish, then run again",
                busy.join(", ")
            ),
        )
    });

    out.push(match facts.selinux {
        SelinuxMode::Disabled => check(
            "SELinux",
            Warning,
            "disabled on this system; if the target enforces SELinux, every carried file \
             is relabelled on the first boot, which takes a while",
        ),
        SelinuxMode::Permissive => check("SELinux", Ready, "permissive"),
        SelinuxMode::Enforcing => check("SELinux", Ready, "enforcing"),
    });

    out
}

/// The worst verdict in `checks`.
pub fn decision(checks: &[Check]) -> Verdict {
    checks
        .iter()
        .map(|c| c.verdict)
        .max()
        .unwrap_or(Verdict::Ready)
}

pub fn render(checks: &[Check]) -> String {
    let mut s = String::from("=== Package host readiness ===\n");
    for c in checks {
        let tag = match c.verdict {
            Verdict::Ready => "ok",
            Verdict::Warning => "WARNING",
            Verdict::Refused => "REFUSED",
        };
        let _ = writeln!(s, "  [{tag}] {}: {}", c.name, c.detail);
    }
    s
}

/// One line of /proc/self/mountinfo, for the fields the checks use.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MountInfoLine {
    mount_point: String,
    options: String,
    fs_type: String,
    source: String,
    super_options: String,
}

/// mountinfo(5): `id parent maj:min root mount-point options [optional...] -
/// fstype source super-options`. Octal escapes (`\040`) are decoded.
fn parse_mountinfo(text: &str) -> Vec<MountInfoLine> {
    let unescape = |s: &str| {
        let mut out = String::new();
        let b = s.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'\\'
                && i + 3 < b.len()
                && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
            {
                let v = u8::from_str_radix(&s[i + 1..i + 4], 8).unwrap_or(b'?');
                out.push(v as char);
                i += 4;
            } else {
                out.push(b[i] as char);
                i += 1;
            }
        }
        out
    };
    text.lines()
        .filter_map(|line| {
            let (pre, post) = line.split_once(" - ")?;
            let pre: Vec<&str> = pre.split(' ').collect();
            let post: Vec<&str> = post.split(' ').collect();
            Some(MountInfoLine {
                mount_point: unescape(pre.get(4)?),
                options: pre.get(5)?.to_string(),
                fs_type: post.first()?.to_string(),
                source: unescape(post.get(1)?),
                super_options: post.get(2).unwrap_or(&"").to_string(),
            })
        })
        .collect()
}

/// The last mount at `point` (the visible one, when mounts are stacked).
fn mount_at<'a>(mounts: &'a [MountInfoLine], point: &str) -> Option<&'a MountInfoLine> {
    mounts.iter().rev().find(|m| m.mount_point == point)
}

fn btrfs_subvol(super_options: &str) -> Option<String> {
    super_options
        .split(',')
        .find_map(|o| o.strip_prefix("subvol="))
        .map(|s| s.trim_start_matches('/').to_string())
}

/// The device-mapper layers under block device `maj:min`, read from
/// `/sys/dev/block/<maj:min>/dm/uuid` and its `slaves`, recursively.
/// Returns (luks, lvm).
fn dm_stack(sys: &Path, majmin: &str, depth: u8) -> (bool, bool) {
    if depth > 8 {
        return (false, false);
    }
    let dev = sys.join("dev/block").join(majmin);
    let uuid = std::fs::read_to_string(dev.join("dm/uuid")).unwrap_or_default();
    let mut luks = uuid.starts_with("CRYPT-");
    let mut lvm = uuid.starts_with("LVM-");
    if let Ok(slaves) = std::fs::read_dir(dev.join("slaves")) {
        for slave in slaves.flatten() {
            if let Ok(mm) = std::fs::read_to_string(slave.path().join("dev")) {
                let (l, v) = dm_stack(sys, mm.trim(), depth + 1);
                luks |= l;
                lvm |= v;
            }
        }
    }
    (luks, lvm)
}

fn source_majmin(source: &str) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let rdev = std::fs::metadata(source).ok()?.rdev();
    Some(format!(
        "{}:{}",
        rustix::fs::major(rdev),
        rustix::fs::minor(rdev)
    ))
}

fn mount_facts(sys: &Path, m: &MountInfoLine) -> MountFacts {
    let (on_luks, on_lvm) = source_majmin(&m.source)
        .map(|mm| dm_stack(sys, &mm, 0))
        .unwrap_or_default();
    MountFacts {
        fs_type: m.fs_type.clone(),
        source: m.source.clone(),
        btrfs_subvol: (m.fs_type == "btrfs")
            .then(|| btrfs_subvol(&m.super_options))
            .flatten(),
        on_luks,
        on_lvm,
    }
}

/// `name (pid N)` for every running process whose comm is a package manager.
fn running_package_managers(proc_root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(proc_root) else {
        return Vec::new();
    };
    let mut found: Vec<(u32, String)> = entries
        .flatten()
        .filter_map(|e| {
            let pid: u32 = e.file_name().to_str()?.parse().ok()?;
            let comm = std::fs::read_to_string(e.path().join("comm")).ok()?;
            let comm = comm.trim().to_string();
            PACKAGE_MANAGERS
                .contains(&comm.as_str())
                .then_some((pid, comm))
        })
        .collect();
    found.sort();
    found
        .into_iter()
        .map(|(pid, comm)| format!("{comm} (pid {pid})"))
        .collect()
}

impl PackageHostFacts {
    /// Read the live system.
    pub fn gather() -> Self {
        let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
        let mounts = parse_mountinfo(&mountinfo);
        let sys = Path::new("/sys");
        let root_line = mount_at(&mounts, "/");
        // Only a separate filesystem counts; a btrfs subvolume of the root's
        // own filesystem mounted at /home is still a separate mount.
        let separate = |point: &str| mount_at(&mounts, point).map(|m| mount_facts(sys, m));
        let package_db = match source_host::detect_host_kind(Path::new("/")) {
            HostKind::Package(db) => Some(db),
            _ => None,
        };
        let selinux = match std::fs::read_to_string("/sys/fs/selinux/enforce") {
            Ok(v) if v.trim() == "1" => SelinuxMode::Enforcing,
            Ok(_) => SelinuxMode::Permissive,
            Err(_) => SelinuxMode::Disabled,
        };
        PackageHostFacts {
            package_db,
            uefi: Path::new("/sys/firmware/efi").exists(),
            root: root_line.map(|m| mount_facts(sys, m)),
            home: separate("/home"),
            boot: separate("/boot"),
            root_read_only: root_line.is_some_and(|m| m.options.split(',').any(|o| o == "ro")),
            free_root_bytes: crate::preflight::system_info::get_free_space("/").ok(),
            package_managers_running: running_package_managers(Path::new("/proc")),
            pacman_lock: Path::new("/var/lib/pacman/db.lck").exists(),
            selinux,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mount(fs: &str, source: &str) -> MountFacts {
        MountFacts {
            fs_type: fs.into(),
            source: source.into(),
            ..Default::default()
        }
    }

    /// Fedora Workstation's default: btrfs `root` and `home` subvolumes,
    /// ext4 /boot, UEFI.
    fn fedora() -> PackageHostFacts {
        PackageHostFacts {
            package_db: Some(PackageDb::Rpm),
            uefi: true,
            root: Some(MountFacts {
                btrfs_subvol: Some("root".into()),
                ..mount("btrfs", "/dev/nvme0n1p3")
            }),
            home: Some(MountFacts {
                btrfs_subvol: Some("home".into()),
                ..mount("btrfs", "/dev/nvme0n1p3")
            }),
            boot: Some(mount("ext4", "/dev/nvme0n1p2")),
            root_read_only: false,
            free_root_bytes: Some(200 * GIB),
            package_managers_running: vec![],
            pacman_lock: false,
            selinux: SelinuxMode::Enforcing,
        }
    }

    fn verdict_of(checks: &[Check], name: &str) -> Verdict {
        checks
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("no check {name}: {checks:?}"))
            .verdict
    }

    #[test]
    fn fedora_default_layout_is_ready() {
        let checks = assess(&fedora(), Some(4 * GIB));
        assert_eq!(decision(&checks), Verdict::Ready, "{}", render(&checks));
    }

    #[test]
    fn refusal_table() {
        type Mutate = fn(&mut PackageHostFacts);
        let cases: &[(&str, Mutate, &str, &str)] = &[
            ("bios", |f| f.uefi = false, "Boot mode", "#370"),
            (
                "no db",
                |f| f.package_db = None,
                "Package manager",
                "not a package-managed",
            ),
            (
                "zfs root",
                |f| f.root = Some(mount("zfs", "rpool/ROOT")),
                "Root filesystem",
                "btrfs, xfs or ext4",
            ),
            (
                "luks root",
                |f| {
                    f.root = Some(MountFacts {
                        on_luks: true,
                        ..mount("xfs", "/dev/mapper/luks-1")
                    });
                    f.home = None;
                },
                "Root device",
                "#305",
            ),
            (
                "lvm root",
                |f| {
                    f.root = Some(MountFacts {
                        on_lvm: true,
                        ..mount("ext4", "/dev/mapper/vg-root")
                    });
                    f.home = None;
                },
                "Root device",
                "LVM",
            ),
            (
                "luks root without separate /boot",
                |f| {
                    f.root = Some(MountFacts {
                        on_luks: true,
                        ..mount("ext4", "/dev/mapper/luks-1")
                    });
                    f.boot = None;
                },
                "/boot",
                "readable /boot",
            ),
            (
                "btrfs top-level root",
                |f| f.root.as_mut().unwrap().btrfs_subvol = Some(String::new()),
                "btrfs subvolume",
                "top level",
            ),
            (
                "/home on luks",
                |f| {
                    f.home = Some(MountFacts {
                        on_luks: true,
                        ..mount("ext4", "/dev/mapper/home")
                    })
                },
                "/home",
                "LUKS",
            ),
            (
                "read-only root",
                |f| f.root_read_only = true,
                "Root writable",
                "read-only",
            ),
            (
                "too little space",
                |f| f.free_root_bytes = Some(10 * GIB),
                "Free space",
                "Free up space",
            ),
            (
                "dnf running",
                |f| f.package_managers_running = vec!["dnf (pid 42)".into()],
                "Package transaction",
                "dnf (pid 42)",
            ),
            (
                "pacman lock",
                |f| f.pacman_lock = true,
                "Package transaction",
                "db.lck",
            ),
        ];
        for (label, mutate, name, needle) in cases {
            let mut f = fedora();
            mutate(&mut f);
            let checks = assess(&f, Some(4 * GIB));
            let c = checks
                .iter()
                .find(|c| c.name == *name)
                .unwrap_or_else(|| panic!("{label}: no check {name}"));
            assert_eq!(c.verdict, Verdict::Refused, "{label}: {}", render(&checks));
            assert!(
                c.detail.contains(needle),
                "{label}: {:?} lacks {needle:?}",
                c.detail
            );
            assert_eq!(decision(&checks), Verdict::Refused, "{label}");
        }
    }

    #[test]
    fn warnings_do_not_refuse() {
        let mut f = fedora();
        f.selinux = SelinuxMode::Disabled;
        let checks = assess(&f, None);
        assert_eq!(verdict_of(&checks, "SELinux"), Verdict::Warning);
        assert_eq!(verdict_of(&checks, "Free space"), Verdict::Warning);
        assert_eq!(decision(&checks), Verdict::Warning);
    }

    #[test]
    fn plain_layouts_are_ready() {
        // ext4 root with /home and /boot as directories (Ubuntu, Arch).
        let mut f = fedora();
        f.package_db = Some(PackageDb::Dpkg);
        f.root = Some(mount("ext4", "/dev/sda2"));
        f.home = None;
        f.boot = None;
        f.selinux = SelinuxMode::Permissive;
        let checks = assess(&f, Some(3 * GIB));
        assert_eq!(decision(&checks), Verdict::Ready, "{}", render(&checks));
        assert!(!checks.iter().any(|c| c.name == "btrfs subvolume"));
    }

    #[test]
    fn free_space_need_scales_with_the_image() {
        assert_eq!(required_free_bytes(4 * GIB), 25 * GIB);
        let mut f = fedora();
        f.free_root_bytes = Some(25 * GIB);
        assert_eq!(
            verdict_of(&assess(&f, Some(4 * GIB)), "Free space"),
            Verdict::Ready
        );
        f.free_root_bytes = Some(25 * GIB - 1);
        assert_eq!(
            verdict_of(&assess(&f, Some(4 * GIB)), "Free space"),
            Verdict::Refused
        );
    }

    #[test]
    fn mountinfo_parsing() {
        let text = "\
22 1 0:33 /root / rw,relatime shared:1 - btrfs /dev/nvme0n1p3 rw,seclabel,compress=zstd:1,subvol=/root
40 22 259:2 / /boot rw,relatime shared:2 - ext4 /dev/nvme0n1p2 rw,seclabel
41 22 0:33 /home /home rw,relatime shared:3 - btrfs /dev/nvme0n1p3 rw,subvol=/home
42 22 0:50 / /mnt/my\\040disk ro,relatime - vfat /dev/sdb1 ro
";
        let m = parse_mountinfo(text);
        assert_eq!(m.len(), 4);
        let root = mount_at(&m, "/").unwrap();
        assert_eq!(root.fs_type, "btrfs");
        assert_eq!(btrfs_subvol(&root.super_options).as_deref(), Some("root"));
        assert_eq!(mount_at(&m, "/boot").unwrap().source, "/dev/nvme0n1p2");
        assert_eq!(
            btrfs_subvol(&mount_at(&m, "/home").unwrap().super_options).as_deref(),
            Some("home")
        );
        let usb = mount_at(&m, "/mnt/my disk").unwrap();
        assert!(usb.options.split(',').any(|o| o == "ro"));
        assert!(mount_at(&m, "/var").is_none());
    }

    #[test]
    fn dm_stack_finds_lvm_on_luks() {
        let dir = tempfile::tempdir().unwrap();
        let sys = dir.path();
        let dev = |mm: &str, uuid: &str, slaves: &[(&str, &str)]| {
            let d = sys.join("dev/block").join(mm);
            std::fs::create_dir_all(d.join("dm")).unwrap();
            std::fs::write(d.join("dm/uuid"), uuid).unwrap();
            std::fs::create_dir_all(d.join("slaves")).unwrap();
            for (name, slave_mm) in slaves {
                let s = d.join("slaves").join(name);
                std::fs::create_dir_all(&s).unwrap();
                std::fs::write(s.join("dev"), format!("{slave_mm}\n")).unwrap();
            }
        };
        dev("253:0", "CRYPT-LUKS2-abc-luks-abc", &[]);
        dev("253:1", "LVM-xyz", &[("dm-0", "253:0")]);
        dev("253:2", "", &[]);
        assert_eq!(dm_stack(sys, "253:1", 0), (true, true));
        assert_eq!(dm_stack(sys, "253:0", 0), (true, false));
        assert_eq!(dm_stack(sys, "253:2", 0), (false, false));
        assert_eq!(dm_stack(sys, "8:1", 0), (false, false));
    }

    #[test]
    fn package_managers_are_found_by_comm() {
        let dir = tempfile::tempdir().unwrap();
        for (pid, comm) in [
            ("1", "systemd"),
            ("42", "dnf5"),
            ("7", "apt-get"),
            ("x", "dnf"),
        ] {
            std::fs::create_dir_all(dir.path().join(pid)).unwrap();
            std::fs::write(dir.path().join(pid).join("comm"), format!("{comm}\n")).unwrap();
        }
        assert_eq!(
            running_package_managers(dir.path()),
            vec!["apt-get (pid 7)".to_string(), "dnf5 (pid 42)".to_string()]
        );
    }
}
