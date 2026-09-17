//! Cross-family re-base on the composefs conversion route
//! (bootc-migrate#256): Fedora → openSUSE, and every other pair that shares
//! no base lineage at all.
//!
//! # Why the same-lineage merge is the wrong merge here
//!
//! Phase 4's 3-way `/etc` merge ([`crate::mergetc`]) keeps the user's copy
//! of every path they changed against the *source's* factory default. That
//! is right within one family: the edit was made against the same defaults
//! the target ships. Across families both sides move — the target's default
//! for `pam.d/`, `sysconfig/`, `default/`, `login.defs`, the package
//! manager's configuration and its repositories is a genuinely different
//! file — and "keep the user's copy" silently carries one family's `/etc`
//! onto the other. A user who did exactly that reported a system that
//! booted openSUSE with Fedora's `/etc` (no zypper repositories "and much
//! more").
//!
//! `bootc-rebase`'s OstreeDeploy route already has the #67 hardening
//! (remap, `/etc` conflict policy, autorelabel) for that shape of problem,
//! but only there; the composefs route had nothing, not even a warning.
//!
//! # The policy
//!
//! Gated: a cross-family target is refused unless `--accept-cross-base`
//! is passed, on the same tri-state terms as [`crate::cross_base`] (an
//! unscannable target refuses too — see bootc-migrate#191).
//!
//! Once accepted, the default is inverted for `/etc`:
//!
//! - every path the **target ships** takes the target's copy;
//! - every path only the **source's vendor** shipped is dropped;
//! - a small, explicit allowlist of *machine* state
//!   ([`CARRY_OVER_EXACT`], [`CARRY_OVER_PREFIXES`]) and every path only
//!   the **user** added are carried verbatim;
//! - identity databases are union-merged target-first (the target's
//!   numbering is what its services and `sysusers.d` expect; the source's
//!   extra accounts — every human user — are appended), and the password
//!   databases stay source-first so nobody is locked out;
//! - every displaced file the user had actually changed is preserved
//!   beside its replacement as a `.rebase-old` sidecar, the convention #15
//!   and #67 established — nothing is destroyed.
//!
//! Then two consequences of the identity merge are handled: file
//! ownership under `/var` and the staged `/etc` is renumbered to the
//! target's ids ([`crate::remap`], the same planner OstreeDeploy uses),
//! and when the target enforces an SELinux policy the source did not (or
//! a different type), a first-boot relabel is scheduled. A composefs root
//! is immutable, so `/.autorelabel` is not an option; a one-shot unit in
//! the staged `/etc` runs `restorecon` over the writable trees instead,
//! and — when `/var` lives on its own filesystem and could not be renumbered
//! before the reboot — the chown steps too.
//!
//! Planning is pure ([`plan_etc`], [`render_firstboot_unit`],
//! [`relabel_needed`]); the I/O is confined to the `apply_*` functions the
//! Phase 4 coordinator calls.

use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::fs;
use std::path::Path;

use crate::cross_base::scan_target_capabilities_with_retries;
use crate::mergetc::{EtcDriftManifest, EtcPathState, is_identity_db};
use crate::remap::{self, IdKind, RemapPlan, RemapStep};
use crate::scan::{self, BaseInfo, Lineage};
use crate::selinux::SelinuxConfig;

// ---- Gate -----------------------------------------------------------------

/// The identities on both sides of an accepted cross-family re-base.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CrossFamilyPlan {
    pub host: BaseInfo,
    pub target: BaseInfo,
}

/// What lineage planning could establish. Mirrors
/// [`crate::cross_base::CrossBaseVerdict`]: "we looked and they share a
/// family" is kept distinct from "we could not find out", because a gate
/// that cannot see must refuse rather than wave things through (#191).
#[derive(Debug)]
pub enum CrossFamilyVerdict {
    /// Scanned successfully; no lineage overlap.
    CrossFamily(Box<CrossFamilyPlan>),
    /// Scanned successfully; same base or same family — the same-lineage
    /// merge applies and nothing here is needed.
    SameFamily(Lineage),
    /// Identity could not be established on one side or the other.
    Unknown(&'static str),
}

impl CrossFamilyVerdict {
    /// The refusal this verdict warrants, or `None` to proceed. `accepted`
    /// is `--accept-cross-base || --force`.
    pub fn refusal(&self, accepted: bool) -> Option<String> {
        if accepted {
            return None;
        }
        match self {
            CrossFamilyVerdict::SameFamily(_) => None,
            CrossFamilyVerdict::Unknown(reason) => Some(format!(
                "Cannot determine whether this is a cross-family re-base: {reason}.\n\
                 \n\
                 This is not the same as having checked and found nothing. A target from \
                 another OS family needs the cross-family /etc policy (bootc-migrate#256), \
                 and it cannot be planned without the target's identity.\n\
                 \n\
                 Re-run with --accept-cross-base to proceed with the same-lineage merge \
                 (appropriate only when you already know the two images share a base), \
                 or restore access to the target image's registry and try again."
            )),
            CrossFamilyVerdict::CrossFamily(plan) => Some(cross_family_refusal(plan)),
        }
    }
}

fn describe(base: &BaseInfo) -> String {
    match &base.id_like {
        Some(like) if !like.trim().is_empty() => format!("{} (ID_LIKE=\"{}\")", base.id, like),
        _ => format!("{} (no ID_LIKE)", base.id),
    }
}

fn cross_family_refusal(plan: &CrossFamilyPlan) -> String {
    format!(
        "Cross-family re-base detected: this host is {host} and the target image is \
         {target}. They share no base lineage, so the same-lineage /etc merge would \
         carry this host's {host_id} configuration onto {target_id} — package-manager, \
         PAM, service and policy defaults included — and the migrated system would boot \
         with the wrong family's /etc (bootc-migrate#256).\n\
         \n\
         Re-run with --accept-cross-base to proceed with the cross-family policy: the \
         target's /etc defaults win, machine-specific state ({carry}) and everything you \
         added yourself are carried over, every displaced file you had changed is kept \
         beside its replacement as a .rebase-old sidecar, identity databases are merged \
         target-first, and file ownership under /var is renumbered to the target's \
         accounts. This route is exploratory: review ROADMAP.md \"Cross-family re-base\" \
         before relying on it.",
        host = describe(&plan.host),
        target = describe(&plan.target),
        host_id = plan.host.id,
        target_id = plan.target.id,
        carry = CARRY_OVER_EXACT.join(", "),
    )
}

/// Establish the lineage relation between this host and `target_image`.
pub fn build_verdict(target_image: &str) -> Result<CrossFamilyVerdict> {
    let Some(host) = scan::read_host_base_info() else {
        return Ok(CrossFamilyVerdict::Unknown(
            "this host's own /etc/os-release and /usr/lib/os-release are both unreadable",
        ));
    };
    let Some(caps) = scan_target_capabilities_with_retries(target_image, "cross-family identity")
    else {
        return Ok(CrossFamilyVerdict::Unknown(
            "the target image could not be scanned (see the registry warning above)",
        ));
    };
    let Some(target) = caps.base else {
        return Ok(CrossFamilyVerdict::Unknown(
            "the target image carries no readable os-release identity",
        ));
    };
    Ok(match scan::lineage(&host, &target) {
        Lineage::CrossFamily => {
            CrossFamilyVerdict::CrossFamily(Box::new(CrossFamilyPlan { host, target }))
        }
        same => CrossFamilyVerdict::SameFamily(same),
    })
}

/// The early gate the composefs routes run before anything is pulled or
/// staged: scan the target's identity over the registry, print the
/// verdict, and refuse a cross-family target unless accepted.
///
/// This is the fast-fail courtesy, not the decision. An unscannable
/// target is a warning here, not a refusal — the conversion route's Phase
/// 4 decides again from the *mounted* image's own `os-release`
/// ([`decide`]), which needs no registry, and refuses there if it must.
/// (The `bootc switch` routes have no such second look; for them the
/// warning is the whole answer, and `bootc switch` itself needs the
/// registry moments later anyway.) Making the scan a hard requirement
/// would turn a proxy or a registry blip into a refusal of every
/// same-family migration, which is the route's protected regression gate.
pub fn gate(target_image: &str, accepted: bool) -> Result<()> {
    let verdict = build_verdict(target_image)?;
    if let CrossFamilyVerdict::CrossFamily(_) = &verdict
        && let Some(message) = verdict.refusal(accepted)
    {
        bail!(message);
    }
    match verdict {
        CrossFamilyVerdict::CrossFamily(plan) => println!(
            "Cross-family re-base accepted: {} -> {}.",
            describe(&plan.host),
            describe(&plan.target)
        ),
        CrossFamilyVerdict::SameFamily(lineage) => {
            let what = match lineage {
                Lineage::SameBase => "the same base",
                _ => "the same base family",
            };
            println!("Base lineage: host and target are {what}; the standard /etc merge applies.");
        }
        CrossFamilyVerdict::Unknown(reason) => eprintln!(
            "Warning: base lineage unknown before the pull — {reason}. Phase 4 decides \
             from the pulled image's own os-release; a cross-family target is refused \
             there unless --accept-cross-base was given."
        ),
    }
    Ok(())
}

/// The authoritative decision, taken in Phase 4 from identities read off
/// the mounted target (and this host): the plan to apply, `None` for the
/// same-lineage merge, or the refusal. Pure so the policy is testable.
///
/// Missing identity on either side cannot be a refusal at this point —
/// there is nothing to compare — and cannot select the inverted policy
/// either (applying it to a same-family target would displace every user
/// edit into a sidecar for no reason). It warns and keeps the standard
/// merge, the pre-#256 behavior.
pub fn decide(
    host: Option<BaseInfo>,
    target: Option<BaseInfo>,
    accepted: bool,
) -> Result<Option<CrossFamilyPlan>> {
    let (Some(host), Some(target)) = (host, target) else {
        eprintln!(
            "Warning: could not read os-release on both sides of the migration; the \
             standard /etc merge applies and no cross-family policy can be planned."
        );
        return Ok(None);
    };
    match scan::lineage(&host, &target) {
        Lineage::CrossFamily => {
            let plan = CrossFamilyPlan { host, target };
            if !accepted {
                bail!(
                    "{}\n\nThe pulled image is cached, so re-running with --accept-cross-base \
                     skips straight to Phase 4.",
                    cross_family_refusal(&plan)
                );
            }
            Ok(Some(plan))
        }
        _ => Ok(None),
    }
}

// ---- /etc policy (pure) ---------------------------------------------------

/// `/etc`-relative paths that describe *this machine* rather than a
/// family's vendor policy, carried verbatim across families. The storage
/// and boot descriptors and the host identity are the same set
/// [`crate::etc_conflict`] holds exempt on the OstreeDeploy route; the
/// name-resolution and locale files are added because a target's factory
/// copy of them is a placeholder, never this machine's answer.
pub const CARRY_OVER_EXACT: &[&str] = &[
    "fstab",
    "crypttab",
    "mdadm.conf",
    "hostname",
    "hosts",
    "localtime",
    "locale.conf",
    "vconsole.conf",
    "adjtime",
    "machine-info",
];

/// Path prefixes carried verbatim for the same reason as
/// [`CARRY_OVER_EXACT`]: host keys (a changed fingerprint on every client),
/// the administrator's own sshd drop-ins, saved network connections, and
/// sudo grants.
pub const CARRY_OVER_PREFIXES: &[&str] = &[
    "ssh/ssh_host_",
    "ssh/sshd_config.d/",
    "NetworkManager/system-connections/",
    "systemd/network/",
    "sudoers.d/",
    "cryptsetup-keys.d/",
];

/// Whether `rel_path` is machine state the policy never replaces.
pub fn is_carried_over(rel_path: &str) -> bool {
    CARRY_OVER_EXACT.contains(&rel_path)
        || CARRY_OVER_PREFIXES.iter().any(|p| rel_path.starts_with(p))
}

/// The per-path outcome of the cross-family `/etc` policy, sorted by path
/// so the report and its JSON twin are stable.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct EtcCrossFamilyPlan {
    /// Paths the target ships: its copy replaces whatever this host had.
    /// Where the host's copy was user-modified it survives as a sidecar.
    pub take_target: Vec<String>,
    /// Paths only the source's vendor shipped: dropped (sidecar if the
    /// user had modified them; otherwise they were never this machine's).
    pub dropped: Vec<String>,
    /// Machine state and user-added paths, carried verbatim.
    pub carried: Vec<String>,
    /// Identity databases, union-merged target-first by [`crate::mergetc`].
    pub identity: Vec<String>,
    /// Paths that will get a `.rebase-old` sidecar: displaced *and*
    /// user-modified.
    pub sidecars: Vec<String>,
}

impl EtcCrossFamilyPlan {
    /// The override manifest that makes [`crate::mergetc`] apply this
    /// plan: every replaced or dropped path is forced to the target's
    /// default (absent, for a drop), which is exactly the `false` decision
    /// the Config Drift Review already defines — including its sidecar.
    /// Carried and identity paths carry no decision and take the default
    /// rule.
    pub fn overrides(&self) -> EtcDriftManifest {
        let decisions = self
            .take_target
            .iter()
            .chain(self.dropped.iter())
            .map(|p| (p.clone(), false))
            .collect();
        EtcDriftManifest { decisions }
    }

    /// The machine-readable twin of the printed report.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("EtcCrossFamilyPlan serialization cannot fail")
    }
}

/// Decide every path — see the module docs for the rule table. Pure.
pub fn plan_etc(states: &[EtcPathState]) -> EtcCrossFamilyPlan {
    let mut plan = EtcCrossFamilyPlan::default();
    for st in states {
        let path = st.path.clone();
        if is_identity_db(&path) {
            plan.identity.push(path);
        } else if is_carried_over(&path) {
            if st.in_current {
                plan.carried.push(path);
            }
        } else if st.in_target_default {
            if st.in_current && st.user_modified {
                plan.sidecars.push(path.clone());
            }
            plan.take_target.push(path);
        } else if st.in_source_default {
            if st.in_current && st.user_modified {
                plan.sidecars.push(path.clone());
            }
            plan.dropped.push(path);
        } else if st.in_current {
            plan.carried.push(path);
        }
    }
    for list in [
        &mut plan.take_target,
        &mut plan.dropped,
        &mut plan.carried,
        &mut plan.identity,
        &mut plan.sidecars,
    ] {
        list.sort();
        list.dedup();
    }
    plan
}

/// Render the end-of-phase summary.
pub fn render_etc_report(host: &BaseInfo, target: &BaseInfo, plan: &EtcCrossFamilyPlan) -> String {
    let mut out = String::new();
    out.push_str("=== Cross-family /etc policy report ===\n");
    out.push_str(&format!("{} -> {}\n", describe(host), describe(target)));
    out.push_str(&format!(
        "  {} path(s) take the target's default\n",
        plan.take_target.len()
    ));
    out.push_str(&format!(
        "  {} source-vendor path(s) dropped\n",
        plan.dropped.len()
    ));
    out.push_str(&format!(
        "  {} machine-state / user-added path(s) carried verbatim\n",
        plan.carried.len()
    ));
    out.push_str(&format!(
        "  {} identity database(s) union-merged target-first\n",
        plan.identity.len()
    ));
    if plan.sidecars.is_empty() {
        out.push_str("  no user-modified path displaced\n");
    } else {
        out.push_str(&format!(
            "  {} user-modified path(s) displaced, each kept as <path>{}:\n",
            plan.sidecars.len(),
            crate::mergetc::REBASE_OLD_SUFFIX
        ));
        for p in &plan.sidecars {
            out.push_str(&format!("    /etc/{p}\n"));
        }
    }
    out
}

// ---- SELinux (pure) -------------------------------------------------------

/// Whether the target's SELinux policy needs the writable trees relabeled
/// on first boot: the target enforces a policy type and either the source
/// had none (labels missing entirely) or a different type.
///
/// Deliberately broader than [`crate::selinux::policy_type_changed`], which
/// requires both sides to be configured: a Debian → Fedora host has no
/// `/etc/selinux/config` at all, and that is the case that needs the
/// relabel most.
pub fn relabel_needed(host: Option<&SelinuxConfig>, target: Option<&SelinuxConfig>) -> bool {
    let Some(target) = target else {
        return false;
    };
    let target_type = match target.selinux_type.as_deref() {
        Some(t) if !t.is_empty() => t,
        _ => return false,
    };
    if target.selinux.as_deref() == Some("disabled") {
        return false;
    }
    match host {
        None => true,
        Some(h) if h.selinux.as_deref() == Some("disabled") => true,
        Some(h) => h.selinux_type.as_deref() != Some(target_type),
    }
}

// ---- First-boot unit (pure rendering, I/O install) ------------------------

/// The one-shot unit's name in the staged `/etc/systemd/system`.
pub const FIRSTBOOT_UNIT: &str = "bootc-migrate-cross-family-firstboot.service";
/// The marker (relative to `/etc`) whose presence arms the unit; the unit
/// removes it as its last step, so it runs exactly once.
pub const FIRSTBOOT_MARKER: &str = "bootc-migrate/cross-family-firstboot";

/// The chown invocation for one remap step over `/var`. `--from` matches
/// the current numeric owner (`uid` or `:gid`), so one pass renumbers every
/// file the step names and nothing else; `-h` keeps symlinks themselves in
/// scope without following them.
fn chown_line(step: &RemapStep) -> String {
    match step.kind {
        IdKind::Uid => format!(
            "ExecStart=/usr/bin/chown -hR --from={} {} /var",
            step.from, step.to
        ),
        IdKind::Gid => format!(
            "ExecStart=/usr/bin/chown -hR --from=:{} :{} /var",
            step.from, step.to
        ),
    }
}

/// Render the first-boot unit, or `None` when there is nothing for it to do.
///
/// `var_steps` are applied to `/var` only: the staged `/etc` is always
/// renumbered before the reboot, and re-running cycle-safe steps over an
/// already-renumbered tree would renumber it a second time. `relabel` adds
/// `restorecon` over both writable trees; it is best-effort (`-`) because a
/// target without `policycoreutils` must still boot.
pub fn render_firstboot_unit(var_steps: &[RemapStep], relabel: bool) -> Option<String> {
    if var_steps.is_empty() && !relabel {
        return None;
    }
    let mut unit = String::new();
    unit.push_str("[Unit]\n");
    unit.push_str(
        "Description=bootc-migrate cross-family first boot (UID/GID remap, SELinux relabel)\n",
    );
    unit.push_str(&format!("ConditionPathExists=/etc/{FIRSTBOOT_MARKER}\n"));
    // Before any service can create files under the old numbering or read
    // a mislabeled path: right after the writable trees are mounted, ahead
    // of sysinit.
    unit.push_str("DefaultDependencies=no\n");
    unit.push_str("After=local-fs.target\n");
    unit.push_str("Before=sysinit.target\n");
    unit.push('\n');
    unit.push_str("[Service]\n");
    unit.push_str("Type=oneshot\n");
    unit.push_str("RemainAfterExit=no\n");
    for step in var_steps {
        unit.push_str(&chown_line(step));
        unit.push('\n');
    }
    if relabel {
        unit.push_str("ExecStart=-/usr/sbin/restorecon -RF /etc /var\n");
    }
    unit.push_str(&format!(
        "ExecStart=/usr/bin/rm -f /etc/{FIRSTBOOT_MARKER}\n"
    ));
    unit.push('\n');
    unit.push_str("[Install]\n");
    unit.push_str("WantedBy=sysinit.target\n");
    Some(unit)
}

/// Write the unit into the staged `/etc`, enable it, and arm its marker.
pub fn install_firstboot_unit(etc_dir: &Path, unit: &str) -> Result<()> {
    let unit_dir = etc_dir.join("systemd/system");
    fs::create_dir_all(&unit_dir)
        .with_context(|| format!("failed to create {}", unit_dir.display()))?;
    fs::write(unit_dir.join(FIRSTBOOT_UNIT), unit)
        .with_context(|| format!("failed to write {FIRSTBOOT_UNIT}"))?;
    let wants_dir = unit_dir.join("sysinit.target.wants");
    fs::create_dir_all(&wants_dir)?;
    let link = wants_dir.join(FIRSTBOOT_UNIT);
    if fs::symlink_metadata(&link).is_ok() {
        fs::remove_file(&link)?;
    }
    std::os::unix::fs::symlink(format!("../{FIRSTBOOT_UNIT}"), &link)
        .with_context(|| format!("failed to enable {FIRSTBOOT_UNIT}"))?;
    let marker = etc_dir.join(FIRSTBOOT_MARKER);
    if let Some(parent) = marker.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        &marker,
        b"armed by bootc-migrate; removed by the first-boot unit\n",
    )
    .with_context(|| format!("failed to write {}", marker.display()))?;
    Ok(())
}

// ---- Phase 4 application (I/O) --------------------------------------------

/// What the `/etc` transition learned from the mounted target while it had
/// it: the policy it applied, and the two inputs the post-merge steps need
/// after the mount is gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrossFamilyEtcOutcome {
    /// The identities the decision was taken on.
    pub plan: CrossFamilyPlan,
    pub etc_plan: EtcCrossFamilyPlan,
    pub target_passwd: String,
    pub target_group: String,
    pub target_selinux: Option<SelinuxConfig>,
}

/// Where the staged `/var` is, which decides whether it can be renumbered
/// before the reboot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StagedVar<'a> {
    /// Copied into the composefs stateroot: renumber it now.
    Copied(&'a Path),
    /// A dedicated filesystem left in place and still live under this
    /// running system: renumbering it now would break the services using
    /// it, so the first-boot unit does it.
    InPlace,
    /// Already staged and renumbered by an earlier run (a `--force`
    /// refresh): left alone, because cycle-safe chown steps applied a
    /// second time would renumber it back.
    PreviouslyStaged,
}

/// The record of everything the cross-family steps did, written beside
/// the deployment as `bootc-migrate-cross-family-report.json` so it
/// survives the reboot.
#[derive(Debug, Clone, Serialize)]
pub struct CrossFamilyReport {
    pub host: BaseInfo,
    pub target: BaseInfo,
    pub etc: EtcCrossFamilyPlan,
    pub remap: RemapPlan,
    /// Files and directories renumbered before the reboot.
    pub rechowned_now: usize,
    /// The `/var` renumbering was deferred to the first-boot unit.
    pub var_remap_deferred: bool,
    pub relabel_scheduled: bool,
    pub firstboot_unit_installed: bool,
}

/// The report's file name inside the deployment directory.
pub const REPORT_FILE: &str = "bootc-migrate-cross-family-report.json";

/// Everything the post-merge steps need, gathered by the Phase 4
/// coordinator.
#[derive(Debug)]
pub struct PostMergeInputs<'a> {
    pub etc_outcome: &'a CrossFamilyEtcOutcome,
    pub deploy_dir: &'a Path,
    pub etc_dir: &'a Path,
    pub staged_var: StagedVar<'a>,
    /// The host's live `/etc/selinux/config`, if any.
    pub host_selinux: Option<SelinuxConfig>,
    /// The host's live `/etc/passwd` and `/etc/group` — the source side of
    /// the remap.
    pub source_passwd: &'a str,
    pub source_group: &'a str,
}

/// Run the post-merge steps: plan and apply the UID/GID remap, decide the
/// relabel, install the first-boot unit when either is needed after the
/// reboot, and write the report.
pub fn apply_post_merge(inputs: &PostMergeInputs<'_>) -> Result<CrossFamilyReport> {
    let outcome = inputs.etc_outcome;
    let plan = &outcome.plan;
    print!(
        "{}",
        render_etc_report(&plan.host, &plan.target, &outcome.etc_plan)
    );

    let remap_plan = remap::plan_remap(
        &remap::parse_passwd(inputs.source_passwd),
        &remap::parse_group(inputs.source_group),
        &remap::parse_passwd(&outcome.target_passwd),
        &remap::parse_group(&outcome.target_group),
    );
    print!("{}", remap::render_report(&remap_plan));

    // The staged /etc is always ours to renumber now; /var only when it was
    // copied. Order matters: every step over every directory, then the
    // next step (see `apply_remap_plan_to_dirs`).
    let (dirs, var_remap_deferred): (Vec<&Path>, bool) = match inputs.staged_var {
        StagedVar::Copied(var) => (vec![inputs.etc_dir, var], false),
        StagedVar::InPlace => (vec![inputs.etc_dir], !remap_plan.steps.is_empty()),
        StagedVar::PreviouslyStaged => (vec![inputs.etc_dir], false),
    };
    let rechowned_now = remap::apply_remap_plan_to_dirs(&dirs, &remap_plan)
        .context("failed to apply the cross-family UID/GID remap")?;
    if rechowned_now > 0 {
        println!("[phase4] cross-family remap: {rechowned_now} file(s)/dir(s) renumbered");
    }
    if var_remap_deferred {
        println!(
            "[phase4] /var is a dedicated filesystem still in use by this system; its \
             renumbering runs from the first-boot unit instead"
        );
    }

    let relabel_scheduled = relabel_needed(
        inputs.host_selinux.as_ref(),
        outcome.target_selinux.as_ref(),
    );
    let var_steps: &[RemapStep] = if var_remap_deferred {
        &remap_plan.steps
    } else {
        &[]
    };
    let firstboot_unit_installed = match render_firstboot_unit(var_steps, relabel_scheduled) {
        Some(unit) => {
            install_firstboot_unit(inputs.etc_dir, &unit)
                .context("failed to install the cross-family first-boot unit")?;
            println!(
                "[phase4] installed {FIRSTBOOT_UNIT} (relabel: {relabel_scheduled}, deferred \
                 /var remap steps: {})",
                var_steps.len()
            );
            true
        }
        None => false,
    };

    let report = CrossFamilyReport {
        host: plan.host.clone(),
        target: plan.target.clone(),
        etc: outcome.etc_plan.clone(),
        remap: remap_plan,
        rechowned_now,
        var_remap_deferred,
        relabel_scheduled,
        firstboot_unit_installed,
    };
    let report_path = inputs.deploy_dir.join(REPORT_FILE);
    fs::write(
        &report_path,
        serde_json::to_string_pretty(&report).expect("CrossFamilyReport serialization cannot fail"),
    )
    .with_context(|| format!("failed to write {}", report_path.display()))?;
    println!(
        "[phase4] cross-family report written to {}",
        report_path.display()
    );
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(id: &str, like: Option<&str>) -> BaseInfo {
        BaseInfo {
            id: id.into(),
            id_like: like.map(Into::into),
            version_id: None,
        }
    }

    fn state(
        path: &str,
        in_source_default: bool,
        in_current: bool,
        in_target_default: bool,
        user_modified: bool,
    ) -> EtcPathState {
        EtcPathState {
            path: path.into(),
            in_source_default,
            in_current,
            in_target_default,
            user_modified,
        }
    }

    /// The whole policy in one table: each row is (path, presence in
    /// source/current/target, user-modified) → which list it lands in and
    /// whether a sidecar is written.
    #[test]
    fn etc_policy_table() {
        #[derive(Debug, PartialEq)]
        enum Bucket {
            Target,
            Dropped,
            Carried,
            Identity,
            Nowhere,
        }
        let cases: &[(&str, bool, bool, bool, bool, Bucket, bool)] = &[
            // Target ships it, user never touched the source copy: target's
            // copy, no sidecar.
            ("login.defs", true, true, true, false, Bucket::Target, false),
            // Target ships it, user edited the source copy: target's copy,
            // sidecar preserves the edit.
            (
                "default/useradd",
                true,
                true,
                true,
                true,
                Bucket::Target,
                true,
            ),
            // Target ships it, user deleted it: target wins (no sidecar —
            // there is nothing to preserve).
            (
                "pam.d/login",
                true,
                false,
                true,
                true,
                Bucket::Target,
                false,
            ),
            // User-added path the target happens to ship too.
            (
                "zypp/zypp.conf",
                false,
                true,
                true,
                true,
                Bucket::Target,
                true,
            ),
            // Source-vendor-only, untouched: dropped silently.
            (
                "dnf/dnf.conf",
                true,
                true,
                false,
                false,
                Bucket::Dropped,
                false,
            ),
            // Source-vendor-only, edited: dropped, sidecar keeps the edit.
            (
                "yum.repos.d/fedora.repo",
                true,
                true,
                false,
                true,
                Bucket::Dropped,
                true,
            ),
            // Source-vendor-only, already deleted by the user: dropped, nothing to keep.
            (
                "sysconfig/gone",
                true,
                false,
                false,
                true,
                Bucket::Dropped,
                false,
            ),
            // User-added, neither vendor ships it: carried.
            (
                "migration-test/marker.conf",
                false,
                true,
                false,
                true,
                Bucket::Carried,
                false,
            ),
            // Machine state, both ship it, user edited: carried, never replaced.
            ("hostname", true, true, true, true, Bucket::Carried, false),
            ("fstab", true, true, true, true, Bucket::Carried, false),
            ("hosts", true, true, true, true, Bucket::Carried, false),
            // Machine-state prefixes.
            (
                "ssh/ssh_host_ed25519_key",
                false,
                true,
                false,
                true,
                Bucket::Carried,
                false,
            ),
            (
                "sudoers.d/90-realuser",
                false,
                true,
                false,
                true,
                Bucket::Carried,
                false,
            ),
            (
                "NetworkManager/system-connections/wifi.nmconnection",
                false,
                true,
                false,
                true,
                Bucket::Carried,
                false,
            ),
            // Machine state the host lacks: nothing to carry, and not forced
            // either — the default rule decides.
            (
                "crypttab",
                false,
                false,
                true,
                false,
                Bucket::Nowhere,
                false,
            ),
            // Identity databases go to the union merge whatever their state.
            ("passwd", true, true, true, true, Bucket::Identity, false),
            ("shadow", true, true, true, true, Bucket::Identity, false),
            (
                "machine-id",
                true,
                true,
                true,
                true,
                Bucket::Identity,
                false,
            ),
            ("subuid", true, true, false, true, Bucket::Identity, false),
        ];
        let states: Vec<EtcPathState> = cases
            .iter()
            .map(|(p, s, c, t, m, _, _)| state(p, *s, *c, *t, *m))
            .collect();
        let plan = plan_etc(&states);
        for (p, _, _, _, _, bucket, sidecar) in cases {
            let path = p.to_string();
            let got = if plan.take_target.contains(&path) {
                Bucket::Target
            } else if plan.dropped.contains(&path) {
                Bucket::Dropped
            } else if plan.carried.contains(&path) {
                Bucket::Carried
            } else if plan.identity.contains(&path) {
                Bucket::Identity
            } else {
                Bucket::Nowhere
            };
            assert_eq!(&got, bucket, "bucket for {p}");
            assert_eq!(plan.sidecars.contains(&path), *sidecar, "sidecar for {p}");
        }
        // Every replaced or dropped path, and only those, becomes a forced
        // target-default decision for mergetc.
        let overrides = plan.overrides();
        for p in plan.take_target.iter().chain(plan.dropped.iter()) {
            assert_eq!(overrides.decisions.get(p), Some(&false), "{p}");
        }
        for p in plan.carried.iter().chain(plan.identity.iter()) {
            assert!(
                !overrides.decisions.contains_key(p),
                "{p} must not be overridden"
            );
        }
        assert_eq!(
            overrides.decisions.len(),
            plan.take_target.len() + plan.dropped.len()
        );
        // Sorted, so the report and JSON are stable.
        let mut sorted = plan.take_target.clone();
        sorted.sort();
        assert_eq!(plan.take_target, sorted);
    }

    #[test]
    fn refusal_policy_table() {
        let plan = CrossFamilyPlan {
            host: base("fedora", None),
            target: base("opensuse-tumbleweed", Some("opensuse suse")),
        };
        let cross = CrossFamilyVerdict::CrossFamily(Box::new(plan));
        let same = CrossFamilyVerdict::SameFamily(Lineage::SameFamily);
        let unknown = CrossFamilyVerdict::Unknown("the target image could not be scanned");

        // Not accepted: only a positive same-family finding proceeds.
        assert!(same.refusal(false).is_none());
        let cross_msg = cross.refusal(false).expect("cross-family must refuse");
        assert!(cross_msg.contains("Cross-family re-base detected"));
        assert!(cross_msg.contains("fedora"));
        assert!(cross_msg.contains("opensuse-tumbleweed"));
        assert!(cross_msg.contains("--accept-cross-base"));
        assert!(cross_msg.contains("#256"));
        let unknown_msg = unknown.refusal(false).expect("unknown must refuse (#191)");
        assert!(unknown_msg.contains("Cannot determine whether this is a cross-family re-base"));
        assert!(unknown_msg.contains("--accept-cross-base"));

        // Accepted: every verdict proceeds.
        for v in [&cross, &same, &unknown] {
            assert!(v.refusal(true).is_none(), "{v:?}");
        }
    }

    /// Phase 4's decision from the mounted identities: refuse cross-family
    /// unless accepted, plan when accepted, keep the standard merge for
    /// same-family and for anything it cannot compare.
    #[test]
    fn decide_table() {
        let fedora = || Some(base("bluefin", Some("fedora")));
        let dakota = || Some(base("dakota", Some("fedora")));
        let suse = || Some(base("opensuse-tumbleweed", Some("opensuse suse")));

        // Same family: no plan, accepted or not.
        assert!(decide(fedora(), dakota(), false).unwrap().is_none());
        assert!(decide(fedora(), dakota(), true).unwrap().is_none());
        // Cross-family, not accepted: refused, naming both sides.
        let err = decide(fedora(), suse(), false).unwrap_err().to_string();
        assert!(err.contains("Cross-family re-base detected"), "{err}");
        assert!(err.contains("opensuse-tumbleweed"));
        assert!(err.contains("--accept-cross-base"));
        // Cross-family, accepted: the plan carries both identities.
        let plan = decide(fedora(), suse(), true).unwrap().unwrap();
        assert_eq!(plan.host.id, "bluefin");
        assert_eq!(plan.target.id, "opensuse-tumbleweed");
        // Unreadable on either side: never a refusal, never a plan.
        assert!(decide(None, suse(), false).unwrap().is_none());
        assert!(decide(fedora(), None, true).unwrap().is_none());
    }

    #[test]
    fn relabel_needed_table() {
        let cfg = |mode: &str, ty: Option<&str>| SelinuxConfig {
            selinux: Some(mode.into()),
            selinux_type: ty.map(Into::into),
        };
        let cases = [
            // (host, target, expected)
            (None, None, false),
            // Target enforces, host had no SELinux at all: labels missing.
            (None, Some(cfg("enforcing", Some("targeted"))), true),
            // Same type on both sides: nothing to do.
            (
                Some(cfg("enforcing", Some("targeted"))),
                Some(cfg("enforcing", Some("targeted"))),
                false,
            ),
            // Type differs.
            (
                Some(cfg("enforcing", Some("targeted"))),
                Some(cfg("enforcing", Some("mls"))),
                true,
            ),
            // Host disabled, target enforcing: labels stale or absent.
            (
                Some(cfg("disabled", Some("targeted"))),
                Some(cfg("enforcing", Some("targeted"))),
                true,
            ),
            // Target disabled or typeless: never.
            (
                Some(cfg("enforcing", Some("targeted"))),
                Some(cfg("disabled", Some("targeted"))),
                false,
            ),
            (
                Some(cfg("enforcing", Some("targeted"))),
                Some(cfg("enforcing", None)),
                false,
            ),
            // Target present, host absent, permissive still needs labels.
            (None, Some(cfg("permissive", Some("targeted"))), true),
        ];
        for (host, target, want) in cases {
            assert_eq!(
                relabel_needed(host.as_ref(), target.as_ref()),
                want,
                "host={host:?} target={target:?}"
            );
        }
    }

    #[test]
    fn firstboot_unit_rendering() {
        // Nothing to do → no unit at all, so a same-numbering, same-policy
        // re-base leaves no trace in the staged /etc.
        assert!(render_firstboot_unit(&[], false).is_none());

        let steps = [
            RemapStep {
                kind: IdKind::Gid,
                from: 10,
                to: 60000,
            },
            RemapStep {
                kind: IdKind::Gid,
                from: 997,
                to: 10,
            },
            RemapStep {
                kind: IdKind::Uid,
                from: 81,
                to: 499,
            },
        ];
        let unit = render_firstboot_unit(&steps, true).unwrap();
        let lines: Vec<&str> = unit.lines().collect();
        // Armed by the marker, runs once, before services start.
        assert!(lines.contains(&format!("ConditionPathExists=/etc/{FIRSTBOOT_MARKER}").as_str()));
        assert!(lines.contains(&"Before=sysinit.target"));
        assert!(lines.contains(&"WantedBy=sysinit.target"));
        assert!(lines.contains(&"Type=oneshot"));
        // Steps in order, gid steps spelled with the leading colon, over /var
        // only (the staged /etc was renumbered before the reboot).
        let execs: Vec<&str> = lines
            .iter()
            .copied()
            .filter(|l| l.starts_with("ExecStart="))
            .collect();
        assert_eq!(
            execs,
            vec![
                "ExecStart=/usr/bin/chown -hR --from=:10 :60000 /var",
                "ExecStart=/usr/bin/chown -hR --from=:997 :10 /var",
                "ExecStart=/usr/bin/chown -hR --from=81 499 /var",
                "ExecStart=-/usr/sbin/restorecon -RF /etc /var",
                &format!("ExecStart=/usr/bin/rm -f /etc/{FIRSTBOOT_MARKER}"),
            ]
        );
        assert!(
            !unit.contains("/etc\n"),
            "chown must never touch /etc: {unit}"
        );

        // Relabel alone: no chown lines.
        let unit = render_firstboot_unit(&[], true).unwrap();
        assert!(!unit.contains("chown"));
        assert!(unit.contains("restorecon"));
        // Remap alone: no restorecon.
        let unit = render_firstboot_unit(&steps[..1], false).unwrap();
        assert!(unit.contains("chown"));
        assert!(!unit.contains("restorecon"));
    }

    #[test]
    fn firstboot_unit_installs_enabled_and_armed() {
        let tmp = tempfile::tempdir().unwrap();
        let etc = tmp.path().join("etc");
        let unit = render_firstboot_unit(&[], true).unwrap();
        install_firstboot_unit(&etc, &unit).unwrap();

        assert_eq!(
            fs::read_to_string(etc.join("systemd/system").join(FIRSTBOOT_UNIT)).unwrap(),
            unit
        );
        let link = etc
            .join("systemd/system/sysinit.target.wants")
            .join(FIRSTBOOT_UNIT);
        assert_eq!(
            fs::read_link(&link).unwrap().to_string_lossy(),
            format!("../{FIRSTBOOT_UNIT}")
        );
        assert!(etc.join(FIRSTBOOT_MARKER).is_file());

        // Re-installing (a --force refresh) replaces rather than fails.
        install_firstboot_unit(&etc, &unit).unwrap();
    }

    #[test]
    fn carry_over_lists_are_machine_state_only() {
        for p in ["fstab", "hostname", "hosts", "localtime"] {
            assert!(is_carried_over(p), "{p}");
        }
        for p in [
            "ssh/ssh_host_rsa_key.pub",
            "sudoers.d/wheel",
            "NetworkManager/system-connections/eth.nmconnection",
        ] {
            assert!(is_carried_over(p), "{p}");
        }
        for p in [
            "pam.d/login",
            "dnf/dnf.conf",
            "zypp/repos.d/oss.repo",
            "ssh/sshd_config",
            "selinux/config",
            "os-release",
        ] {
            assert!(!is_carried_over(p), "{p} must follow the target");
        }
    }

    /// The policy end to end over real trees: plan over the three
    /// directories, hand the plan to the merge as forced decisions, and
    /// check what lands in the staged /etc — exactly what Phase 4 does.
    #[test]
    fn policy_applied_through_the_merge() {
        use crate::mergetc::{IdentityMergePolicy, MergePolicy, REBASE_OLD_SUFFIX};
        let old = tempfile::tempdir().unwrap();
        let cur = tempfile::tempdir().unwrap();
        let new = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let w = |d: &tempfile::TempDir, p: &str, c: &str| {
            let full = d.path().join(p);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(full, c).unwrap();
        };
        // Fedora vendor + the user's live copy + openSUSE vendor.
        w(&old, "login.defs", "fedora-defaults\n");
        w(&cur, "login.defs", "fedora-defaults\n"); // untouched
        w(&new, "login.defs", "suse-defaults\n");
        w(&old, "default/useradd", "HOME=/home\n");
        w(&cur, "default/useradd", "HOME=/home\nSHELL=/bin/zsh\n"); // edited
        w(&new, "default/useradd", "HOME=/var/home\n");
        w(&old, "dnf/dnf.conf", "[main]\n");
        w(&cur, "dnf/dnf.conf", "[main]\nmax_parallel_downloads=20\n"); // edited, target lacks
        w(&old, "yum.repos.d/fedora.repo", "[fedora]\n");
        w(&cur, "yum.repos.d/fedora.repo", "[fedora]\n"); // untouched, target lacks
        w(&new, "zypp/repos.d/oss.repo", "[repo-oss]\n"); // target-only
        w(&old, "hostname", "fedora\n");
        w(&cur, "hostname", "mybox\n"); // machine state, both ship
        w(&new, "hostname", "localhost\n");
        w(&cur, "migration-test/marker.conf", "etc-state-value\n"); // user-added
        w(
            &cur,
            "sudoers.d/90-realuser",
            "realuser ALL=(ALL) NOPASSWD: ALL\n",
        );
        w(
            &old,
            "passwd",
            "root:x:0:0::/root:/bin/bash\nwheel:x:10:10::/:/sbin/nologin\n",
        );
        w(
            &cur,
            "passwd",
            "root:x:0:0::/root:/bin/bash\nwheel:x:10:10::/:/sbin/nologin\nrealuser:x:1000:1000::/var/home/realuser:/bin/bash\n",
        );
        w(
            &new,
            "passwd",
            "root:x:0:0::/root:/bin/bash\nwheel:x:997:997::/:/sbin/nologin\nchrony:x:499:499::/var/lib/chrony:/sbin/nologin\n",
        );
        w(&old, "shadow", "root:*:1::::::\n");
        w(
            &cur,
            "shadow",
            "root:$6$real:1::::::\nrealuser:$6$u:1::::::\n",
        );
        w(&new, "shadow", "root:*:1::::::\nchrony:!:1::::::\n");

        let states = crate::mergetc::etc_path_states(old.path(), cur.path(), new.path()).unwrap();
        let plan = plan_etc(&states);
        let overrides = plan.overrides();
        crate::mergetc::merge_etc_files_with_policy(
            old.path(),
            cur.path(),
            new.path(),
            out.path(),
            &MergePolicy {
                overrides: Some(&overrides),
                identity: IdentityMergePolicy::TargetFirst,
            },
        )
        .unwrap();

        let read = |p: &str| fs::read_to_string(out.path().join(p)).ok();
        let sidecar = |p: &str| read(&format!("{p}{REBASE_OLD_SUFFIX}"));
        // Target ships it: target's copy. Untouched → no sidecar; edited → sidecar.
        assert_eq!(read("login.defs").as_deref(), Some("suse-defaults\n"));
        assert_eq!(sidecar("login.defs"), None);
        assert_eq!(read("default/useradd").as_deref(), Some("HOME=/var/home\n"));
        assert_eq!(
            sidecar("default/useradd").as_deref(),
            Some("HOME=/home\nSHELL=/bin/zsh\n")
        );
        // Source-vendor-only: dropped; the edited one survives as a sidecar.
        assert_eq!(read("dnf/dnf.conf"), None);
        assert_eq!(
            sidecar("dnf/dnf.conf").as_deref(),
            Some("[main]\nmax_parallel_downloads=20\n")
        );
        assert_eq!(read("yum.repos.d/fedora.repo"), None);
        assert_eq!(sidecar("yum.repos.d/fedora.repo"), None);
        // Target-only: added.
        assert_eq!(
            read("zypp/repos.d/oss.repo").as_deref(),
            Some("[repo-oss]\n")
        );
        // Machine state and user additions: carried verbatim, no sidecar.
        assert_eq!(read("hostname").as_deref(), Some("mybox\n"));
        assert_eq!(sidecar("hostname"), None);
        assert_eq!(
            read("migration-test/marker.conf").as_deref(),
            Some("etc-state-value\n")
        );
        assert_eq!(
            read("sudoers.d/90-realuser").as_deref(),
            Some("realuser ALL=(ALL) NOPASSWD: ALL\n")
        );
        // Identity: target numbering wins, the human user is appended, root
        // keeps its real password hash.
        assert_eq!(
            read("passwd").as_deref(),
            Some(
                "root:x:0:0::/root:/bin/bash\nwheel:x:997:997::/:/sbin/nologin\nchrony:x:499:499::/var/lib/chrony:/sbin/nologin\nrealuser:x:1000:1000::/var/home/realuser:/bin/bash\n"
            )
        );
        assert_eq!(
            read("shadow").as_deref(),
            Some("root:$6$real:1::::::\nrealuser:$6$u:1::::::\nchrony:!:1::::::\n")
        );
        assert_eq!(sidecar("passwd"), None);

        // And the remap the identity merge implies: wheel 10 -> 997.
        let remap_plan = remap::plan_remap(
            &remap::parse_passwd(&fs::read_to_string(cur.path().join("passwd")).unwrap()),
            &remap::parse_group(""),
            &remap::parse_passwd(&fs::read_to_string(new.path().join("passwd")).unwrap()),
            &remap::parse_group(""),
        );
        assert_eq!(remap_plan.remaps.len(), 1);
        assert_eq!(remap_plan.remaps[0].name, "wheel");
        assert_eq!(
            (remap_plan.remaps[0].old_id, remap_plan.remaps[0].new_id),
            (10, 997)
        );
    }
}
