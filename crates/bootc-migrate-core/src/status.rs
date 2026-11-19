//! `status`: what migration happened here, and what is left to do (L2).
//!
//! Staging routes record a small JSON report; [`gather`] combines it with
//! the L1 verify result and the live boot state into a [`StatusState`],
//! which [`render`] turns into CLI text. Gathering is I/O, rendering is
//! pure and table-tested.

use serde::Serialize;
use std::path::{Path, PathBuf};

/// Where CoreMigration and ImageSwap record what they staged (carried into
/// the new deployment with `/var`). OstreeInstall keeps its own richer
/// report; see below.
pub const MIGRATE_REPORT: &str = "/var/lib/bootc-migrate/report.json";
/// OstreeInstall's report, written pre-reboot and carried with `/var`.
pub const OSTREE_INSTALL_REPORT: &str = "/var/lib/bootc-rebase/ostree-install-report.json";

/// The route string [`gather`] reports for an OstreeInstall host.
pub const OSTREE_INSTALL_ROUTE: &str = "composefs -> ostree via OstreeInstall";
/// The route string [`gather`] reports for a PackageInstall host.
pub const PACKAGE_INSTALL_ROUTE: &str = "package -> ostree via PackageInstall";
/// PackageInstall's report, written pre-reboot and carried with `/var`.
pub const PACKAGE_INSTALL_REPORT: &str = "/var/lib/bootc-rebase/package-install-report.json";

/// One manual check an OstreeInstall host needs after its first boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Day2Check {
    /// Phrase the L2 prompt text must also contain (kept in sync by test).
    pub keyword: &'static str,
    /// The `status` line.
    pub text: &'static str,
}

/// Day-2 checks for OstreeInstall: the breakage the first real Dakota ->
/// Utah run (#313) found by hand, which the L1 probe does not cover.
pub const OSTREE_INSTALL_DAY2_CHECKS: &[Day2Check] = &[
    Day2Check {
        keyword: "/etc/fstab",
        text: "the ESP has an /etc/fstab entry (`findmnt --fstab /boot/efi`)",
    },
    Day2Check {
        keyword: "bootloader default",
        text: "the bootloader default is the OSTree entry, not the composefs rollback",
    },
    Day2Check {
        keyword: "SELinux",
        text: "service data under /var has the SELinux labels of the new policy \
               (`restorecon -Rnv /var`)",
    },
    Day2Check {
        keyword: "container storage",
        text: "container storage belongs to its users \
               (`~/.local/share/containers` for rootless containers)",
    },
];

/// What a staging route records about itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MigrationReport {
    /// e.g. "ostree -> composefs via CoreMigration".
    pub route: String,
    pub target_image: String,
    /// Unix epoch seconds when the deployment was staged.
    pub staged_at: u64,
}

/// Record a migration report. Called once per staging run.
pub fn write_report(path: &Path, route: &str, target_image: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let staged_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let report = MigrationReport {
        route: route.to_string(),
        target_image: target_image.to_string(),
        staged_at,
    };
    std::fs::write(path, serde_json::to_string_pretty(&report)?)?;
    Ok(())
}

/// The L1 verify probe's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verify {
    /// No result file: the probe never ran (route stages none, or this boot
    /// predates the migration).
    NotRun,
    Clean,
    Findings {
        count: u64,
        classes: Vec<String>,
    },
}

/// Whether `bootc-migrate commit` has work to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Commit {
    /// Booted a staged composefs deployment with legacy content still present.
    Available,
    /// Booted composefs and nothing legacy remains.
    Committed,
    /// Commit is meaningless here; the String says why.
    NotApplicable(String),
}

/// Everything `status` reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusState {
    pub route: Option<String>,
    pub target_image: Option<String>,
    pub source_distro: Option<String>,
    pub rollback_entry: Option<String>,
    pub software_report: Option<PathBuf>,
    pub software_summary: Option<String>,
    pub verify: Verify,
    pub commit: Commit,
}

/// Filesystem inputs to [`gather`], injectable for tests.
#[derive(Debug, Clone)]
pub struct StatusPaths {
    pub migrate_report: PathBuf,
    pub ostree_install_report: PathBuf,
    pub package_install_report: PathBuf,
    pub software_report: PathBuf,
    pub verify_result: PathBuf,
    pub verify_report: PathBuf,
    pub cmdline: PathBuf,
    pub ostree_dir: PathBuf,
    pub sysroot_usr: PathBuf,
    pub entries_dir: PathBuf,
}
impl StatusPaths {
    fn production() -> Self {
        Self {
            migrate_report: PathBuf::from(MIGRATE_REPORT),
            ostree_install_report: PathBuf::from(OSTREE_INSTALL_REPORT),
            package_install_report: PathBuf::from(PACKAGE_INSTALL_REPORT),
            software_report: PathBuf::from(crate::software_report::STATE_DIR)
                .join(crate::software_report::MARKDOWN_REPORT_FILE),
            verify_result: PathBuf::from(crate::firstboot_verify::VERIFY_RESULT),
            verify_report: PathBuf::from(crate::firstboot_verify::VERIFY_REPORT),
            cmdline: PathBuf::from("/proc/cmdline"),
            ostree_dir: PathBuf::from("/sysroot/ostree"),
            sysroot_usr: PathBuf::from("/sysroot/usr"),
            entries_dir: PathBuf::from("/boot/loader/entries"),
        }
    }
}

/// Read the live system state.
pub fn gather() -> StatusState {
    gather_from(&StatusPaths::production())
}

/// [`gather`] over explicit paths, so tests use fixtures.
pub fn gather_from(p: &StatusPaths) -> StatusState {
    let (route, target_image) = read_route(p);
    let source_distro = read_source_distro(p);
    let rollback_entry = read_rollback_entry(p);
    let (software_report, software_summary) = read_software_report(p);
    let verify = read_verify(p);
    let booted_composefs = std::fs::read_to_string(&p.cmdline)
        .map(|c| crate::composefs_karg::present(&c))
        .unwrap_or(false);
    let booted_ostree = std::fs::read_to_string(&p.cmdline)
        .map(|c| c.contains("ostree="))
        .unwrap_or(false);
    let legacy_present = legacy_ostree_content(&p.ostree_dir);
    let commit = match (
        &route,
        booted_composefs,
        booted_ostree,
        legacy_present,
        p.sysroot_usr.exists(),
    ) {
        (Some(r), true, _, true, _) if r.contains("composefs via CoreMigration") => {
            Commit::Available
        }
        (Some(r), true, _, false, _) if r.contains("composefs via CoreMigration") => {
            Commit::Committed
        }
        (Some(r), false, _, _, _) if r.contains("composefs via CoreMigration") => {
            Commit::NotApplicable(
                "booted the OSTree side; reboot into the composefs entry to commit".to_string(),
            )
        }
        (Some(r), _, true, _, true) if r == PACKAGE_INSTALL_ROUTE => Commit::Available,
        (Some(r), _, true, _, false) if r == PACKAGE_INSTALL_ROUTE => Commit::Committed,
        (Some(r), _, false, _, _) if r == PACKAGE_INSTALL_ROUTE => Commit::NotApplicable(
            "booted the rollback entry; boot the new deployment to commit".to_string(),
        ),
        (Some(r), _, _, _, _) if r == OSTREE_INSTALL_ROUTE => Commit::NotApplicable(
            "this route commits with `bootc-rebase commit`, which removes the composefs side"
                .to_string(),
        ),
        (Some(r), _, _, _, _) => Commit::NotApplicable(format!("route {r} has no commit step")),
        (None, _, _, _, _) => {
            Commit::NotApplicable("no migration has been staged here".to_string())
        }
    };
    StatusState {
        route,
        target_image,
        source_distro,
        rollback_entry,
        software_report,
        software_summary,
        verify,
        commit,
    }
}

/// Legacy OSTree content = anything under `/sysroot/ostree` besides the
/// target bootc installation's own container storage (`bootc/`), which
/// commit deliberately preserves.
fn legacy_ostree_content(ostree_dir: &Path) -> bool {
    let entries = match std::fs::read_dir(ostree_dir) {
        Ok(rd) => rd,
        Err(_) => return false,
    };
    entries.flatten().any(|e| {
        e.file_name()
            .to_str()
            .is_some_and(|n| n != "bootc" && !n.starts_with('.'))
    })
}

/// Route and target from whichever report exists. The migrate report wins
/// over the ostree-install one when both are present (a host migrated twice
/// reports its latest staging).
fn read_route(p: &StatusPaths) -> (Option<String>, Option<String>) {
    if let Ok(text) = std::fs::read_to_string(&p.migrate_report)
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
    {
        let route = v.get("route").and_then(|r| r.as_str()).map(str::to_string);
        let image = v
            .get("target_image")
            .and_then(|r| r.as_str())
            .map(str::to_string);
        if route.is_some() {
            return (route, image);
        }
    }
    if let Ok(text) = std::fs::read_to_string(&p.package_install_report)
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
    {
        let image = v
            .get("target_image")
            .and_then(|r| r.as_str())
            .map(str::to_string);
        return (Some(PACKAGE_INSTALL_ROUTE.to_string()), image);
    }
    if let Ok(text) = std::fs::read_to_string(&p.ostree_install_report)
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
    {
        let image = v
            .get("target_image")
            .and_then(|r| r.as_str())
            .map(str::to_string);
        return (Some(OSTREE_INSTALL_ROUTE.to_string()), image);
    }
    (None, None)
}
fn read_source_distro(p: &StatusPaths) -> Option<String> {
    if let Ok(text) = std::fs::read_to_string(&p.package_install_report)
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
        && let Some(s) = v.get("source_distro").and_then(|s| s.as_str())
    {
        return Some(s.to_string());
    }
    for path in ["/sysroot/etc/os-release", "/sysroot/usr/lib/os-release"] {
        if let Ok(content) = std::fs::read_to_string(path) {
            for line in content.lines() {
                if let Some(val) = line.strip_prefix("PRETTY_NAME=") {
                    return Some(val.trim_matches('"').to_string());
                }
            }
            for line in content.lines() {
                if let Some(val) = line.strip_prefix("NAME=") {
                    return Some(val.trim_matches('"').to_string());
                }
            }
        }
    }
    None
}

fn read_rollback_entry(p: &StatusPaths) -> Option<String> {
    if let Ok(text) = std::fs::read_to_string(&p.package_install_report)
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
        && let Some(r) = v.get("rollback_entry").and_then(|r| r.as_str())
    {
        return Some(r.to_string());
    }
    for dir in [
        &p.entries_dir,
        &PathBuf::from("/boot/loader/entries"),
        &PathBuf::from("/boot/efi/loader/entries"),
        &PathBuf::from("/efi/loader/entries"),
    ] {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "conf")
                    && let Ok(content) = std::fs::read_to_string(&path)
                {
                    for line in content.lines() {
                        if line.starts_with("title ")
                            && line.to_lowercase().contains("previous system")
                        {
                            return Some(line.trim_start_matches("title ").trim().to_string());
                        }
                    }
                    if content.to_lowercase().contains("previous system") {
                        return Some(
                            path.file_stem()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .to_string(),
                        );
                    }
                }
            }
        }
    }
    if let Ok(text) = std::fs::read_to_string(&p.package_install_report)
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
        && v.get("boot_snapshot_dir").is_some()
    {
        return Some("Previous system".to_string());
    }
    None
}

fn read_software_report(p: &StatusPaths) -> (Option<PathBuf>, Option<String>) {
    let candidate = if p.software_report.exists() {
        Some(p.software_report.clone())
    } else {
        let md = PathBuf::from(crate::software_report::STATE_DIR)
            .join(crate::software_report::MARKDOWN_REPORT_FILE);
        let json = PathBuf::from(crate::software_report::STATE_DIR)
            .join(crate::software_report::JSON_REPORT_FILE);
        if md.exists() {
            Some(md)
        } else if json.exists() {
            Some(json)
        } else {
            None
        }
    };
    let json_candidate = candidate.as_ref().map(|path| {
        if path.extension().is_some_and(|e| e == "json") {
            path.clone()
        } else {
            path.with_extension("json")
        }
    });
    let summary = json_candidate.and_then(|json_path| {
        let text = std::fs::read_to_string(&json_path).ok()?;
        let val: serde_json::Value = serde_json::from_str(&text).ok()?;
        let pkg_count = val
            .get("missing_packages")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let svc_count = val
            .get("missing_enabled_services")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        if pkg_count > 0 || svc_count > 0 {
            Some(format!(
                "{pkg_count} missing package(s), {svc_count} missing service(s)"
            ))
        } else {
            None
        }
    });
    (candidate, summary)
}

fn read_verify(p: &StatusPaths) -> Verify {
    let summary = std::fs::read_to_string(&p.verify_result)
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if summary == "OK" {
        return Verify::Clean;
    }
    if let Some(count) = summary.strip_prefix("FINDINGS ") {
        let count = count.trim().parse::<u64>().unwrap_or(0);
        return Verify::Findings {
            count,
            classes: verify_classes(p),
        };
    }
    Verify::NotRun
}

fn verify_classes(p: &StatusPaths) -> Vec<String> {
    let text = std::fs::read_to_string(&p.verify_report).unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
    v.get("findings")
        .and_then(|f| f.as_array())
        .map(|fs| {
            fs.iter()
                .filter_map(|f| f.get("class").and_then(|c| c.as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Render a [`StatusState`] as CLI text.
pub fn render(s: &StatusState) -> String {
    let mut out = String::from("Migration status\n");
    match (&s.route, &s.target_image) {
        (Some(r), Some(t)) => out.push_str(&format!("  Route: {r}\n  Target image: {t}\n")),
        (Some(r), None) => out.push_str(&format!("  Route: {r}\n")),
        (None, _) => out.push_str("  Route: no migration has been staged here\n"),
    }
    if let Some(distro) = &s.source_distro {
        out.push_str(&format!("  Migrated from: {distro}\n"));
    }
    if let Some(rollback) = &s.rollback_entry {
        out.push_str(&format!("  Rollback entry: {rollback}\n"));
    }
    if let Some(sw) = &s.software_report {
        out.push_str(&format!("  Software report: {}", sw.display()));
        if let Some(summary) = &s.software_summary {
            out.push_str(&format!(" ({summary})"));
        }
        out.push('\n');
    }
    match &s.verify {
        Verify::NotRun => out.push_str("  First-boot verify: not run yet\n"),
        Verify::Clean => out.push_str("  First-boot verify: OK\n"),
        Verify::Findings { count, classes } => {
            out.push_str(&format!("  First-boot verify: FINDINGS {count}"));
            if !classes.is_empty() {
                out.push_str(&format!(" ({})", classes.join(", ")));
            }
            out.push('\n');
            out.push_str(&format!(
                "  See {} for details\n",
                crate::firstboot_verify::VERIFY_REPORT
            ));
        }
    }
    match (&s.commit, s.route.as_deref()) {
        (Commit::Available, Some(r)) if r == PACKAGE_INSTALL_ROUTE => {
            out.push_str("  Commit: available — run `bootc-rebase commit` when ready\n");
        }
        (Commit::Available, _) => {
            out.push_str("  Commit: available — run `bootc-migrate commit`\n");
        }
        (Commit::Committed, _) => out.push_str("  Commit: done, nothing legacy remains\n"),
        (Commit::NotApplicable(why), _) => {
            out.push_str(&format!("  Commit: not applicable ({why})\n"));
        }
    }
    if s.route.as_deref() == Some(OSTREE_INSTALL_ROUTE) {
        out.push_str(&format!("  Report: {OSTREE_INSTALL_REPORT}\n"));
        out.push_str("  Day-2 checks (not covered by the first-boot verify probe):\n");
        for check in OSTREE_INSTALL_DAY2_CHECKS {
            out.push_str(&format!("    - {}\n", check.text));
        }
    } else if s.route.as_deref() == Some(PACKAGE_INSTALL_ROUTE) {
        out.push_str(&format!("  Report: {PACKAGE_INSTALL_REPORT}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn paths(dir: &Path) -> StatusPaths {
        StatusPaths {
            migrate_report: dir.join("report.json"),
            ostree_install_report: dir.join("ostree-install-report.json"),
            package_install_report: dir.join("package-install-report.json"),
            software_report: dir.join("software-report.md"),
            verify_result: dir.join("verify-result"),
            verify_report: dir.join("verify-report.json"),
            cmdline: dir.join("cmdline"),
            ostree_dir: dir.join("ostree"),
            sysroot_usr: dir.join("sysroot_usr"),
            entries_dir: dir.join("entries"),
        }
    }
    fn seed_report(path: &Path, route: &str, image: &str) {
        std::fs::write(
            path,
            format!("{{\"route\": \"{route}\", \"target_image\": \"{image}\", \"staged_at\": 1}}"),
        )
        .unwrap();
    }

    #[test]
    fn committed_composefs_migration() {
        let dir = tempdir().unwrap();
        let p = paths(dir.path());
        seed_report(
            &p.migrate_report,
            "ostree -> composefs via CoreMigration",
            "img:t",
        );
        std::fs::write(&p.verify_result, "OK\n").unwrap();
        std::fs::write(&p.cmdline, "composefs=abc root=/dev/x\n").unwrap();
        std::fs::create_dir_all(p.ostree_dir.join("bootc")).unwrap();

        let s = gather_from(&p);
        assert_eq!(
            s,
            StatusState {
                route: Some("ostree -> composefs via CoreMigration".to_string()),
                target_image: Some("img:t".to_string()),
                source_distro: None,
                rollback_entry: None,
                software_report: None,
                software_summary: None,
                verify: Verify::Clean,
                commit: Commit::Committed,
            }
        );
        let text = render(&s);
        assert!(text.contains("First-boot verify: OK"), "{text}");
        assert!(text.contains("Commit: done"), "{text}");
    }

    #[test]
    fn commit_offered_only_with_report_boot_and_legacy() {
        let dir = tempdir().unwrap();
        let p = paths(dir.path());
        seed_report(
            &p.migrate_report,
            "ostree -> composefs via CoreMigration",
            "img:t",
        );
        std::fs::write(&p.cmdline, "composefs=abc\n").unwrap();
        std::fs::create_dir_all(p.ostree_dir.join("deploy")).unwrap();

        let s = gather_from(&p);
        assert_eq!(s.commit, Commit::Available);
        assert!(render(&s).contains("Commit: available"), "{}", render(&s));

        // Same disk, booted the OSTree side: commit must not be offered.
        std::fs::write(&p.cmdline, "ostree=foo\n").unwrap();
        let s = gather_from(&p);
        assert!(matches!(s.commit, Commit::NotApplicable(_)), "{s:?}");

        // Same boot, no report: never offer a deleting operation without
        // evidence this tool staged the deployment.
        std::fs::write(&p.cmdline, "composefs=abc\n").unwrap();
        std::fs::remove_file(&p.migrate_report).unwrap();
        let s = gather_from(&p);
        assert!(matches!(s.commit, Commit::NotApplicable(_)), "{s:?}");
    }

    #[test]
    fn findings_list_their_classes() {
        let dir = tempdir().unwrap();
        let p = paths(dir.path());
        std::fs::write(&p.verify_result, "FINDINGS 2\n").unwrap();
        std::fs::write(
            &p.verify_report,
            r#"{"findings": [{"class": "home-entries", "detail": "x"}, {"class": "machine-id", "detail": "y"}]}"#,
        )
        .unwrap();

        let s = gather_from(&p);
        assert_eq!(
            s.verify,
            Verify::Findings {
                count: 2,
                classes: vec!["home-entries".to_string(), "machine-id".to_string()],
            }
        );
        let text = render(&s);
        assert!(
            text.contains("FINDINGS 2 (home-entries, machine-id)"),
            "{text}"
        );
    }

    #[test]
    fn ostree_install_route_reads_its_own_report() {
        let dir = tempdir().unwrap();
        let p = paths(dir.path());
        std::fs::write(
            &p.ostree_install_report,
            r#"{"target_image": "ghcr.io/x/utah:t", "deployment": "abc.0"}"#,
        )
        .unwrap();
        std::fs::write(&p.cmdline, "ostree=foo\n").unwrap();

        let s = gather_from(&p);
        assert_eq!(
            s.route.as_deref(),
            Some("composefs -> ostree via OstreeInstall")
        );
        assert_eq!(s.target_image.as_deref(), Some("ghcr.io/x/utah:t"));
        assert!(matches!(s.commit, Commit::NotApplicable(_)), "{s:?}");

        let text = render(&s);
        assert!(
            text.contains(&format!("Report: {OSTREE_INSTALL_REPORT}")),
            "{text}"
        );
        for check in OSTREE_INSTALL_DAY2_CHECKS {
            assert!(
                text.contains(check.text),
                "missing {:?}:\n{text}",
                check.keyword
            );
        }
    }

    #[test]
    fn day2_checks_render_only_for_ostree_install() {
        let base = StatusState {
            route: None,
            target_image: None,
            source_distro: None,
            rollback_entry: None,
            software_report: None,
            software_summary: None,
            verify: Verify::Clean,
            commit: Commit::NotApplicable("n/a".to_string()),
        };
        for (route, want) in [
            (None, false),
            (Some("ostree -> composefs via CoreMigration"), false),
            (Some("composefs -> composefs via ImageSwap"), false),
            (Some(OSTREE_INSTALL_ROUTE), true),
        ] {
            let s = StatusState {
                route: route.map(str::to_string),
                ..base.clone()
            };
            let text = render(&s);
            assert_eq!(text.contains("Day-2 checks"), want, "{route:?}:\n{text}");
            assert_eq!(
                text.contains(OSTREE_INSTALL_REPORT),
                want,
                "{route:?}:\n{text}"
            );
        }
    }

    #[test]
    fn package_install_route_reads_its_own_report_and_status() {
        let dir = tempdir().unwrap();
        let p = paths(dir.path());
        std::fs::write(
            &p.package_install_report,
            r#"{
                "target_image": "quay.io/fedora/fedora-bootc:43",
                "deployment": "fedora-bootc.0",
                "source_distro": "Fedora Linux 43 (Cloud Edition)",
                "rollback_entry": "Previous system (Fedora Linux 43 (Cloud Edition))"
            }"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("software-report.json"),
            r#"{"missing_packages": [{"name": "cowsay"}], "missing_enabled_services": []}"#,
        )
        .unwrap();
        std::fs::write(&p.software_report, "# Software report\n").unwrap();
        std::fs::write(&p.cmdline, "ostree=/ostree/boot.1/x/y/0 rw\n").unwrap();
        std::fs::create_dir_all(&p.sysroot_usr).unwrap();
        std::fs::write(&p.verify_result, "OK\n").unwrap();

        let s = gather_from(&p);
        assert_eq!(
            s.route.as_deref(),
            Some("package -> ostree via PackageInstall")
        );
        assert_eq!(
            s.target_image.as_deref(),
            Some("quay.io/fedora/fedora-bootc:43")
        );
        assert_eq!(
            s.source_distro.as_deref(),
            Some("Fedora Linux 43 (Cloud Edition)")
        );
        assert_eq!(
            s.rollback_entry.as_deref(),
            Some("Previous system (Fedora Linux 43 (Cloud Edition))")
        );
        assert_eq!(s.commit, Commit::Available);

        let text = render(&s);
        assert!(
            text.contains("Route: package -> ostree via PackageInstall"),
            "{text}"
        );
        assert!(
            text.contains("Migrated from: Fedora Linux 43 (Cloud Edition)"),
            "{text}"
        );
        assert!(text.contains("Rollback entry: Previous system"), "{text}");
        assert!(text.contains("Software report:"), "{text}");
        assert!(text.contains("1 missing package(s)"), "{text}");
        assert!(text.contains("First-boot verify: OK"), "{text}");
        assert!(
            text.contains("Commit: available — run `bootc-rebase commit` when ready"),
            "{text}"
        );
        assert!(
            text.contains(&format!("Report: {PACKAGE_INSTALL_REPORT}")),
            "{text}"
        );

        // After commit, sysroot_usr is gone
        std::fs::remove_dir(&p.sysroot_usr).unwrap();
        let s2 = gather_from(&p);
        assert_eq!(s2.commit, Commit::Committed);
        assert!(render(&s2).contains("Commit: done"), "{}", render(&s2));

        // Booted rollback entry without ostree karg
        std::fs::write(&p.cmdline, "root=UUID=abc rw\n").unwrap();
        let s3 = gather_from(&p);
        assert!(matches!(s3.commit, Commit::NotApplicable(_)), "{s3:?}");
    }
    #[test]
    fn untouched_system() {
        let dir = tempdir().unwrap();
        let p = paths(dir.path());
        let s = gather_from(&p);
        assert_eq!(s.route, None);
        assert_eq!(s.verify, Verify::NotRun);
        let text = render(&s);
        assert!(text.contains("no migration has been staged"), "{text}");
        assert!(text.contains("not run yet"), "{text}");
    }

    #[test]
    fn write_report_round_trips() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sub/report.json");
        super::write_report(&path, "r", "i").unwrap();
        let (route, image) = read_route(&StatusPaths {
            migrate_report: path,
            ..paths(dir.path())
        });
        assert_eq!(route.as_deref(), Some("r"));
        assert_eq!(image.as_deref(), Some("i"));
    }
}
