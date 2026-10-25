//! Guided commit for package-managed hosts migrated to OSTree (issue #378).
//!
//! The package route installs OSTree beside the package-managed root. This
//! module proves that the new deployment booted cleanly, reports every old-root
//! path it can safely remove, and removes only paths whose ownership or carried
//! copy is known. Unknown data remains on the physical root for manual review.

use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use crate::ostree_install::{PACKAGE_REPORT_FILE, PHYSICAL_ROOT, PackageInstallReport};
use crate::ostree_install_commit::{
    diff_var, format_bytes, mounts_within, ostree_cmdline_path, parse_mountpoints, remount_command,
    tree_usage,
};
use crate::package_data::is_allowed_var_lib_path;

pub const COMMIT_RECORD_FILE: &str = "package-install-commit.json";
const VERIFY_RESULT_REL: &str = "lib/bootc-migrate/verify-result";
const ROLLBACK_ENTRY: &str = "loader/entries/bootc-migrate-rollback.conf";
const ROLLBACK_DIR: &str = "bootc-migrate-rollback";
const ROLLBACK_ID: &str = "bootc-migrate-rollback";
const ROLLBACK_STATE_REL: &str = "var/lib/bootc-rebase/package-rollback";
const PACKAGE_STATE_DIRS: &[&str] = &["rpm", "dnf", "apt", "dpkg", "pacman", "yum", "zypper"];
const PRUNE_IF_EMPTY: &[&str] = &[
    "var/lib/bootc-rebase",
    "var/lib",
    "var/cache",
    "var/tmp",
    "var/run",
    "var/lock",
    "var",
];

#[derive(Debug, Clone)]
pub struct Layout {
    pub sysroot: PathBuf,
    pub live_var: PathBuf,
    pub deployment_var: PathBuf,
    pub live_etc: PathBuf,
    pub boot: PathBuf,
    pub live_home: PathBuf,
}

impl Layout {
    pub fn host() -> Self {
        Self {
            sysroot: PathBuf::from(PHYSICAL_ROOT),
            live_var: PathBuf::from("/var"),
            deployment_var: PathBuf::from("/sysroot/ostree/deploy/default/var"),
            live_etc: PathBuf::from("/etc"),
            boot: PathBuf::from("/boot"),
            live_home: PathBuf::from("/var/home"),
        }
    }

    fn report_path(&self) -> PathBuf {
        self.live_var
            .join("lib/bootc-rebase")
            .join(PACKAGE_REPORT_FILE)
    }

    fn old(&self, rel: &str) -> PathBuf {
        self.sysroot.join(rel)
    }
}

#[derive(Debug, Clone, Default)]
pub struct HostFacts {
    pub cmdline: String,
    pub proc_mounts: String,
    pub verify_result: String,
}

impl HostFacts {
    pub fn gather(layout: &Layout) -> Self {
        Self {
            cmdline: fs::read_to_string("/proc/cmdline").unwrap_or_default(),
            proc_mounts: fs::read_to_string("/proc/mounts").unwrap_or_default(),
            verify_result: fs::read_to_string(layout.live_var.join(VERIFY_RESULT_REL))
                .unwrap_or_default(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    /// Package cleanup checks are hard: `--force` must not turn missing data or
    /// an unhealthy deployment into permission to erase the old system.
    pub hard: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Target {
    /// Relative to the physical root.
    pub rel: String,
    pub path: PathBuf,
    pub bytes: u64,
    pub what: String,
}

#[derive(Debug, Clone, Serialize)]
pub enum RollbackEntry {
    Bls { path: PathBuf, bytes: u64 },
    Grub { path: PathBuf, bytes: u64 },
}

impl RollbackEntry {
    fn path(&self) -> &Path {
        match self {
            Self::Bls { path, .. } | Self::Grub { path, .. } => path,
        }
    }

    fn bytes(&self) -> u64 {
        match self {
            Self::Bls { bytes, .. } | Self::Grub { bytes, .. } => *bytes,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct HomeMove {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub fstab: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct CommitPlan {
    pub checks: Vec<Check>,
    pub report_path: PathBuf,
    pub targets: Vec<Target>,
    pub rollback_entry: Option<RollbackEntry>,
    pub home_move: Option<HomeMove>,
    pub kept: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CommitOutcome {
    pub deleted: Vec<Target>,
    pub rollback_entry_removed: Option<String>,
    pub home_moved: bool,
    pub reclaimed_bytes: u64,
    pub forced: bool,
}

fn check(name: &'static str, ok: bool, detail: impl Into<String>) -> Check {
    Check {
        name,
        ok,
        hard: true,
        detail: detail.into(),
    }
}

pub fn find_report(layout: &Layout) -> Option<PackageInstallReport> {
    let text = fs::read_to_string(layout.report_path()).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn report_present(layout: &Layout) -> bool {
    layout.report_path().exists()
}
fn booted_deployment(layout: &Layout, cmdline: &str) -> Option<String> {
    let arg = ostree_cmdline_path(cmdline)?;
    let resolved = fs::canonicalize(layout.sysroot.join(arg.trim_start_matches('/'))).ok()?;
    Some(resolved.file_name()?.to_string_lossy().into_owned())
}

fn clean_relative(rel: &str) -> bool {
    !rel.is_empty()
        && Path::new(rel)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn package_state_rel(rel: &str) -> bool {
    let Some(name) = rel.strip_prefix("var/lib/") else {
        return false;
    };
    !name.contains('/') && PACKAGE_STATE_DIRS.contains(&name)
}

/// The complete allowlist for deletion below the physical package root.
pub fn is_deletable(rel: &str) -> bool {
    if !clean_relative(rel) {
        return false;
    }
    if matches!(
        rel,
        "usr" | "etc" | "opt" | "bin" | "lib" | "lib64" | "sbin"
    ) {
        return true;
    }
    if matches!(rel, "var/cache" | "var/tmp" | "var/run" | "var/lock")
        || rel == ROLLBACK_STATE_REL
        || package_state_rel(rel)
    {
        return true;
    }
    if let Some(name) = rel.strip_prefix("var/")
        && !name.contains('/')
        && !matches!(name, "home" | "lib" | "cache" | "tmp" | "run" | "lock")
    {
        return true;
    }
    rel.strip_prefix("var/lib/")
        .is_some_and(|p| is_allowed_var_lib_path(Path::new(p), true))
        || rel
            .strip_prefix("var/lib/bootc-rebase/")
            .is_some_and(clean_relative)
}

fn add_target(targets: &mut Vec<Target>, layout: &Layout, rel: &str, what: &str) -> Result<()> {
    let path = layout.old(rel);
    if fs::symlink_metadata(&path).is_err() {
        return Ok(());
    }
    if !is_deletable(rel) {
        bail!("refusing to plan deletion of protected path {rel}");
    }
    targets.push(Target {
        rel: rel.to_string(),
        bytes: tree_usage(&path)?,
        path,
        what: what.to_string(),
    });
    Ok(())
}

fn tree_is_carried(source: &Path, destination: &Path) -> Result<(bool, String)> {
    let source_meta = match fs::symlink_metadata(source) {
        Ok(meta) => meta,
        Err(_) => return Ok((true, format!("{} does not exist", source.display()))),
    };
    let destination_meta = match fs::symlink_metadata(destination) {
        Ok(meta) => meta,
        Err(_) => {
            return Ok((
                false,
                format!(
                    "{} exists but its carried destination {} is missing",
                    source.display(),
                    destination.display()
                ),
            ));
        }
    };
    if source_meta.file_type().is_dir() != destination_meta.file_type().is_dir()
        || source_meta.file_type().is_symlink() != destination_meta.file_type().is_symlink()
    {
        return Ok((
            false,
            format!(
                "{} and {} have different file types",
                source.display(),
                destination.display()
            ),
        ));
    }
    if !source_meta.is_dir() {
        return Ok((true, format!("{} is present", destination.display())));
    }
    let diff = diff_var(source, destination)?;
    let ok = diff.missing.is_empty() && diff.volatile_missing == 0 && diff.type_changed.is_empty();
    Ok((
        ok,
        if ok {
            format!(
                "{} entries are present under {}",
                diff.entries,
                destination.display()
            )
        } else {
            format!(
                "{} path(s) from {} are missing, volatile-missing, or type-changed under {}: {}",
                diff.missing.len() + diff.volatile_missing as usize + diff.type_changed.len(),
                source.display(),
                destination.display(),
                diff.missing
                    .iter()
                    .chain(&diff.type_changed)
                    .take(5)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        },
    ))
}

fn add_carried_target(
    checks: &mut Vec<Check>,
    targets: &mut Vec<Target>,
    layout: &Layout,
    rel: &str,
    destination: &Path,
    what: &str,
) -> Result<()> {
    let source = layout.old(rel);
    if fs::symlink_metadata(&source).is_err() {
        return Ok(());
    }
    let (ok, detail) = tree_is_carried(&source, destination)?;
    checks.push(check("old data has a carried copy", ok, detail));
    add_target(targets, layout, rel, what)
}

fn compatibility_link(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
        && fs::read_link(path).is_ok_and(|target| {
            let target = target.to_string_lossy();
            target == "usr" || target.starts_with("usr/") || target.starts_with("/usr/")
        })
}

fn plan_old_root(
    layout: &Layout,
    checks: &mut Vec<Check>,
    targets: &mut Vec<Target>,
) -> Result<()> {
    let local = layout.old("usr/local");
    if fs::symlink_metadata(&local).is_ok() {
        let (ok, detail) = tree_is_carried(&local, &layout.live_var.join("usrlocal"))?;
        checks.push(check("/usr/local has a carried copy", ok, detail));
    }
    add_target(targets, layout, "usr", "old package-managed /usr")?;
    add_target(targets, layout, "etc", "old package-host /etc")?;
    add_carried_target(
        checks,
        targets,
        layout,
        "opt",
        &layout.live_var.join("opt"),
        "old /opt after carry to /var/opt",
    )?;

    for rel in ["bin", "lib", "lib64", "sbin"] {
        if compatibility_link(&layout.old(rel)) {
            add_target(targets, layout, rel, "old usr-merge compatibility symlink")?;
        }
    }

    let old_var = layout.old("var");
    if !old_var.is_dir() {
        return Ok(());
    }
    let mut top: Vec<_> = fs::read_dir(&old_var)?.filter_map(|e| e.ok()).collect();
    top.sort_by_key(|e| e.file_name());
    for entry in top {
        let name = entry.file_name().to_string_lossy().into_owned();
        match name.as_str() {
            "home" | "lib" => continue,
            "cache" | "tmp" | "run" | "lock" => add_target(
                targets,
                layout,
                &format!("var/{name}"),
                "old package-host cache or runtime state",
            )?,
            _ => add_carried_target(
                checks,
                targets,
                layout,
                &format!("var/{name}"),
                &layout.live_var.join(&name),
                "old /var tree after carry",
            )?,
        }
    }

    let old_lib = old_var.join("lib");
    if old_lib.is_dir() {
        let mut children: Vec<_> = fs::read_dir(&old_lib)?.filter_map(|e| e.ok()).collect();
        children.sort_by_key(|e| e.file_name());
        for entry in children {
            let name = entry.file_name().to_string_lossy().into_owned();
            let rel = format!("var/lib/{name}");
            if PACKAGE_STATE_DIRS.contains(&name.as_str()) {
                add_target(targets, layout, &rel, "old package-manager state")?;
            } else if is_allowed_var_lib_path(Path::new(&name), true) {
                add_carried_target(
                    checks,
                    targets,
                    layout,
                    &rel,
                    &layout.live_var.join("lib").join(&name),
                    "old application state after carry",
                )?;
            }
        }
    }
    Ok(())
}

fn rollback_entry(layout: &Layout) -> Result<Option<RollbackEntry>> {
    let bls = layout.boot.join(ROLLBACK_ENTRY);
    if bls.is_file() {
        return Ok(Some(RollbackEntry::Bls {
            bytes: fs::metadata(&bls)?.len(),
            path: bls,
        }));
    }
    for path in [
        layout.boot.join("grub2/custom.cfg"),
        layout.boot.join("grub/custom.cfg"),
    ] {
        if let Ok(content) = fs::read_to_string(&path)
            && let Some(updated) = remove_grub_entry(&content)
        {
            return Ok(Some(RollbackEntry::Grub {
                bytes: content.len().saturating_sub(updated.len()) as u64,
                path,
            }));
        }
    }
    Ok(None)
}

fn fstab_has_root_home_bind(fstab: &str) -> bool {
    fstab.lines().any(|line| {
        let fields: Vec<_> = line.split_whitespace().collect();
        !line.trim_start().starts_with('#')
            && fields.len() >= 4
            && fields[0] == "/sysroot/home"
            && fields[1] == "/var/home"
            && fields[3].split(',').any(|option| option == "bind")
    })
}

fn mount_source(mounts: &str, mountpoint: &Path) -> Option<String> {
    let mountpoint = mountpoint.to_string_lossy();
    mounts.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let source = fields.next()?;
        (fields.next()? == mountpoint).then(|| source.to_string())
    })
}

fn plan_home_move(
    layout: &Layout,
    facts: &HostFacts,
    requested: bool,
    checks: &mut Vec<Check>,
) -> Result<Option<HomeMove>> {
    let fstab = layout.live_etc.join("fstab");
    let text = fs::read_to_string(&fstab).unwrap_or_default();
    if !fstab_has_root_home_bind(&text) {
        return Ok(None);
    }
    if !requested {
        return Ok(None);
    }

    let source = layout.old("home");
    let destination = layout.deployment_var.join("home");
    let source_exists = source.is_dir();
    checks.push(check(
        "plain-directory /home source exists",
        source_exists,
        format!("{}", source.display()),
    ));
    let mounted_from_source = mount_source(&facts.proc_mounts, &layout.live_home)
        .is_some_and(|mounted| mounted == source.to_string_lossy());
    checks.push(check(
        "/var/home is the old-home bind",
        mounted_from_source,
        format!(
            "{} is mounted from {}",
            layout.live_home.display(),
            mount_source(&facts.proc_mounts, &layout.live_home)
                .as_deref()
                .unwrap_or("<unknown>")
        ),
    ));
    let destination_empty = !destination.exists()
        || (destination.is_dir()
            && fs::read_dir(&destination).is_ok_and(|mut entries| entries.next().is_none()));
    checks.push(check(
        "new /var/home is empty below the bind",
        destination_empty,
        format!("{}", destination.display()),
    ));
    if source_exists {
        let source_parent = fs::metadata(source.parent().unwrap_or(&layout.sysroot));
        let destination_parent =
            fs::metadata(destination.parent().unwrap_or(&layout.deployment_var));
        let same_fs =
            matches!((source_parent, destination_parent), (Ok(a), Ok(b)) if a.dev() == b.dev());
        checks.push(check(
            "plain-directory /home move stays on one filesystem",
            same_fs,
            format!("{} -> {}", source.display(), destination.display()),
        ));
    }
    Ok(Some(HomeMove {
        source,
        destination,
        fstab,
    }))
}

pub fn plan(layout: &Layout, facts: &HostFacts, move_home: bool) -> Result<CommitPlan> {
    let mut checks = Vec::new();
    let report = find_report(layout);
    checks.push(check(
        "package install report",
        report.is_some(),
        if report.is_some() {
            layout.report_path().display().to_string()
        } else {
            format!(
                "{} is missing or invalid; this host is not proven to be a package migration",
                layout.report_path().display()
            )
        },
    ));

    let booted = booted_deployment(layout, &facts.cmdline);
    checks.push(check(
        "booted from OSTree",
        ostree_cmdline_path(&facts.cmdline).is_some(),
        format!(
            "booted deployment: {}",
            booted.as_deref().unwrap_or("<unresolved>")
        ),
    ));
    if let Some(report) = &report {
        checks.push(check(
            "booted deployment matches the report",
            booted.as_deref() == Some(report.deployment.as_str()),
            format!(
                "booted {}, report names {}",
                booted.as_deref().unwrap_or("<unresolved>"),
                report.deployment
            ),
        ));
    }

    let healthy = facts.verify_result.trim() == "OK";
    checks.push(check(
        "first-boot verification is healthy",
        healthy,
        if facts.verify_result.trim().is_empty() {
            "verify-result is missing; wait for bootc-migrate-verify-firstboot.service and inspect its report"
                .to_string()
        } else {
            format!("verify-result: {}", facts.verify_result.trim())
        },
    ));

    let entry = rollback_entry(layout)?;
    checks.push(check(
        "Previous system boot entry exists",
        entry.is_some(),
        entry
            .as_ref()
            .map(|e| e.path().display().to_string())
            .unwrap_or_else(|| {
                "no managed BLS or GRUB Previous system entry was found".to_string()
            }),
    ));

    let mut targets = Vec::new();
    plan_old_root(layout, &mut checks, &mut targets)?;
    add_target(
        &mut targets,
        layout,
        ROLLBACK_STATE_REL,
        "saved package-host rollback snapshot",
    )?;
    if let Some(report) = &report
        && let Some(rel) = report.boot_snapshot_dir.strip_prefix("/var/")
    {
        add_target(
            &mut targets,
            layout,
            &format!("var/{rel}"),
            "pre-install boot snapshot",
        )?;
    }
    let rollback_dir = layout.boot.join(ROLLBACK_DIR);
    if rollback_dir.exists() {
        targets.push(Target {
            rel: format!("boot:{ROLLBACK_DIR}"),
            path: rollback_dir.clone(),
            bytes: tree_usage(&rollback_dir)?,
            what: "Previous system kernel snapshot".to_string(),
        });
    }

    if let (Ok(old), Ok(live)) = (
        fs::metadata(layout.old("var")),
        fs::metadata(&layout.live_var),
    ) {
        checks.push(check(
            "old /var is not the live /var",
            (old.dev(), old.ino()) != (live.dev(), live.ino()),
            format!(
                "{} vs {}",
                layout.old("var").display(),
                layout.live_var.display()
            ),
        ));
    }

    let mountpoints = parse_mountpoints(&facts.proc_mounts);
    let mounted: Vec<_> = targets
        .iter()
        .flat_map(|target| mounts_within(&target.path, &mountpoints))
        .collect();
    checks.push(check(
        "no filesystem mounted inside a deletion target",
        mounted.is_empty(),
        if mounted.is_empty() {
            "none".to_string()
        } else {
            format!(
                "mounted inside a target: {}; unmount first",
                mounted.join(", ")
            )
        },
    ));

    let home_move = plan_home_move(layout, facts, move_home, &mut checks)?;
    let mut kept = vec![
        (
            layout.old("ostree").display().to_string(),
            "the OSTree repo and booted deployment".to_string(),
        ),
        (layout.boot.display().to_string(), "target boot artifacts; only the managed Previous system entry and snapshot are removed".to_string()),
        (layout.old("home").display().to_string(), if home_move.is_some() { "moved atomically into the target /var/home" } else { "carried home source; pass --move-home after reviewing the dry run to move a plain-directory home" }.to_string()),
        (layout.old("root").display().to_string(), "administrator data is not classified as old OS content".to_string()),
        (layout.old("srv").display().to_string(), "site data is not classified as old OS content".to_string()),
    ];
    let old_lib = layout.old("var/lib");
    if old_lib.is_dir() {
        let mut unknown = Vec::new();
        for entry in fs::read_dir(&old_lib)?.filter_map(|entry| entry.ok()) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !PACKAGE_STATE_DIRS.contains(&name.as_str())
                && !is_allowed_var_lib_path(Path::new(&name), true)
                && name != "bootc-rebase"
            {
                unknown.push(name);
            }
        }
        unknown.sort();
        if !unknown.is_empty() {
            kept.push((
                old_lib.display().to_string(),
                format!(
                    "unclassified application state kept: {}",
                    unknown.join(", ")
                ),
            ));
        }
    }

    Ok(CommitPlan {
        checks,
        report_path: layout.report_path(),
        targets,
        rollback_entry: entry,
        home_move,
        kept,
    })
}

impl CommitPlan {
    pub fn blocking(&self, force: bool) -> Vec<&Check> {
        self.checks
            .iter()
            .filter(|check| !check.ok && (check.hard || !force))
            .collect()
    }

    pub fn total_bytes(&self) -> u64 {
        self.targets.iter().map(|target| target.bytes).sum::<u64>()
            + self.rollback_entry.as_ref().map_or(0, RollbackEntry::bytes)
    }

    pub fn print(&self) {
        println!("=== Preconditions ===");
        for check in &self.checks {
            println!(
                "  [{}] {}: {}",
                if check.ok { "ok  " } else { "FAIL" },
                check.name,
                check.detail
            );
        }
        println!("=== Would delete, in order ===");
        if let Some(entry) = &self.rollback_entry {
            println!(
                "  {:>12}  {}  (Previous system boot entry)",
                format_bytes(entry.bytes()),
                entry.path().display()
            );
        }
        for target in &self.targets {
            println!(
                "  {:>12}  {}  ({})",
                format_bytes(target.bytes),
                target.path.display(),
                target.what
            );
        }
        println!(
            "  {:>12}  total ({} bytes planned)",
            format_bytes(self.total_bytes()),
            self.total_bytes()
        );
        if let Some(home) = &self.home_move {
            println!("=== Would move plain-directory /home ===");
            println!(
                "  {} -> {} (same-filesystem rename; remove the /var/home bind)",
                home.source.display(),
                home.destination.display()
            );
        }
        println!("=== Kept ===");
        for (path, reason) in &self.kept {
            println!("  {path}: {reason}");
        }
    }
}

fn remove_grub_entry(content: &str) -> Option<String> {
    let lines: Vec<_> = content.lines().collect();
    let start = lines
        .iter()
        .position(|line| line.contains("menuentry") && line.contains(ROLLBACK_ID))?;
    let mut depth = 0isize;
    let mut end = None;
    for (index, line) in lines.iter().enumerate().skip(start) {
        depth += line.matches('{').count() as isize;
        depth -= line.matches('}').count() as isize;
        if depth == 0 && index >= start {
            end = Some(index);
            break;
        }
    }
    let end = end?;
    let mut kept = lines[..start].to_vec();
    kept.extend_from_slice(&lines[end + 1..]);
    let mut rendered = kept.join("\n");
    if !rendered.is_empty() {
        rendered.push('\n');
    }
    Some(rendered)
}

fn remove_rollback_entry(entry: &RollbackEntry) -> Result<()> {
    match entry {
        RollbackEntry::Bls { path, .. } => {
            fs::remove_file(path).with_context(|| format!("deleting {}", path.display()))
        }
        RollbackEntry::Grub { path, .. } => {
            let current =
                fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
            let updated = remove_grub_entry(&current).ok_or_else(|| {
                anyhow::anyhow!(
                    "managed Previous system entry disappeared from {}",
                    path.display()
                )
            })?;
            if updated.trim().is_empty() {
                fs::remove_file(path).with_context(|| format!("deleting {}", path.display()))
            } else {
                fs::write(path, updated).with_context(|| format!("updating {}", path.display()))
            }
        }
    }
}

fn remove_home_bind(fstab: &Path) -> Result<()> {
    let current =
        fs::read_to_string(fstab).with_context(|| format!("reading {}", fstab.display()))?;
    let mut updated = String::new();
    for line in current.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        let managed = !line.trim_start().starts_with('#')
            && fields.len() >= 4
            && fields[0] == "/sysroot/home"
            && fields[1] == "/var/home"
            && fields[3].split(',').any(|option| option == "bind");
        if !managed {
            updated.push_str(line);
            updated.push('\n');
        }
    }
    let temporary = fstab.with_extension("bootc-migrate-new");
    fs::write(&temporary, updated).with_context(|| format!("writing {}", temporary.display()))?;
    fs::rename(&temporary, fstab).with_context(|| format!("updating {}", fstab.display()))
}

fn apply_home_move(home: &HomeMove) -> Result<()> {
    if home.destination.is_dir()
        && fs::read_dir(&home.destination).is_ok_and(|mut entries| entries.next().is_none())
    {
        fs::remove_dir(&home.destination)
            .with_context(|| format!("removing empty {}", home.destination.display()))?;
    }
    fs::rename(&home.source, &home.destination).with_context(|| {
        format!(
            "moving {} to {} on the same filesystem",
            home.source.display(),
            home.destination.display()
        )
    })?;
    remove_home_bind(&home.fstab)
}

fn validate_target(
    layout: &Layout,
    target: &Target,
    mountpoints: &[(String, String)],
) -> Result<()> {
    let valid_path = if let Some(rel) = target.rel.strip_prefix("boot:") {
        clean_relative(rel) && target.path == layout.boot.join(rel) && rel == ROLLBACK_DIR
    } else {
        is_deletable(&target.rel) && target.path == layout.old(&target.rel)
    };
    if !valid_path {
        bail!(
            "refusing to delete protected path {}",
            target.path.display()
        );
    }
    let inside = mounts_within(&target.path, mountpoints);
    if !inside.is_empty() {
        bail!(
            "refusing to delete {}: {} mounted inside it",
            target.path.display(),
            inside.join(", ")
        );
    }
    Ok(())
}

pub fn execute(
    layout: &Layout,
    plan: &CommitPlan,
    proc_mounts: &str,
    forced: bool,
) -> Result<CommitOutcome> {
    if !plan.blocking(forced).is_empty() {
        bail!("refusing to execute a package commit whose plan has failed checks");
    }
    let mountpoints = parse_mountpoints(proc_mounts);
    // Discharge every path and mount check before the first mutation. The
    // checks run again immediately before each delete to narrow the race with
    // a mount appearing while cleanup is in progress.
    for target in &plan.targets {
        validate_target(layout, target, &mountpoints)?;
    }

    let mut deleted = Vec::new();
    for target in &plan.targets {
        validate_target(layout, target, &mountpoints)?;
        let metadata = match fs::symlink_metadata(&target.path) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if metadata.is_dir() {
            fs::remove_dir_all(&target.path)
        } else {
            fs::remove_file(&target.path)
        }
        .with_context(|| format!("deleting {}", target.path.display()))?;
        deleted.push(target.clone());
    }
    let home_moved = if let Some(home) = &plan.home_move {
        apply_home_move(home)?;
        true
    } else {
        false
    };
    let rollback_entry_removed = if let Some(entry) = &plan.rollback_entry {
        remove_rollback_entry(entry)?;
        Some(entry.path().display().to_string())
    } else {
        None
    };

    for rel in PRUNE_IF_EMPTY {
        let path = layout.old(rel);
        if fs::read_dir(&path).is_ok_and(|mut entries| entries.next().is_none()) {
            let _ = fs::remove_dir(path);
        }
    }

    let reclaimed_bytes = deleted.iter().map(|target| target.bytes).sum::<u64>()
        + plan.rollback_entry.as_ref().map_or(0, RollbackEntry::bytes);
    let outcome = CommitOutcome {
        deleted,
        rollback_entry_removed,
        home_moved,
        reclaimed_bytes,
        forced,
    };
    let record = layout
        .live_var
        .join("lib/bootc-rebase")
        .join(COMMIT_RECORD_FILE);
    fs::write(
        &record,
        serde_json::to_string_pretty(&outcome).expect("CommitOutcome serializes"),
    )
    .with_context(|| format!("writing {}", record.display()))?;
    Ok(outcome)
}

pub fn execute_host(plan: &CommitPlan, forced: bool) -> Result<CommitOutcome> {
    let layout = Layout::host();
    let rw = remount_command(PHYSICAL_ROOT, false);
    let status = Command::new(&rw[0])
        .args(&rw[1..])
        .status()
        .with_context(|| format!("running {}", rw.join(" ")))?;
    if !status.success() {
        bail!(
            "{} failed (exit {status}); nothing was deleted",
            rw.join(" ")
        );
    }

    if plan.home_move.is_some() {
        let status = Command::new("umount")
            .arg(&layout.live_home)
            .status()
            .context("unmounting /var/home before its same-filesystem move")?;
        if !status.success() {
            let ro = remount_command(PHYSICAL_ROOT, true);
            let _ = Command::new(&ro[0]).args(&ro[1..]).status();
            bail!(
                "umount {} failed (exit {status}); nothing was deleted",
                layout.live_home.display()
            );
        }
    }
    let proc_mounts = fs::read_to_string("/proc/mounts").context("reading /proc/mounts")?;
    let result = execute(&layout, plan, &proc_mounts, forced);
    if result.is_err()
        && plan
            .home_move
            .as_ref()
            .is_some_and(|home| home.source.exists())
    {
        let remount = Command::new("mount").arg(&layout.live_home).status();
        if !remount.is_ok_and(|status| status.success()) {
            eprintln!(
                "Warning: commit failed after /var/home was unmounted and it could not be mounted again; reboot restores the original bind."
            );
        }
    }
    let ro = remount_command(PHYSICAL_ROOT, true);
    if !Command::new(&ro[0])
        .args(&ro[1..])
        .status()
        .is_ok_and(|status| status.success())
    {
        eprintln!(
            "Note: {PHYSICAL_ROOT} could not be remounted read-only (busy); it stays read-write until the next boot."
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use tempfile::tempdir;

    fn fixture() -> (tempfile::TempDir, Layout, HostFacts, String) {
        let root = tempdir().unwrap();
        let sysroot = root.path().join("sysroot");
        let live_var = root.path().join("var");
        let deployment_var = sysroot.join("ostree/deploy/default/var");
        let live_etc = root.path().join("etc");
        let boot = root.path().join("boot");
        fs::create_dir_all(sysroot.join("usr/local/bin")).unwrap();
        fs::write(sysroot.join("usr/local/bin/tool"), "tool").unwrap();
        fs::create_dir_all(sysroot.join("etc")).unwrap();
        fs::write(sysroot.join("etc/old.conf"), "old").unwrap();
        fs::create_dir_all(sysroot.join("var/lib/rpm")).unwrap();
        fs::write(sysroot.join("var/lib/rpm/Packages"), "rpmdb").unwrap();
        fs::create_dir_all(sysroot.join("var/lib/flatpak/app/demo")).unwrap();
        fs::write(sysroot.join("var/lib/flatpak/app/demo/metadata"), "flatpak").unwrap();
        fs::create_dir_all(sysroot.join("var/log")).unwrap();
        fs::write(sysroot.join("var/log/old.log"), "log").unwrap();
        fs::create_dir_all(sysroot.join("var/lib/private-app")).unwrap();
        fs::write(sysroot.join("var/lib/private-app/data"), "keep").unwrap();
        fs::create_dir_all(sysroot.join("home/alice")).unwrap();
        fs::write(sysroot.join("home/alice/file"), "home").unwrap();
        symlink("usr/bin", sysroot.join("bin")).unwrap();

        fs::create_dir_all(live_var.join("usrlocal/bin")).unwrap();
        fs::write(live_var.join("usrlocal/bin/tool"), "tool").unwrap();
        fs::create_dir_all(live_var.join("lib/flatpak/app/demo")).unwrap();
        fs::write(live_var.join("lib/flatpak/app/demo/metadata"), "flatpak").unwrap();
        fs::create_dir_all(live_var.join("log")).unwrap();
        fs::write(live_var.join("log/old.log"), "log").unwrap();
        fs::create_dir_all(live_var.join("lib/bootc-rebase")).unwrap();
        fs::create_dir_all(&deployment_var).unwrap();
        fs::create_dir_all(&live_etc).unwrap();
        fs::create_dir_all(boot.join("loader/entries")).unwrap();
        fs::create_dir_all(boot.join(ROLLBACK_DIR)).unwrap();
        fs::write(boot.join(ROLLBACK_DIR).join("kernel"), "kernel").unwrap();
        fs::write(boot.join(ROLLBACK_ENTRY), "title Previous system\n").unwrap();

        let deployment = "abc.0";
        let deploy = sysroot
            .join("ostree/deploy/default/deploy")
            .join(deployment);
        fs::create_dir_all(&deploy).unwrap();
        let boot_link = sysroot.join("ostree/boot.1/default/abc/0");
        fs::create_dir_all(boot_link.parent().unwrap()).unwrap();
        symlink(&deploy, &boot_link).unwrap();
        let report = PackageInstallReport {
            target_image: "quay.io/fedora/fedora-bootc:43".into(),
            deployment: deployment.into(),
            esp: boot.display().to_string(),
            boot_snapshot_dir: "/var/lib/bootc-rebase/pre-install-boot".into(),
            boot_snapshot_paths: Vec::new(),
            etc_merge: "merged".into(),
            var_copied: true,
            cross_family: false,
            grub_boot_entry: Some("0001".into()),
        };
        fs::write(
            live_var.join("lib/bootc-rebase").join(PACKAGE_REPORT_FILE),
            serde_json::to_string(&report).unwrap(),
        )
        .unwrap();
        fs::create_dir_all(sysroot.join("var/lib/bootc-rebase/pre-install-boot")).unwrap();
        fs::write(
            sysroot.join("var/lib/bootc-rebase/pre-install-boot/kernel"),
            "snapshot",
        )
        .unwrap();
        let cmdline = "root=UUID=x ostree=/ostree/boot.1/default/abc/0 rw".to_string();
        let facts = HostFacts {
            cmdline,
            proc_mounts: String::new(),
            verify_result: "OK\n".into(),
        };
        let layout = Layout {
            sysroot,
            live_var: live_var.clone(),
            deployment_var,
            live_etc,
            boot,
            live_home: live_var.join("home"),
        };
        (root, layout, facts, deployment.into())
    }

    #[test]
    fn deletion_allowlist_rejects_protected_and_traversal_paths() {
        for path in [
            "", "../boot", "/usr", "ostree", "boot", "home", "var/home", "root",
        ] {
            assert!(!is_deletable(path), "unexpectedly allowed {path}");
        }
        for path in [
            "usr",
            "etc",
            "var/cache",
            "var/lib/rpm",
            "var/lib/flatpak",
            "var/lib/bootc-rebase/package-rollback",
        ] {
            assert!(is_deletable(path), "unexpectedly refused {path}");
        }
    }

    #[test]
    fn dry_run_lists_only_proven_or_disposable_old_state() {
        let (_root, layout, facts, _) = fixture();
        let plan = plan(&layout, &facts, false).unwrap();
        assert!(plan.blocking(false).is_empty(), "{:#?}", plan.checks);
        let rels: Vec<_> = plan
            .targets
            .iter()
            .map(|target| target.rel.as_str())
            .collect();
        assert!(rels.contains(&"usr"));
        assert!(rels.contains(&"etc"));
        assert!(rels.contains(&"var/lib/rpm"));
        assert!(rels.contains(&"var/lib/flatpak"));
        assert!(rels.contains(&"var/log"));
        assert!(
            !rels
                .iter()
                .any(|rel| rel.starts_with("home") || *rel == "var/lib/private-app")
        );
        assert!(plan.total_bytes() > 0);
    }

    #[test]
    fn failed_health_and_missing_carry_are_hard_refusals() {
        let (_root, layout, mut facts, _) = fixture();
        facts.verify_result = "FINDINGS 1\n".into();
        fs::remove_file(layout.live_var.join("usrlocal/bin/tool")).unwrap();
        let plan = plan(&layout, &facts, false).unwrap();
        let failed: Vec<_> = plan.blocking(true).iter().map(|check| check.name).collect();
        assert!(failed.contains(&"first-boot verification is healthy"));
        assert!(failed.contains(&"/usr/local has a carried copy"));
    }

    #[test]
    fn execute_removes_old_os_and_rollback_but_keeps_unknown_data() {
        let (_root, layout, facts, _) = fixture();
        let plan = plan(&layout, &facts, false).unwrap();
        let expected = plan.total_bytes();
        let outcome = execute(&layout, &plan, "", false).unwrap();
        assert_eq!(outcome.reclaimed_bytes, expected);
        assert!(!layout.old("usr").exists());
        assert!(!layout.old("etc").exists());
        assert!(!layout.old("var/lib/rpm").exists());
        assert!(!layout.boot.join(ROLLBACK_ENTRY).exists());
        assert!(!layout.boot.join(ROLLBACK_DIR).exists());
        assert!(layout.old("home/alice/file").exists());
        assert!(layout.old("var/lib/private-app/data").exists());
        assert!(
            layout
                .live_var
                .join("lib/bootc-rebase")
                .join(COMMIT_RECORD_FILE)
                .is_file()
        );
    }

    #[test]
    fn late_mount_refusal_happens_before_any_mutation() {
        let (_root, layout, facts, _) = fixture();
        let plan = plan(&layout, &facts, false).unwrap();
        let mounts = format!(
            "/dev/loop0 {} ext4 rw 0 0\n",
            layout.old("usr/mounted").display()
        );
        let error = execute(&layout, &plan, &mounts, false).unwrap_err();
        assert!(error.to_string().contains("mounted inside"));
        assert!(layout.old("usr/local/bin/tool").exists());
        assert!(layout.boot.join(ROLLBACK_ENTRY).exists());
        assert!(
            !layout
                .live_var
                .join("lib/bootc-rebase")
                .join(COMMIT_RECORD_FILE)
                .exists()
        );
    }

    #[test]
    fn plain_home_move_is_opt_in_and_drops_only_managed_fstab_row() {
        let (_root, layout, mut facts, _) = fixture();
        fs::create_dir_all(&layout.live_home).unwrap();
        fs::write(
            layout.live_etc.join("fstab"),
            "UUID=root / ext4 defaults 0 1\n/sysroot/home /var/home none bind,x-systemd.requires-mounts-for=/sysroot/home 0 0\n",
        )
        .unwrap();
        facts.proc_mounts = format!(
            "{} {} none rw,bind 0 0\n",
            layout.old("home").display(),
            layout.live_home.display()
        );
        let plan = plan(&layout, &facts, true).unwrap();
        assert!(plan.blocking(false).is_empty(), "{:#?}", plan.checks);
        execute(&layout, &plan, "", false).unwrap();
        assert!(layout.deployment_var.join("home/alice/file").exists());
        let fstab = fs::read_to_string(layout.live_etc.join("fstab")).unwrap();
        assert!(fstab.contains("UUID=root"));
        assert!(!fstab.contains("/sysroot/home"));
    }

    #[test]
    fn grub_cleanup_preserves_unrelated_custom_entries() {
        let content = concat!(
            "menuentry 'Previous system' --id bootc-migrate-rollback {\n",
            "  linux /bootc-migrate-rollback/kernel root=UUID=old\n",
            "}\n",
            "menuentry 'Rescue' --id rescue {\n",
            "  linux /vmlinuz rescue\n",
            "}\n",
        );
        let updated = remove_grub_entry(content).unwrap();
        assert!(!updated.contains(ROLLBACK_ID));
        assert!(updated.contains("menuentry 'Rescue'"));
    }
}
