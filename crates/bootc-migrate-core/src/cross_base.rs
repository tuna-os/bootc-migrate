//! Cross-base re-base orchestration (issue #67, #80): deciding whether a
//! re-base crosses base lineages, planning and applying the UID/GID remap,
//! and reconciling `/etc` paths whose vendor default moved between bases.
//!
//! The planners themselves live in [`crate::remap`] and [`crate::etc_conflict`];
//! this module owns the orchestration: registry scans, `ostree admin status`
//! deployment discovery, and the two post-`bootc switch` applications.

use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};

use crate::{etc_conflict, registry, remap, scan};

/// Scan `target_image`'s capabilities, retrying on transient registry
/// failures — early-boot E2E runs have raced the guest's own network coming
/// up (`bootc switch`'s own pull moments later succeeds against the same
/// registry). Returns `None` (with a printed warning) rather than an error
/// so callers can degrade to "unknown information, don't gate on it."
pub fn scan_target_capabilities_with_retries(
    target_image: &str,
    purpose: &str,
) -> Option<scan::Capabilities> {
    const SCAN_ATTEMPTS: u32 = 3;
    const SCAN_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);
    let mut last_err = None;
    for attempt in 1..=SCAN_ATTEMPTS {
        match scan::scan_target_image(target_image) {
            Ok(c) => return Some(c),
            Err(e) => {
                if attempt < SCAN_ATTEMPTS {
                    std::thread::sleep(SCAN_RETRY_DELAY);
                }
                last_err = Some(e);
            }
        }
    }
    eprintln!(
        "Warning: could not scan target image for {purpose} after {SCAN_ATTEMPTS} attempt(s) \
         ({}); proceeding without it.",
        last_err.expect("None is returned only after at least one failed attempt")
    );
    None
}

pub fn build_cross_base_plan(target_image: &str) -> Result<Option<remap::RemapPlan>> {
    let Some(host_base) = scan::read_host_base_info() else {
        return Ok(None);
    };

    let Some(caps) = scan_target_capabilities_with_retries(target_image, "cross-base identity")
    else {
        return Ok(None);
    };
    let Some(target_base) = caps.base else {
        return Ok(None);
    };
    if !scan::is_cross_base(&host_base, &target_base) {
        return Ok(None);
    }

    let source_passwd =
        remap::parse_passwd(&std::fs::read_to_string("/etc/passwd").unwrap_or_default());
    let source_group =
        remap::parse_group(&std::fs::read_to_string("/etc/group").unwrap_or_default());

    let scratch = tempfile::Builder::new()
        .prefix("bootc-rebase-remap-")
        .tempdir_in("/var/tmp")
        .context("failed to create scratch dir for target identity DBs")?;
    let target_passwd_path = scratch.path().join("passwd");
    let target_group_path = scratch.path().join("group");
    registry::extract_files_via_registry(
        target_image,
        &[
            (Path::new("etc/passwd"), target_passwd_path.as_path()),
            (Path::new("etc/group"), target_group_path.as_path()),
        ],
    )
    .context("failed to fetch target identity DBs over the registry")?;
    let target_passwd =
        remap::parse_passwd(&std::fs::read_to_string(&target_passwd_path).unwrap_or_default());
    let target_group =
        remap::parse_group(&std::fs::read_to_string(&target_group_path).unwrap_or_default());

    Ok(Some(remap::plan_remap(
        &source_passwd,
        &source_group,
        &target_passwd,
        &target_group,
    )))
}

/// Print the remap report and, unless `accept_cross_base` (or `force`) was
/// passed, refuse with the blast radius already visible. Returns the plan
/// so the caller can apply it after staging succeeds — `None` when this
/// re-base isn't cross-base at all.
pub fn gate_cross_base(
    target_image: &str,
    accept_cross_base: bool,
    force: bool,
) -> Result<Option<remap::RemapPlan>> {
    let Some(plan) = build_cross_base_plan(target_image)? else {
        return Ok(None);
    };
    println!("{}", remap::render_report(&plan));
    if !accept_cross_base && !force {
        bail!(
            "Cross-base re-base detected (host and target disagree on ID/ID_LIKE). \
             Re-run with --accept-cross-base to proceed with the remap above."
        );
    }
    Ok(Some(plan))
}

/// #80: print (read-only, before staging) any system accounts the target
/// image's `sysusers.d` declares that this host's live identity DB lacks —
/// `bootc switch`'s native `/etc` merge has no key-level reconciliation for
/// them (see `remap::missing_target_sysusers`'s doc for the confirmed
/// mechanism). Advisory only; never refuses the re-base. Degrades silently
/// (via `scan_target_capabilities_with_retries`'s own warning) when the
/// target can't be scanned — this is a courtesy heads-up, not a gate.
pub fn warn_identity_merge_gap(target_image: &str) {
    let Some(caps) =
        scan_target_capabilities_with_retries(target_image, "identity-DB compatibility")
    else {
        return;
    };
    let host_passwd =
        remap::parse_passwd(&std::fs::read_to_string("/etc/passwd").unwrap_or_default());
    let host_group = remap::parse_group(&std::fs::read_to_string("/etc/group").unwrap_or_default());
    let missing = remap::missing_target_sysusers(&caps.sysusers, &host_passwd, &host_group);
    if missing.is_empty() {
        return;
    }
    println!(
        "Note (#80): the target image expects system account(s) not present in this \
         host's live /etc/passwd or /etc/group: {}. `bootc switch`'s native /etc merge \
         does not reconcile identity databases (unlike this tool's own composefs-conversion \
         merge) — if a locally-modified /etc/passwd is kept verbatim across the switch, \
         these accounts won't be added, and any service depending on them (e.g. dbus \
         needing a `messagebus` user) may fail to start after reboot. This is informational; \
         the re-base is not blocked.",
        missing.join(", ")
    );
}

/// `ostree admin status` stdout, or an error naming why it could not be
/// obtained.
pub fn ostree_admin_status() -> Result<String> {
    let out = std::process::Command::new("ostree")
        .args(["admin", "status"])
        .output()
        .map_err(|e| anyhow::anyhow!("failed to execute ostree admin status: {e}"))?;
    if !out.status.success() {
        bail!(
            "ostree admin status failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The staged deployment's root directory under `/ostree/deploy/<stateroot>`,
/// found via `ostree admin status`: exactly two deployments exist right
/// after `bootc switch` stages a target (booted + staged), and the booted
/// one is marked with a leading `*` — so the other line is unambiguously the
/// staged deployment. Mirrors the parsing tests/run-e2e.sh's ostree-rebase
/// cell already relies on for its own post-merge fixture injection.
pub fn staged_deployment_root() -> Result<PathBuf> {
    parse_staged_deployment_root(&ostree_admin_status()?)
}

/// The booted deployment's root directory — the `*`-marked line. Needed by
/// the cross-base `/etc` policy, whose "what did the source image ship"
/// input is that deployment's `usr/etc`.
pub fn booted_deployment_root() -> Result<PathBuf> {
    parse_booted_deployment_root(&ostree_admin_status()?)
}

/// Build a deployment root from one `ostree admin status` deployment line
/// (`* dakota abc123.0`, or the same without the booted marker).
pub fn deployment_root_from_line(line: &str) -> Result<PathBuf> {
    let mut fields = line.split_whitespace().filter(|f| *f != "*");
    let stateroot = fields
        .next()
        .ok_or_else(|| anyhow::anyhow!("malformed ostree admin status line: {line}"))?;
    let checksum_serial = fields
        .next()
        .ok_or_else(|| anyhow::anyhow!("malformed ostree admin status line: {line}"))?;
    Ok(PathBuf::from("/ostree/deploy")
        .join(stateroot)
        .join("deploy")
        .join(checksum_serial))
}

/// Testable core of [`staged_deployment_root`]: find the non-booted
/// deployment line in `ostree admin status` output and build its path.
pub fn parse_staged_deployment_root(admin_status_stdout: &str) -> Result<PathBuf> {
    let deploy_line = admin_status_stdout
        .lines()
        .find(|l| !l.trim_start().starts_with('*') && !l.trim().is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("no staged (non-booted) deployment found in ostree admin status")
        })?;
    deployment_root_from_line(deploy_line)
}

/// Testable core of [`booted_deployment_root`].
pub fn parse_booted_deployment_root(admin_status_stdout: &str) -> Result<PathBuf> {
    let deploy_line = admin_status_stdout
        .lines()
        .find(|l| l.trim_start().starts_with('*'))
        .ok_or_else(|| anyhow::anyhow!("no booted deployment found in ostree admin status"))?;
    deployment_root_from_line(deploy_line)
}

/// Apply the cross-base remap plan (chown /var + preserved /etc in the
/// staged deployment to the target's ids) after `bootc switch` has staged
/// it. No-op when `plan` is empty (same-base re-base, or no accounts
/// diverged even though the bases differ).
/// File written to the staged deployment root recording the remap plan and
/// its outcome, so the JSON twin of the printed report survives the reboot.
const REMAP_REPORT_FILE: &str = "bootc-migrate-remap-report.json";

pub fn apply_cross_base_remap(staged_root: &Path, plan: &remap::RemapPlan) -> Result<()> {
    // Always write the JSON report so the plan is preserved across reboot,
    // even when no remapping was needed (empty plan → empty report).
    let report_path = staged_root.join(REMAP_REPORT_FILE);
    std::fs::write(&report_path, plan.to_json())
        .with_context(|| format!("failed to write remap report to {}", report_path.display()))?;

    if plan.is_empty() {
        return Ok(());
    }
    let changed = remap::apply_remap_plan(staged_root, plan)
        .context("failed to apply cross-base UID/GID remap")?;
    println!(
        "Cross-base remap applied: {changed} file(s)/dir(s) rechowned under {}",
        staged_root.display()
    );
    Ok(())
}

/// The user's live `/etc` — the middle input of the cross-base three-way
/// conflict check, and the same tree `bootc switch` merged from.
const LIVE_ETC: &str = "/etc";

/// #67 part 2: reconcile the `/etc` paths where the target image ships a
/// different default *and* this host had modified the source's. `bootc
/// switch`'s native merge keeps the local value for every one of them,
/// which is right within one base lineage and wrong across two — see
/// [`etc_conflict`]'s module docs for the seam and the policy.
///
/// Runs after [`apply_cross_base_remap`] on purpose: the defaults this
/// writes already carry the target image's numeric ownership, and a chown
/// pass running afterwards could mistake one of those ids for a stale
/// source id and renumber it a second time.
///
/// Degrades to a warning rather than an error when one of the three trees
/// is missing: the re-base is already staged and sound by this point, and
/// the only consequence of skipping is that the local value stays in place
/// — exactly the pre-#67 behavior.
pub fn apply_cross_base_etc_policy(staged_root: &Path) -> Result<()> {
    let booted_root = booted_deployment_root()
        .context("failed to locate the booted deployment for the cross-base /etc policy")?;
    let source_defaults = etc_conflict::vendor_etc_dir(&booted_root);
    let target_defaults = etc_conflict::vendor_etc_dir(staged_root);
    let staged_etc = staged_root.join("etc");
    let current = Path::new(LIVE_ETC);

    for (label, dir) in [
        ("source image /etc defaults", source_defaults.as_path()),
        ("target image /etc defaults", target_defaults.as_path()),
        ("staged /etc", staged_etc.as_path()),
    ] {
        if !dir.is_dir() {
            eprintln!(
                "Warning: {label} not found at {} — skipping the cross-base /etc conflict \
                 policy (#67). The staged deployment keeps `bootc switch`'s own merge result, \
                 so any target default this host had locally modified stays overridden by the \
                 local value.",
                dir.display()
            );
            return Ok(());
        }
    }

    let triples = etc_conflict::collect_etc_triples(&source_defaults, current, &target_defaults)
        .context("failed to read the three /etc trees for the cross-base conflict policy")?;
    let plan = etc_conflict::plan_etc_conflicts(&triples);
    print!("{}", etc_conflict::render_report(&plan));

    // Write the JSON twin so the conflict record survives reboot.
    let conflict_report_path = staged_root.join("bootc-migrate-etc-conflict-report.json");
    std::fs::write(&conflict_report_path, plan.to_json()).with_context(|| {
        format!(
            "failed to write /etc conflict report to {}",
            conflict_report_path.display()
        )
    })?;

    let rewritten =
        etc_conflict::apply_etc_conflict_plan(&staged_etc, current, &target_defaults, &plan)
            .context("failed to apply the cross-base /etc conflict policy")?;
    if rewritten > 0 {
        println!(
            "Cross-base /etc policy applied: {rewritten} path(s) under {} now hold the \
             target image's default; the displaced value is beside each one.",
            staged_etc.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn staged_deployment_root_picks_non_starred_line() {
        // Real `ostree admin status` output: the booted deployment is
        // prefixed with '*', the staged one is not.
        let status = "* dakota abc123.0\nbluefin def456.1\n";
        let root = parse_staged_deployment_root(status).unwrap();
        assert_eq!(
            root,
            PathBuf::from("/ostree/deploy/bluefin/deploy/def456.1")
        );
    }

    #[test]
    fn staged_deployment_root_errors_when_only_booted_present() {
        let only_booted = "* dakota abc123.0\n";
        assert!(parse_staged_deployment_root(only_booted).is_err());
    }

    #[test]
    fn staged_deployment_root_errors_on_malformed_line() {
        let malformed = "* dakota abc123.0\nonly-one-field\n";
        assert!(parse_staged_deployment_root(malformed).is_err());
    }

    #[test]
    fn booted_deployment_root_picks_the_starred_line() {
        // The staged deployment is listed first; the booted one carries the
        // '*' marker wherever it appears.
        let status = "  bluefin def456.1\n* dakota abc123.0\n";
        assert_eq!(
            parse_booted_deployment_root(status).unwrap(),
            PathBuf::from("/ostree/deploy/dakota/deploy/abc123.0")
        );
        assert_eq!(
            parse_staged_deployment_root(status).unwrap(),
            PathBuf::from("/ostree/deploy/bluefin/deploy/def456.1")
        );
    }

    #[test]
    fn booted_deployment_root_errors_when_nothing_is_booted() {
        assert!(parse_booted_deployment_root("  bluefin def456.1\n").is_err());
        assert!(parse_booted_deployment_root("* only-one-field\n").is_err());
    }
}
