//! What `bootc-rebase scan` reports about the host it runs on, when that
//! host is a positively-detected package-managed source (issue #371): the
//! distribution, package manager and count, filesystem layout, boot mode
//! and SELinux state the `package -> ostree` migration will see.
//!
//! Purely informational — unlike [`crate::package_host::assess`], nothing
//! here passes or refuses a migration. [`assemble`] is pure, so the single
//! gate that matters (no section at all off a non-package host) is
//! table-tested without a machine; [`gather`] reads the live system.

use std::path::Path;

use serde::Serialize;

use crate::migration::os_release::read_os_release;
use crate::package_host::PackageHostFacts;
use crate::scan::{self, PkgFamily};

/// This host's distribution identity, from `/etc/os-release` plus the
/// package-manager family its `/usr/bin` reveals ([`scan::pkg_family_from_root`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HostDistro {
    pub id: String,
    pub version_id: String,
    pub pretty_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pkg_family: Option<PkgFamily>,
}

/// A snapshot of a package-managed host, for `scan`'s host section.
/// `facts` is flattened into the JSON object alongside `distro` and
/// `installed_package_count`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HostOverview {
    /// `None` when `/etc/os-release` could not be read — distro identity
    /// unknown, but still a positively-detected package host.
    pub distro: Option<HostDistro>,
    /// `None` when the caller has not computed one (e.g. an incomplete
    /// registry pass on the target side left the comparison short).
    pub installed_package_count: Option<usize>,
    #[serde(flatten)]
    pub facts: PackageHostFacts,
}

/// Combine already-gathered facts into the overview `scan`'s host section
/// reports. `None` when `facts` has no package database: an ostree or
/// composefs deployment reports through `bootc status` instead, and an
/// unknown root has nothing to show.
pub fn assemble(
    facts: &PackageHostFacts,
    distro: Option<HostDistro>,
    installed_package_count: Option<usize>,
) -> Option<HostOverview> {
    facts.package_db?;
    Some(HostOverview {
        distro,
        installed_package_count,
        facts: facts.clone(),
    })
}

/// Read the live system: this host's distro identity, package-manager
/// family, and [`PackageHostFacts`] (mounts, boot mode, SELinux).
///
/// `installed_package_count` comes from the caller rather than being read
/// here: `scan` already pays the cost of reading the package database once,
/// to build the software report (bootc-migrate#377); re-reading it a second
/// time just to count would double that work for no new information.
pub fn gather(installed_package_count: Option<usize>) -> Option<HostOverview> {
    let facts = PackageHostFacts::gather();
    let distro = read_os_release(Path::new("/")).ok().map(|os| HostDistro {
        id: os.id,
        version_id: os.version_id,
        pretty_name: os.pretty_name,
        pkg_family: scan::pkg_family_from_root(Path::new("/")),
    });
    assemble(&facts, distro, installed_package_count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package_host::SelinuxMode;
    use crate::source_host::PackageDb;

    fn facts(package_db: Option<PackageDb>) -> PackageHostFacts {
        PackageHostFacts {
            package_db,
            uefi: true,
            secure_boot: Some(true),
            root: None,
            home: None,
            boot: None,
            var: None,
            root_read_only: false,
            free_root_bytes: None,
            package_managers_running: Vec::new(),
            pacman_lock: false,
            selinux: SelinuxMode::Enforcing,
        }
    }

    #[test]
    fn no_section_without_a_positively_detected_package_host() {
        // An ostree/composefs/unknown root has no package_db: scan's host
        // section must not appear for it at all, not even as an
        // all-unknown shell — `bootc status` (or nothing) already covers
        // those roots.
        assert_eq!(assemble(&facts(None), None, None), None);
    }

    #[test]
    fn assembles_the_overview_for_a_package_host() {
        let distro = HostDistro {
            id: "fedora".into(),
            version_id: "41".into(),
            pretty_name: "Fedora Linux 41 (Workstation Edition)".into(),
            pkg_family: Some(PkgFamily::Dnf),
        };
        let overview = assemble(
            &facts(Some(PackageDb::Rpm)),
            Some(distro.clone()),
            Some(812),
        )
        .expect("a package host assembles an overview");
        assert_eq!(overview.distro, Some(distro));
        assert_eq!(overview.installed_package_count, Some(812));
        assert_eq!(overview.facts.package_db, Some(PackageDb::Rpm));
    }

    #[test]
    fn missing_distro_and_count_pass_through_as_none() {
        // /etc/os-release unreadable, or the caller has no count yet:
        // still a package host, reported with what is known.
        let overview = assemble(&facts(Some(PackageDb::Dpkg)), None, None)
            .expect("a package host assembles an overview even with gaps");
        assert_eq!(overview.distro, None);
        assert_eq!(overview.installed_package_count, None);
    }
}
