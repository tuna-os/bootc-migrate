//! What kind of system the migration starts from, read off its root
//! filesystem (issue #371).
//!
//! `bootc status` names the backend of a bootc deployment. On a host that is
//! not one (a regular Fedora, Ubuntu or Arch install) it reports nothing, or
//! is not installed at all. "Not bootc" alone must not be taken to mean "a
//! package-managed host": a broken bootc system and an unknown root both look
//! like that. So a package host is detected positively, by its package
//! database, and only when no ostree or composefs deployment state exists.

use std::path::Path;

use crate::rebase_plan::Backend;

/// The package database that manages a package host's root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageDb {
    Rpm,
    Dpkg,
    Pacman,
}

impl std::fmt::Display for PackageDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Rpm => "rpm",
            Self::Dpkg => "dpkg",
            Self::Pacman => "pacman",
        })
    }
}

/// The kind of root a migration starts from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostKind {
    /// An ostree deployment (`/run/ostree-booted` or `/sysroot/ostree/deploy`).
    Ostree,
    /// A native composefs deployment (`/sysroot/composefs`).
    Composefs,
    /// A writable root managed by a package manager, with no deployment state.
    Package(PackageDb),
    /// None of the above. Never migrated.
    Unknown,
}

impl HostKind {
    /// The source backend for the route table, or `None` for an unknown root.
    pub fn backend(self) -> Option<Backend> {
        match self {
            Self::Ostree => Some(Backend::Ostree),
            Self::Composefs => Some(Backend::Composefs),
            Self::Package(_) => Some(Backend::Package),
            Self::Unknown => None,
        }
    }
}

/// Paths whose presence means the root is a bootc (ostree or composefs)
/// deployment. Checked before any package database: an ostree image also
/// carries an rpm database under `/usr/lib/sysimage/rpm`.
const COMPOSEFS_MARKERS: &[&str] = &["sysroot/composefs"];
const OSTREE_MARKERS: &[&str] = &["run/ostree-booted", "sysroot/ostree/deploy"];

/// Package databases, in order. Each entry is the database and the paths
/// that hold it; one existing path is enough.
const PACKAGE_DBS: &[(PackageDb, &[&str])] = &[
    (PackageDb::Rpm, &["var/lib/rpm", "usr/lib/sysimage/rpm"]),
    (PackageDb::Dpkg, &["var/lib/dpkg/status"]),
    (PackageDb::Pacman, &["var/lib/pacman/local"]),
];

/// Classify the root at `root` (`/` on a live system). Pure apart from
/// reading directory entries, so tests run it on fixture roots.
pub fn detect_host_kind(root: &Path) -> HostKind {
    let exists = |p: &&str| std::fs::symlink_metadata(root.join(p)).is_ok();
    if COMPOSEFS_MARKERS.iter().any(exists) {
        return HostKind::Composefs;
    }
    if OSTREE_MARKERS.iter().any(exists) {
        return HostKind::Ostree;
    }
    PACKAGE_DBS
        .iter()
        .find(|(_, paths)| paths.iter().any(exists))
        .map_or(HostKind::Unknown, |(db, _)| HostKind::Package(*db))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(paths: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for p in paths {
            let path = dir.path().join(p);
            if p.ends_with("status") || p.ends_with("ostree-booted") {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, b"").unwrap();
            } else {
                std::fs::create_dir_all(path).unwrap();
            }
        }
        dir
    }

    #[test]
    fn detection_table() {
        let cases: &[(&[&str], HostKind)] = &[
            (&["var/lib/rpm"], HostKind::Package(PackageDb::Rpm)),
            (&["usr/lib/sysimage/rpm"], HostKind::Package(PackageDb::Rpm)),
            (&["var/lib/dpkg/status"], HostKind::Package(PackageDb::Dpkg)),
            (
                &["var/lib/pacman/local"],
                HostKind::Package(PackageDb::Pacman),
            ),
            // An ostree image carries an rpm database too: deployment state wins.
            (
                &["usr/lib/sysimage/rpm", "sysroot/ostree/deploy"],
                HostKind::Ostree,
            ),
            (
                &["usr/lib/sysimage/rpm", "run/ostree-booted"],
                HostKind::Ostree,
            ),
            (
                &["usr/lib/sysimage/rpm", "sysroot/composefs"],
                HostKind::Composefs,
            ),
            (&["usr/bin", "etc"], HostKind::Unknown),
            // dpkg without its status file is not a dpkg database.
            (&["var/lib/dpkg"], HostKind::Unknown),
        ];
        for (paths, want) in cases {
            let dir = fixture(paths);
            assert_eq!(detect_host_kind(dir.path()), *want, "{paths:?}");
        }
    }

    #[test]
    fn backend_of_each_kind() {
        assert_eq!(HostKind::Ostree.backend(), Some(Backend::Ostree));
        assert_eq!(HostKind::Composefs.backend(), Some(Backend::Composefs));
        assert_eq!(
            HostKind::Package(PackageDb::Dpkg).backend(),
            Some(Backend::Package)
        );
        assert_eq!(HostKind::Unknown.backend(), None);
    }
}
