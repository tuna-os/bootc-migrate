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
    pub verify: Verify,
    pub commit: Commit,
}

/// Filesystem inputs to [`gather`], injectable for tests.
#[derive(Debug, Clone)]
pub struct StatusPaths {
    pub migrate_report: PathBuf,
    pub ostree_install_report: PathBuf,
    pub verify_result: PathBuf,
    pub verify_report: PathBuf,
    pub cmdline: PathBuf,
    pub ostree_dir: PathBuf,
}

impl StatusPaths {
    fn production() -> Self {
        Self {
            migrate_report: PathBuf::from(MIGRATE_REPORT),
            ostree_install_report: PathBuf::from(OSTREE_INSTALL_REPORT),
            verify_result: PathBuf::from(crate::firstboot_verify::VERIFY_RESULT),
            verify_report: PathBuf::from(crate::firstboot_verify::VERIFY_REPORT),
            cmdline: PathBuf::from("/proc/cmdline"),
            ostree_dir: PathBuf::from("/sysroot/ostree"),
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
    let verify = read_verify(p);
    let booted_composefs = std::fs::read_to_string(&p.cmdline)
        .map(|c| c.contains("composefs="))
        .unwrap_or(false);
    let legacy_present = legacy_ostree_content(&p.ostree_dir);
    let commit = match (&route, booted_composefs, legacy_present) {
        // Commit deletes; only offer it when our own report says we staged
        // a composefs deployment, we booted it, and legacy content remains.
        (Some(r), true, true) if r.contains("composefs via CoreMigration") => Commit::Available,
        (Some(r), true, false) if r.contains("composefs via CoreMigration") => Commit::Committed,
        (Some(r), false, _) if r.contains("composefs via CoreMigration") => Commit::NotApplicable(
            "booted the OSTree side; reboot into the composefs entry to commit".to_string(),
        ),
        (Some(r), _, _) => Commit::NotApplicable(format!("route {r} has no commit step")),
        (None, _, _) => Commit::NotApplicable("no migration has been staged here".to_string()),
    };
    StatusState {
        route,
        target_image,
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
    if let Ok(text) = std::fs::read_to_string(&p.ostree_install_report)
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
    {
        let image = v
            .get("target_image")
            .and_then(|r| r.as_str())
            .map(str::to_string);
        return (
            Some("composefs -> ostree via OstreeInstall".to_string()),
            image,
        );
    }
    (None, None)
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
    match &s.commit {
        Commit::Available => out.push_str("  Commit: available — run `bootc-migrate commit`\n"),
        Commit::Committed => out.push_str("  Commit: done, nothing legacy remains\n"),
        Commit::NotApplicable(why) => out.push_str(&format!("  Commit: not applicable ({why})\n")),
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
            verify_result: dir.join("verify-result"),
            verify_report: dir.join("verify-report.json"),
            cmdline: dir.join("cmdline"),
            ostree_dir: dir.join("ostree"),
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
