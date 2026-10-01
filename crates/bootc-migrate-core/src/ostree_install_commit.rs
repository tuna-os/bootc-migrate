//! The commit step of `Strategy::OstreeInstall` (bootc-migrate#315):
//! verify, then remove, the composefs world a composefs → ostree re-base
//! left on disk.
//!
//! [`crate::ostree_install`] builds the OSTree deployment *beside* the
//! composefs root and keeps the composefs side as rollback. Once the OSTree
//! deployment has booted and rollback is no longer wanted, the old state is
//! dead weight — on the first real host, 560G under `/sysroot/state` and
//! `/sysroot/composefs`. This module is the guarded path to reclaim it:
//!
//! 1. **Preconditions.** Booted from OSTree (never from composefs — that is
//!    the tree this deletes); the booted deployment is the one the install
//!    report names; the ESP snapshot the route took is still on disk; the
//!    firmware still has the shim/GRUB entry that reaches the deployment.
//! 2. **Verification.** Every path of the old stateroot `/var` must exist in
//!    the live `/var` (presence + size, not ownership: a cross-family re-base
//!    remapped that on purpose). Paths missing outside
//!    [`VOLATILE_VAR_PATHS`] refuse the commit unless `--force`.
//! 3. **Forensics first.** The old `/var/lib/bootc-rebase` (install report,
//!    ESP snapshot) is mirrored into the live one without overwriting
//!    anything there.
//! 4. **Delete, in order:** old stateroot `/var`, composefs store (and its
//!    loopback image, when there is one), composefs deploy dirs. The OSTree
//!    repo, its deployments, `/boot`, the ESP and NVRAM are never touched.
//! 5. **Report** reclaimed bytes and what was kept and why.
//!
//! Everything below takes a [`Layout`] so the planning, the tree walks and
//! the deletion run against a temporary directory in the tests; only
//! [`execute_host`] (remount, efibootmgr) is host-only.

use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::migration::rollback;
use crate::ostree_install::{OstreeInstallReport, PHYSICAL_ROOT, REPORT_FILE, STATE_DIR};

/// The old composefs stateroot `/var`, relative to the physical root.
pub const OLD_STATEROOT_VAR: &str = "state/os/default/var";
/// The composefs deployments (one `<verity>` directory each, holding its
/// `/etc`), relative to the physical root.
pub const OLD_DEPLOY_DIR: &str = "state/deploy";
/// The composefs object store, relative to the physical root.
pub const COMPOSEFS_STORE: &str = "composefs";
/// The ext4 image a forward-migrated host loop-mounts at the store.
pub const COMPOSEFS_LOOPBACK: &str = "composefs-loopback.ext4";
/// Old-world directories removed once the targets inside them are gone, and
/// only when nothing else is left in them.
const PRUNE_IF_EMPTY: &[&str] = &["state/os/default", "state/os", "state/deploy", "state"];
/// Top-level names under the physical root that no commit target may be or
/// sit under: the live OSTree repo and deployments, and the boot trees.
const PROTECTED_ROOTS: &[&str] = &["ostree", "boot", "efi"];

/// Paths of the old `/var` (relative to it) that may be absent from the
/// live `/var` without blocking the commit. Each is either scratch space
/// that is valid to lose, written by the running composefs system after the
/// route copied `/var`, or (`lib/bootc-rebase`) mirrored by the forensics
/// step before anything is deleted.
pub const VOLATILE_VAR_PATHS: &[&str] = &[
    "tmp",
    "cache",
    "log/journal",
    "lib/systemd/coredump",
    "lib/bootc-rebase",
];

/// Where the commit record is written, beside the install report.
pub const COMMIT_RECORD_FILE: &str = "ostree-install-commit.json";

/// How many paths of each kind of difference are printed before the rest
/// is summarized as a count.
const DIFF_PRINT_LIMIT: usize = 40;

/// The physical root and the live `/var` the commit works between.
#[derive(Debug, Clone)]
pub struct Layout {
    pub sysroot: PathBuf,
    pub live_var: PathBuf,
}

impl Layout {
    /// The booted host: `/sysroot` and `/var`.
    pub fn host() -> Self {
        Self {
            sysroot: PathBuf::from(PHYSICAL_ROOT),
            live_var: PathBuf::from("/var"),
        }
    }

    pub fn old_var(&self) -> PathBuf {
        self.sysroot.join(OLD_STATEROOT_VAR)
    }

    /// Where the route keeps its state, inside the live `/var` or the old one.
    fn state_dir_in(var: &Path) -> PathBuf {
        var.join(var_relative(STATE_DIR).expect("STATE_DIR is under /var"))
    }
}

// ---- Pure helpers --------------------------------------------------------

/// `/var/lib/x` → `lib/x`; `None` for a path outside `/var`.
pub fn var_relative(abs: &str) -> Option<&str> {
    abs.strip_prefix("/var/").filter(|r| !r.is_empty())
}

/// The `ostree=` boot argument (`/ostree/boot.1/default/<csum>/0`), if the
/// system booted an OSTree deployment.
pub fn ostree_cmdline_path(cmdline: &str) -> Option<&str> {
    cmdline
        .split_whitespace()
        .find_map(|arg| arg.strip_prefix("ostree="))
        .filter(|p| !p.is_empty())
}

/// Whether the kernel command line names a composefs deployment.
pub fn booted_composefs(cmdline: &str) -> bool {
    cmdline
        .split_whitespace()
        .any(|arg| arg.starts_with("composefs="))
}

/// Whether a path of the old `/var` (relative to it) is covered by
/// [`VOLATILE_VAR_PATHS`].
pub fn is_volatile(rel: &str) -> bool {
    VOLATILE_VAR_PATHS
        .iter()
        .any(|v| rel == *v || rel.strip_prefix(v).is_some_and(|r| r.starts_with('/')))
}

/// Whether a path relative to the physical root may be deleted at all:
/// non-empty, no `..`/absolute components, and not the OSTree repo or a
/// boot tree. Checked again right before every delete.
pub fn is_deletable(rel: &str) -> bool {
    let p = Path::new(rel);
    let mut comps = p.components();
    let Some(std::path::Component::Normal(first)) = comps.next() else {
        return false;
    };
    if PROTECTED_ROOTS.iter().any(|r| first == *r) {
        return false;
    }
    comps.all(|c| matches!(c, std::path::Component::Normal(_)))
}

/// Mountpoints from `/proc/mounts` text, with the kernel's octal escapes
/// (`\040` for a space) decoded.
pub fn parse_mountpoints(proc_mounts: &str) -> Vec<(String, String)> {
    proc_mounts
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let _dev = fields.next()?;
            let mp = fields.next()?;
            let fstype = fields.next()?;
            Some((unescape_mount_field(mp), fstype.to_string()))
        })
        .collect()
}

fn unescape_mount_field(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && let Ok(code) = u8::from_str_radix(&s[i + 1..i + 4], 8)
        {
            out.push(code as char);
            i += 4;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// The mountpoints at or below `target`. A recursive delete would cross
/// into each of them, so any hit refuses the target.
pub fn mounts_within(target: &Path, mountpoints: &[(String, String)]) -> Vec<String> {
    mountpoints
        .iter()
        .map(|(mp, _)| mp)
        .filter(|mp| Path::new(mp).starts_with(target))
        .cloned()
        .collect()
}

/// Bytes as a human-readable binary size.
pub fn format_bytes(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.2} {}", UNITS[i])
    }
}

// ---- Tree walks ----------------------------------------------------------

/// What the old stateroot `/var` has that the live `/var` does not.
#[derive(Debug, Clone, Default, Serialize)]
pub struct VarDiff {
    /// Paths compared (old side).
    pub entries: u64,
    /// Missing live, outside [`VOLATILE_VAR_PATHS`]: these block the commit.
    pub missing: Vec<String>,
    /// Missing live, inside [`VOLATILE_VAR_PATHS`].
    pub volatile_missing: u64,
    /// Regular files present on both sides with different sizes.
    pub size_changed: Vec<String>,
    /// Present on both sides as different file types.
    pub type_changed: Vec<String>,
}

/// Walk the old `/var` and look up every path in the live one. A missing
/// directory is reported once, not once per child. Symlinks are compared,
/// never followed, and the walk does not leave the old tree's filesystem.
pub fn diff_var(old_var: &Path, live_var: &Path) -> Result<VarDiff> {
    let mut diff = VarDiff::default();
    if !old_var.is_dir() {
        return Ok(diff);
    }
    let root_dev = fs::symlink_metadata(old_var)?.dev();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel_dir) = stack.pop() {
        let dir = old_var.join(&rel_dir);
        let mut names: Vec<_> = fs::read_dir(&dir)
            .with_context(|| format!("reading {}", dir.display()))?
            .collect::<std::io::Result<Vec<_>>>()?;
        names.sort_by_key(|e| e.file_name());
        for entry in names {
            let rel = rel_dir.join(entry.file_name());
            let rel_str = rel.to_string_lossy().into_owned();
            let old_meta = entry.metadata()?;
            diff.entries += 1;
            let Ok(live_meta) = fs::symlink_metadata(live_var.join(&rel)) else {
                if is_volatile(&rel_str) {
                    diff.volatile_missing += 1;
                } else {
                    diff.missing.push(rel_str);
                }
                continue;
            };
            let (old_ty, live_ty) = (old_meta.file_type(), live_meta.file_type());
            if old_ty.is_dir() != live_ty.is_dir()
                || old_ty.is_symlink() != live_ty.is_symlink()
                || old_ty.is_file() != live_ty.is_file()
            {
                diff.type_changed.push(rel_str);
                continue;
            }
            if old_ty.is_file() && old_meta.len() != live_meta.len() {
                diff.size_changed.push(rel_str);
            } else if old_ty.is_dir() && old_meta.dev() == root_dev {
                stack.push(rel);
            }
        }
    }
    diff.missing.sort();
    diff.size_changed.sort();
    diff.type_changed.sort();
    Ok(diff)
}

/// Allocated bytes under `path` (what deleting it frees): hard links are
/// counted once, symlinks are not followed, other filesystems are skipped.
pub fn tree_usage(path: &Path) -> Result<u64> {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return Ok(0);
    };
    let root_dev = meta.dev();
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    let mut total = 0u64;
    let mut stack = vec![(path.to_path_buf(), meta)];
    while let Some((p, m)) = stack.pop() {
        if m.dev() != root_dev {
            continue;
        }
        if m.nlink() > 1 && !m.is_dir() && !seen.insert((m.dev(), m.ino())) {
            continue;
        }
        total += m.blocks() * 512;
        if m.is_dir() {
            for entry in fs::read_dir(&p).with_context(|| format!("reading {}", p.display()))? {
                let entry = entry?;
                stack.push((entry.path(), entry.metadata()?));
            }
        }
    }
    Ok(total)
}

// ---- Plan ----------------------------------------------------------------

/// One precondition and its outcome.
#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    /// A failed hard check refuses the commit even with `--force`.
    pub hard: bool,
    pub detail: String,
}

/// One path the commit deletes.
#[derive(Debug, Clone, Serialize)]
pub struct Target {
    /// Relative to the physical root.
    pub rel: String,
    pub path: PathBuf,
    pub bytes: u64,
    pub what: &'static str,
}

/// Everything the commit found, before it changes anything.
#[derive(Debug, Clone, Serialize)]
pub struct CommitPlan {
    pub checks: Vec<Check>,
    pub report_path: Option<PathBuf>,
    pub diff: VarDiff,
    pub targets: Vec<Target>,
    pub kept: Vec<(String, String)>,
}

/// Host facts the plan reads but cannot see through a [`Layout`].
#[derive(Debug, Clone, Default)]
pub struct HostFacts {
    pub cmdline: String,
    /// `efibootmgr -v` output; `None` when it could not be run.
    pub efibootmgr: Option<String>,
    pub proc_mounts: String,
}

impl HostFacts {
    pub fn gather() -> Self {
        let efibootmgr = Command::new("efibootmgr")
            .arg("-v")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
        Self {
            cmdline: fs::read_to_string("/proc/cmdline").unwrap_or_default(),
            efibootmgr,
            proc_mounts: fs::read_to_string("/proc/mounts").unwrap_or_default(),
        }
    }
}

/// Find the install report: the live `/var` first, then the old one (the
/// route writes it after copying `/var`, so until the forensics step runs
/// it usually lives only on the old side).
pub fn find_report(layout: &Layout) -> Option<(PathBuf, OstreeInstallReport)> {
    [&layout.live_var, &layout.old_var()]
        .into_iter()
        .map(|var| Layout::state_dir_in(var).join(REPORT_FILE))
        .find_map(|p| {
            let text = fs::read_to_string(&p).ok()?;
            let report = serde_json::from_str(&text).ok()?;
            Some((p, report))
        })
}

/// The deployment directory name the `ostree=` argument resolves to.
fn booted_deployment(layout: &Layout, cmdline: &str) -> Option<String> {
    let arg = ostree_cmdline_path(cmdline)?;
    let resolved = fs::canonicalize(layout.sysroot.join(arg.trim_start_matches('/'))).ok()?;
    Some(resolved.file_name()?.to_string_lossy().into_owned())
}

fn check(name: &'static str, ok: bool, hard: bool, detail: String) -> Check {
    Check {
        name,
        ok,
        hard,
        detail,
    }
}

/// Inspect the host and plan the commit. Read-only.
pub fn plan(layout: &Layout, facts: &HostFacts) -> Result<CommitPlan> {
    let mut checks = Vec::new();

    let composefs = booted_composefs(&facts.cmdline);
    let ostree = ostree_cmdline_path(&facts.cmdline).is_some();
    checks.push(check(
        "booted from OSTree",
        ostree && !composefs,
        true,
        if composefs {
            "the running system booted from composefs — the tree this commit deletes; \
             reboot into the OSTree deployment first"
                .into()
        } else if ostree {
            "the kernel command line has ostree= and no composefs=".into()
        } else {
            "the kernel command line has no ostree= argument".into()
        },
    ));

    let report = find_report(layout);
    checks.push(check(
        "install report",
        report.is_some(),
        false,
        match &report {
            Some((p, _)) => format!("{}", p.display()),
            None => format!(
                "no {REPORT_FILE} under {} in the live or the old /var; this host may not have \
                 been re-based by `bootc-rebase --target-backend ostree`",
                STATE_DIR
            ),
        },
    ));

    if let Some((_, r)) = &report {
        let booted = booted_deployment(layout, &facts.cmdline);
        checks.push(check(
            "booted deployment matches the report",
            booted.as_deref() == Some(r.deployment.as_str()),
            false,
            format!(
                "booted {}, report names {}",
                booted.as_deref().unwrap_or("<unresolved>"),
                r.deployment
            ),
        ));

        let snapshot = var_relative(&r.esp_snapshot_dir).and_then(|rel| {
            [&layout.live_var, &layout.old_var()]
                .into_iter()
                .map(|v| v.join(rel))
                .find(|p| p.is_dir())
        });
        checks.push(check(
            "ESP snapshot present",
            snapshot.is_some(),
            false,
            match &snapshot {
                Some(p) => format!("{}", p.display()),
                None => format!(
                    "{} not found in the live or the old /var",
                    r.esp_snapshot_dir
                ),
            },
        ));
    }

    let grub = facts
        .efibootmgr
        .as_deref()
        .and_then(rollback::parse_ostree_boot_entry_id);
    checks.push(check(
        "firmware reaches the OSTree deployment",
        grub.is_some(),
        false,
        match (&facts.efibootmgr, &grub) {
            (None, _) => "efibootmgr -v could not be run".into(),
            (Some(_), None) => "no shim/GRUB entry in NVRAM".into(),
            (Some(_), Some(id)) => format!("shim/GRUB entry Boot{id}"),
        },
    ));

    let old_var = layout.old_var();
    let diff = diff_var(&old_var, &layout.live_var)?;
    checks.push(check(
        "old /var is carried into the live /var",
        diff.missing.is_empty(),
        false,
        if diff.missing.is_empty() {
            format!(
                "{} path(s) compared, none missing ({} volatile path(s) skipped)",
                diff.entries, diff.volatile_missing
            )
        } else {
            format!(
                "{} path(s) of the old /var are missing from the live /var",
                diff.missing.len()
            )
        },
    ));

    if let (Ok(a), Ok(b)) = (fs::metadata(&old_var), fs::metadata(&layout.live_var)) {
        checks.push(check(
            "old /var is not the live /var",
            (a.dev(), a.ino()) != (b.dev(), b.ino()),
            true,
            format!("{} vs {}", old_var.display(), layout.live_var.display()),
        ));
    }

    let mountpoints = parse_mountpoints(&facts.proc_mounts);
    let targets = plan_targets(layout)?;
    let mounted: Vec<String> = targets
        .iter()
        .flat_map(|t| mounts_within(&t.path, &mountpoints))
        .collect();
    checks.push(check(
        "no filesystem mounted inside a target",
        mounted.is_empty(),
        true,
        if mounted.is_empty() {
            "none".into()
        } else {
            format!(
                "mounted inside a target: {} — unmount first (the composefs store may still \
                 be loop-mounted by a unit carried in /etc)",
                mounted.join(", ")
            )
        },
    ));

    Ok(CommitPlan {
        checks,
        report_path: report.map(|(p, _)| p),
        diff,
        targets,
        kept: kept_paths(layout)?,
    })
}

/// The paths to delete, in deletion order, with their sizes. Only paths
/// that exist are listed.
pub fn plan_targets(layout: &Layout) -> Result<Vec<Target>> {
    let mut spec: Vec<(String, &'static str)> = vec![
        (OLD_STATEROOT_VAR.into(), "old composefs stateroot /var"),
        (COMPOSEFS_STORE.into(), "composefs object store"),
        (COMPOSEFS_LOOPBACK.into(), "composefs store loopback image"),
    ];
    let deploy = layout.sysroot.join(OLD_DEPLOY_DIR);
    if deploy.is_dir() {
        let mut names: Vec<String> = fs::read_dir(&deploy)?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        for n in names {
            spec.push((format!("{OLD_DEPLOY_DIR}/{n}"), "composefs deployment"));
        }
    }
    let mut out = Vec::new();
    for (rel, what) in spec {
        let path = layout.sysroot.join(&rel);
        if fs::symlink_metadata(&path).is_err() {
            continue;
        }
        if !is_deletable(&rel) {
            bail!("refusing to plan the deletion of protected path {rel}");
        }
        out.push(Target {
            bytes: tree_usage(&path)?,
            rel,
            path,
            what,
        });
    }
    Ok(out)
}

/// What the commit leaves in place, and why.
fn kept_paths(layout: &Layout) -> Result<Vec<(String, String)>> {
    let mut kept = vec![
        (
            layout.sysroot.join("ostree").display().to_string(),
            "the OSTree repo and the booted deployment".to_string(),
        ),
        (
            "ESP and /boot".to_string(),
            "never touched; the composefs kernel, systemd-boot and loader entries the route \
             restored stay, but no longer boot anything after this commit"
                .to_string(),
        ),
        (
            "UEFI NVRAM".to_string(),
            "never touched; the \"Linux Boot Manager\" rollback entry stays — remove it with \
             `bootc-rebase boot-entries --interactive --apply`"
                .to_string(),
        ),
    ];
    // Anything else under the old state directory is not ours to judge.
    let state = layout.sysroot.join("state");
    if state.is_dir() {
        let known = ["os", "deploy"];
        let mut extra: Vec<String> = fs::read_dir(&state)?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| !known.contains(&n.as_str()))
            .collect();
        extra.sort();
        for n in extra {
            kept.push((
                state.join(n).display().to_string(),
                "not part of the composefs layout this commit knows".to_string(),
            ));
        }
    }
    Ok(kept)
}

impl CommitPlan {
    /// The failed checks that refuse the commit: every failed hard check,
    /// and every failed check when `force` is off.
    pub fn blocking(&self, force: bool) -> Vec<&Check> {
        self.checks
            .iter()
            .filter(|c| !c.ok && (c.hard || !force))
            .collect()
    }

    pub fn total_bytes(&self) -> u64 {
        self.targets.iter().map(|t| t.bytes).sum()
    }

    pub fn print(&self) {
        println!("=== Preconditions ===");
        for c in &self.checks {
            let mark = if c.ok { "ok  " } else { "FAIL" };
            println!("  [{mark}] {}: {}", c.name, c.detail);
        }

        println!("=== /var verification (old stateroot vs live) ===");
        print_limited("missing from the live /var", &self.diff.missing);
        print_limited("different size (informational)", &self.diff.size_changed);
        print_limited(
            "different file type (informational)",
            &self.diff.type_changed,
        );
        if self.diff.volatile_missing > 0 {
            println!(
                "  {} volatile path(s) missing, allowed ({})",
                self.diff.volatile_missing,
                VOLATILE_VAR_PATHS.join(", ")
            );
        }

        println!("=== Would delete, in order ===");
        if self.targets.is_empty() {
            println!("  nothing: no composefs state found");
        }
        for t in &self.targets {
            println!(
                "  {:>12}  {}  ({})",
                format_bytes(t.bytes),
                t.path.display(),
                t.what
            );
        }
        println!("  {:>12}  total", format_bytes(self.total_bytes()));

        println!("=== Kept ===");
        for (p, why) in &self.kept {
            println!("  {p}: {why}");
        }
    }
}

fn print_limited(label: &str, paths: &[String]) {
    if paths.is_empty() {
        return;
    }
    println!("  {} path(s) {label}:", paths.len());
    for p in paths.iter().take(DIFF_PRINT_LIMIT) {
        println!("    {p}");
    }
    if paths.len() > DIFF_PRINT_LIMIT {
        println!("    … and {} more", paths.len() - DIFF_PRINT_LIMIT);
    }
}

// ---- Execute -------------------------------------------------------------

/// What the commit did, written to [`COMMIT_RECORD_FILE`].
#[derive(Debug, Clone, Serialize)]
pub struct CommitOutcome {
    pub forensics_copied: Vec<String>,
    pub deleted: Vec<Target>,
    pub pruned: Vec<String>,
    pub reclaimed_bytes: u64,
    pub forced: bool,
}

/// Mirror the old `/var/lib/bootc-rebase` into the live one, never
/// overwriting a path that exists live. Returns the relative paths copied.
pub fn mirror_forensics(layout: &Layout) -> Result<Vec<String>> {
    let src = Layout::state_dir_in(&layout.old_var());
    let dst = Layout::state_dir_in(&layout.live_var);
    let mut copied = Vec::new();
    if !src.is_dir() {
        return Ok(copied);
    }
    let mut stack = vec![PathBuf::new()];
    while let Some(rel_dir) = stack.pop() {
        fs::create_dir_all(dst.join(&rel_dir))?;
        for entry in fs::read_dir(src.join(&rel_dir))? {
            let entry = entry?;
            let rel = rel_dir.join(entry.file_name());
            let ty = entry.file_type()?;
            if ty.is_dir() {
                stack.push(rel);
                continue;
            }
            let to = dst.join(&rel);
            if fs::symlink_metadata(&to).is_ok() {
                continue;
            }
            if ty.is_symlink() {
                std::os::unix::fs::symlink(fs::read_link(entry.path())?, &to)?;
            } else if ty.is_file() {
                fs::copy(entry.path(), &to)
                    .with_context(|| format!("copying {}", entry.path().display()))?;
            } else {
                continue;
            }
            copied.push(rel.to_string_lossy().into_owned());
        }
    }
    copied.sort();
    Ok(copied)
}

/// Forensics, then the deletes in plan order, then the empty-directory
/// prune. Every target is re-checked against [`is_deletable`] and the live
/// mount table immediately before it is removed.
pub fn execute(
    layout: &Layout,
    plan: &CommitPlan,
    proc_mounts: &str,
    forced: bool,
) -> Result<CommitOutcome> {
    let forensics_copied = mirror_forensics(layout).context(
        "mirroring the install report and ESP snapshot into the live /var; nothing was deleted",
    )?;
    println!(
        "[forensics] {} file(s) mirrored into {}",
        forensics_copied.len(),
        Layout::state_dir_in(&layout.live_var).display()
    );

    let mountpoints = parse_mountpoints(proc_mounts);
    let mut deleted = Vec::new();
    for t in &plan.targets {
        if !is_deletable(&t.rel) || t.path != layout.sysroot.join(&t.rel) {
            bail!("refusing to delete protected path {}", t.path.display());
        }
        let inside = mounts_within(&t.path, &mountpoints);
        if !inside.is_empty() {
            bail!(
                "refusing to delete {}: {} mounted inside it",
                t.path.display(),
                inside.join(", ")
            );
        }
        println!(
            "[delete] {} ({}, {})",
            t.path.display(),
            t.what,
            format_bytes(t.bytes)
        );
        let meta = match fs::symlink_metadata(&t.path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_dir() {
            fs::remove_dir_all(&t.path)
        } else {
            fs::remove_file(&t.path)
        }
        .with_context(|| format!("deleting {}", t.path.display()))?;
        deleted.push(t.clone());
    }

    let mut pruned = Vec::new();
    for rel in PRUNE_IF_EMPTY {
        let p = layout.sysroot.join(rel);
        if fs::read_dir(&p).is_ok_and(|mut d| d.next().is_none()) && fs::remove_dir(&p).is_ok() {
            pruned.push(p.display().to_string());
        }
    }

    let outcome = CommitOutcome {
        forensics_copied,
        reclaimed_bytes: deleted.iter().map(|t| t.bytes).sum(),
        deleted,
        pruned,
        forced,
    };
    let record = Layout::state_dir_in(&layout.live_var).join(COMMIT_RECORD_FILE);
    fs::write(
        &record,
        serde_json::to_string_pretty(&outcome).expect("CommitOutcome serializes"),
    )
    .with_context(|| format!("writing {}", record.display()))?;
    println!("Commit record written to {}", record.display());
    Ok(outcome)
}

/// [`execute`] on the booted host: `/sysroot` is read-only on an OSTree
/// boot, so it is remounted read-write around the deletes and back to
/// read-only afterwards (a busy filesystem refuses that; it then stays
/// read-write until the next boot, which is harmless).
pub fn execute_host(plan: &CommitPlan, forced: bool) -> Result<CommitOutcome> {
    let status = Command::new("mount")
        .args(["-o", "remount,rw", PHYSICAL_ROOT])
        .status()
        .context("running mount -o remount,rw /sysroot")?;
    if !status.success() {
        bail!("mount -o remount,rw {PHYSICAL_ROOT} failed (exit {status}); nothing was deleted");
    }
    let proc_mounts = fs::read_to_string("/proc/mounts").context("reading /proc/mounts")?;
    let result = execute(&Layout::host(), plan, &proc_mounts, forced);
    let ro = Command::new("mount")
        .args(["-o", "remount,ro", PHYSICAL_ROOT])
        .status();
    if !ro.is_ok_and(|s| s.success()) {
        eprintln!(
            "Note: {PHYSICAL_ROOT} could not be remounted read-only (busy); it stays read-write \
             until the next boot, which is harmless."
        );
    }
    let outcome = result?;
    let btrfs = parse_mountpoints(&proc_mounts)
        .iter()
        .any(|(mp, fstype)| mp == PHYSICAL_ROOT && fstype == "btrfs");
    println!(
        "Reclaimed {} from {} path(s).",
        format_bytes(outcome.reclaimed_bytes),
        outcome.deleted.len()
    );
    if btrfs {
        println!(
            "Note: btrfs frees deleted extents in the background; `df` can take a while to \
             show the reclaimed space (`btrfs filesystem sync {PHYSICAL_ROOT}` waits for it)."
        );
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn var_relative_table() {
        for (abs, want) in [
            ("/var/lib/bootc-rebase", Some("lib/bootc-rebase")),
            (
                "/var/lib/bootc-rebase/esp-snapshot-1",
                Some("lib/bootc-rebase/esp-snapshot-1"),
            ),
            ("/var/", None),
            ("/etc/x", None),
            ("var/lib", None),
        ] {
            assert_eq!(var_relative(abs), want, "{abs}");
        }
    }

    #[test]
    fn cmdline_table() {
        let ostree = "BOOT_IMAGE=(hd0,gpt2)/ostree/x/vmlinuz root=UUID=1 rw \
                      ostree=/ostree/boot.1/default/abc/0 quiet";
        assert_eq!(
            ostree_cmdline_path(ostree),
            Some("/ostree/boot.1/default/abc/0")
        );
        assert!(!booted_composefs(ostree));

        let cfs = "root=UUID=1 composefs=deadbeef rw";
        assert_eq!(ostree_cmdline_path(cfs), None);
        assert!(booted_composefs(cfs));

        assert_eq!(ostree_cmdline_path("ostree= quiet"), None);
        // A karg that merely contains the word is not the argument.
        assert!(!booted_composefs("rd.composefs=0"));
    }

    #[test]
    fn volatile_table() {
        for (rel, want) in [
            ("tmp", true),
            ("tmp/x", true),
            ("tmpfoo", false),
            ("cache/dnf/x", true),
            ("log/journal/abc/system.journal", true),
            ("log/messages", false),
            ("lib/bootc-rebase/ostree-install-report.json", true),
            ("lib/containers/storage/overlay/x", false),
            ("home/user/file", false),
        ] {
            assert_eq!(is_volatile(rel), want, "{rel}");
        }
    }

    #[test]
    fn deletable_table() {
        for (rel, want) in [
            ("state/os/default/var", true),
            ("composefs", true),
            ("state/deploy/abc", true),
            ("ostree", false),
            ("ostree/repo", false),
            ("boot/loader", false),
            ("efi", false),
            ("", false),
            ("/state", false),
            ("state/../ostree", false),
            ("./state", false),
        ] {
            assert_eq!(is_deletable(rel), want, "{rel:?}");
        }
    }

    #[test]
    fn mountpoints_parse_and_match() {
        let text = "/dev/vda3 /sysroot btrfs ro,relatime 0 0\n\
                    /dev/loop0 /sysroot/composefs ext4 rw 0 0\n\
                    /dev/vdb1 /mnt/with\\040space xfs rw 0 0\n";
        let mps = parse_mountpoints(text);
        assert_eq!(mps[0], ("/sysroot".into(), "btrfs".into()));
        assert_eq!(mps[2].0, "/mnt/with space");
        assert_eq!(
            mounts_within(Path::new("/sysroot/composefs"), &mps),
            vec!["/sysroot/composefs".to_string()]
        );
        assert!(mounts_within(Path::new("/sysroot/state"), &mps).is_empty());
        // Prefix of a name is not containment.
        assert!(mounts_within(Path::new("/sysroot/compose"), &mps).is_empty());
    }

    #[test]
    fn format_bytes_table() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.00 KiB");
        assert_eq!(format_bytes(560 * 1024 * 1024 * 1024), "560.00 GiB");
    }

    fn write(p: &Path, body: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    #[test]
    fn diff_var_reports_missing_volatile_size_and_type() {
        let d = tempdir().unwrap();
        let (old, live) = (d.path().join("old"), d.path().join("live"));
        write(&old.join("home/u/a.txt"), "aaaa");
        write(&live.join("home/u/a.txt"), "aaaa");
        write(&old.join("home/u/gone/deep/x"), "x");
        write(&old.join("log/grown.log"), "1");
        write(&live.join("log/grown.log"), "12");
        write(&old.join("tmp/scratch"), "s");
        write(&old.join("lib/kind"), "file");
        fs::create_dir_all(live.join("lib/kind")).unwrap();
        std::os::unix::fs::symlink("a.txt", old.join("home/u/link")).unwrap();
        std::os::unix::fs::symlink("a.txt", live.join("home/u/link")).unwrap();

        let diff = diff_var(&old, &live).unwrap();
        // The missing directory is reported once, not per child.
        assert_eq!(diff.missing, vec!["home/u/gone".to_string()]);
        assert_eq!(diff.volatile_missing, 1);
        assert_eq!(diff.size_changed, vec!["log/grown.log".to_string()]);
        assert_eq!(diff.type_changed, vec!["lib/kind".to_string()]);
    }

    #[test]
    fn diff_var_of_absent_old_var_is_empty() {
        let d = tempdir().unwrap();
        let diff = diff_var(&d.path().join("nope"), d.path()).unwrap();
        assert_eq!(diff.entries, 0);
        assert!(diff.missing.is_empty());
    }

    #[test]
    fn tree_usage_counts_hard_links_once() {
        let d = tempdir().unwrap();
        let f = d.path().join("t/f");
        write(&f, &"x".repeat(64 * 1024));
        let once = tree_usage(&d.path().join("t")).unwrap();
        fs::hard_link(&f, d.path().join("t/g")).unwrap();
        assert_eq!(tree_usage(&d.path().join("t")).unwrap(), once);
        assert!(once >= 64 * 1024);
        assert_eq!(tree_usage(&d.path().join("absent")).unwrap(), 0);
    }

    /// A physical root shaped like a host after the route: OSTree beside
    /// the composefs state, the report on the old side only.
    fn fixture(deployment: &str) -> (tempfile::TempDir, Layout) {
        let d = tempdir().unwrap();
        let sysroot = d.path().join("sysroot");
        let live_var = d.path().join("var");
        let old_var = sysroot.join(OLD_STATEROOT_VAR);
        let report = serde_json::json!({
            "target_image": "ghcr.io/example/utah:latest",
            "deployment": deployment,
            "esp": "/boot/efi",
            "esp_snapshot_dir": "/var/lib/bootc-rebase/esp-snapshot-1",
            "esp_paths_restored": [],
            "etc_merge": "merged",
            "var_copied": true,
            "cross_family": false,
            "grub_boot_entry": "0001",
        });
        write(
            &old_var.join("lib/bootc-rebase").join(REPORT_FILE),
            &report.to_string(),
        );
        write(
            &old_var.join("lib/bootc-rebase/esp-snapshot-1/loader/loader.conf"),
            "timeout 3",
        );
        // Already mirrored by an earlier step: must not be overwritten.
        write(
            &live_var.join("lib/bootc-rebase/esp-snapshot-1/loader/loader.conf"),
            "live copy",
        );
        write(&old_var.join("home/u/data"), "data");
        write(&live_var.join("home/u/data"), "data");
        write(&sysroot.join("composefs/objects/ab/cd"), "obj");
        write(&sysroot.join("state/deploy/0123abcd/etc/hostname"), "h");
        write(&sysroot.join("state/keepme/x"), "?");
        let deploy = sysroot
            .join("ostree/deploy/default/deploy")
            .join(deployment);
        fs::create_dir_all(&deploy).unwrap();
        fs::create_dir_all(sysroot.join("ostree/boot.1/default/abc")).unwrap();
        std::os::unix::fs::symlink(
            format!("../../../deploy/default/deploy/{deployment}"),
            sysroot.join("ostree/boot.1/default/abc/0"),
        )
        .unwrap();
        write(&sysroot.join("ostree/repo/config"), "[core]");
        (d, Layout { sysroot, live_var })
    }

    fn facts() -> HostFacts {
        HostFacts {
            cmdline: "root=UUID=1 rw ostree=/ostree/boot.1/default/abc/0".into(),
            efibootmgr: Some(
                "BootCurrent: 0001\nBootOrder: 0001,0002\n\
                 Boot0001* Fedora\tHD(1,GPT,x)/File(\\EFI\\fedora\\shimx64.efi)\n\
                 Boot0002* Linux Boot Manager\tHD(1,GPT,x)/File(\\EFI\\systemd\\systemd-bootx64.efi)\n"
                    .into(),
            ),
            proc_mounts: String::new(),
        }
    }

    #[test]
    fn plan_passes_on_a_healthy_host_and_lists_targets_in_order() {
        let (_d, layout) = fixture("cafe.0");
        let plan = plan(&layout, &facts()).unwrap();
        assert!(
            plan.blocking(false).is_empty(),
            "unexpected refusal: {:?}",
            plan.blocking(false)
        );
        let rels: Vec<&str> = plan.targets.iter().map(|t| t.rel.as_str()).collect();
        assert_eq!(
            rels,
            vec![OLD_STATEROOT_VAR, COMPOSEFS_STORE, "state/deploy/0123abcd"]
        );
        assert!(plan.total_bytes() > 0);
        assert!(plan.kept.iter().any(|(p, _)| p.ends_with("state/keepme")));
    }

    #[test]
    fn plan_refuses_live_missing_data_unless_forced() {
        let (_d, layout) = fixture("cafe.0");
        write(&layout.old_var().join("home/u/only-old"), "lost");
        let plan = plan(&layout, &facts()).unwrap();
        assert_eq!(plan.diff.missing, vec!["home/u/only-old".to_string()]);
        assert_eq!(plan.blocking(false).len(), 1);
        assert!(plan.blocking(true).is_empty());
    }

    #[test]
    fn plan_refuses_a_composefs_boot_even_when_forced() {
        let (_d, layout) = fixture("cafe.0");
        let f = HostFacts {
            cmdline: "root=UUID=1 composefs=abc".into(),
            ..facts()
        };
        let plan = plan(&layout, &f).unwrap();
        let hard: Vec<_> = plan.blocking(true).iter().map(|c| c.name).collect();
        assert_eq!(hard, vec!["booted from OSTree"]);
    }

    #[test]
    fn plan_flags_a_deployment_mismatch_and_missing_grub() {
        let (_d, layout) = fixture("cafe.0");
        let other = layout.sysroot.join("ostree/deploy/default/deploy/beef.0");
        fs::create_dir_all(&other).unwrap();
        let link = layout.sysroot.join("ostree/boot.1/default/abc/0");
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink("../../../deploy/default/deploy/beef.0", &link).unwrap();
        let f = HostFacts {
            efibootmgr: Some("BootOrder: 0002\nBoot0002* Linux Boot Manager\tX\n".into()),
            ..facts()
        };
        let plan = plan(&layout, &f).unwrap();
        let names: Vec<_> = plan.blocking(false).iter().map(|c| c.name).collect();
        assert_eq!(
            names,
            vec![
                "booted deployment matches the report",
                "firmware reaches the OSTree deployment"
            ]
        );
    }

    #[test]
    fn plan_refuses_a_target_with_a_mount_inside_even_when_forced() {
        let (_d, layout) = fixture("cafe.0");
        let f = HostFacts {
            proc_mounts: format!(
                "/dev/loop0 {} ext4 rw 0 0\n",
                layout.sysroot.join("composefs").display()
            ),
            ..facts()
        };
        let plan = plan(&layout, &f).unwrap();
        let hard: Vec<_> = plan.blocking(true).iter().map(|c| c.name).collect();
        assert_eq!(hard, vec!["no filesystem mounted inside a target"]);
    }

    #[test]
    fn execute_mirrors_forensics_then_deletes_only_the_old_world() {
        let (_d, layout) = fixture("cafe.0");
        let plan = plan(&layout, &facts()).unwrap();
        let outcome = execute(&layout, &plan, "", false).unwrap();

        // Forensics: the report came across; the existing live copy won.
        let state = layout.live_var.join("lib/bootc-rebase");
        assert!(state.join(REPORT_FILE).is_file());
        assert_eq!(
            fs::read_to_string(state.join("esp-snapshot-1/loader/loader.conf")).unwrap(),
            "live copy"
        );
        assert_eq!(outcome.forensics_copied, vec![REPORT_FILE.to_string()]);
        assert!(state.join(COMMIT_RECORD_FILE).is_file());

        // The old world is gone; the OSTree side and unknown state are not.
        assert!(!layout.old_var().exists());
        assert!(!layout.sysroot.join("composefs").exists());
        assert!(!layout.sysroot.join("state/deploy").exists());
        assert!(!layout.sysroot.join("state/os").exists());
        assert!(layout.sysroot.join("state/keepme/x").is_file());
        assert!(layout.sysroot.join("ostree/repo/config").is_file());
        assert!(
            layout
                .sysroot
                .join("ostree/deploy/default/deploy/cafe.0")
                .is_dir()
        );
        assert_eq!(outcome.deleted.len(), 3);
        assert_eq!(outcome.reclaimed_bytes, plan.total_bytes());
    }

    #[test]
    fn execute_refuses_a_target_that_became_a_mountpoint() {
        let (_d, layout) = fixture("cafe.0");
        let plan = plan(&layout, &facts()).unwrap();
        let mounts = format!(
            "/dev/vdb1 {} xfs rw 0 0\n",
            layout.old_var().join("home").display()
        );
        let err = execute(&layout, &plan, &mounts, false).unwrap_err();
        assert!(err.to_string().contains("mounted inside it"), "{err:#}");
        assert!(layout.old_var().join("home/u/data").is_file());
    }

    #[test]
    fn execute_refuses_a_tampered_protected_target() {
        let (_d, layout) = fixture("cafe.0");
        let mut plan = plan(&layout, &facts()).unwrap();
        plan.targets[0].rel = "ostree".into();
        plan.targets[0].path = layout.sysroot.join("ostree");
        assert!(execute(&layout, &plan, "", false).is_err());
        assert!(layout.sysroot.join("ostree/repo/config").is_file());
    }
}
