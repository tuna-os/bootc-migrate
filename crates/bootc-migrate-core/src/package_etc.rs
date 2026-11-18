//! `/etc` migration for package-managed source hosts (bootc-migrate#375).
//!
//! A package host has no image-provided factory `/etc`, so the normal three-way
//! merge cannot distinguish vendor defaults from administrator changes.  This
//! module uses the package database as that missing classifier, then feeds the
//! result through [`crate::cross_family::plan_etc`] instead of maintaining a
//! second path-policy table.
//!
//! Package-owned, unmodified paths take the target image's copy.  Modified,
//! unowned, and machine-state paths keep the live copy; when a modified path
//! also exists in the target, that target copy is retained as `.rebase-new`.
//! Identity databases use a narrower target-first merge: only human accounts
//! and their groups are brought over from the package host.

use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::process::Command;

use crate::cross_family::{self, EtcCrossFamilyPlan};
use crate::mergetc::{self, EtcDriftManifest, EtcPathState, IdentityMergePolicy, MergePolicy};
use crate::remap::{self, IdKind, RemapEntry, RemapPlan};
use crate::source_host::PackageDb;

/// Sidecar holding the target image's copy when a package-host edit wins.
pub const REBASE_NEW_SUFFIX: &str = ".rebase-new";

/// Package-database evidence used to classify live `/etc` paths.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackageEtcInventory {
    /// Package-owned paths, relative to `/etc`.
    pub owned: BTreeSet<String>,
    /// Package-owned paths whose verification differs from packaged metadata.
    pub modified: BTreeSet<String>,
    /// False when either ownership or verification could not be read.  An
    /// incomplete inventory never causes a live path to be discarded.
    pub complete: bool,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PackagePathClass {
    VendorDefault,
    UserModified,
    UserAdded,
    MachineState,
    Unknown,
}

impl PackageEtcInventory {
    /// Classify one live `/etc` path.  A database gap is fail-safe: keep it.
    pub fn classify(&self, path: &str) -> PackagePathClass {
        if self.complete && self.owned.contains(path) && self.modified.contains(path) {
            PackagePathClass::UserModified
        } else if cross_family::is_carried_over(path) {
            PackagePathClass::MachineState
        } else if !self.complete {
            PackagePathClass::Unknown
        } else if !self.owned.contains(path) {
            PackagePathClass::UserAdded
        } else {
            PackagePathClass::VendorDefault
        }
    }
}

fn etc_relative(path: &str) -> Option<String> {
    let path = path.trim().trim_matches('"');
    let rel = path
        .strip_prefix("/etc/")
        .or_else(|| path.strip_prefix("etc/"))?;
    let rel = rel.trim_end_matches('/');
    (!rel.is_empty()).then(|| rel.to_string())
}

fn etc_path_in_output(line: &str) -> Option<String> {
    let start = line.find("/etc/")?;
    let path = line[start..]
        .split_once(" (")
        .map_or(&line[start..], |(path, _)| path);
    etc_relative(path)
}

/// Parse `rpm -qal`.  Querying all package file lists once avoids one `rpm
/// -qf` process per `/etc` path while answering the same ownership question.
pub fn parse_rpm_owned(output: &str) -> BTreeSet<String> {
    output.lines().filter_map(etc_relative).collect()
}

/// Parse `rpm -Va --nomtime --nosize --nouser --nogroup` output.  rpm emits
/// only files whose remaining verification attributes differ; restrict those
/// findings to config (`c`) paths below `/etc`.
pub fn parse_rpm_verify(output: &str) -> BTreeSet<String> {
    output
        .lines()
        .filter(|line| {
            let before_path = line.split("/etc/").next().unwrap_or(line);
            before_path.split_whitespace().any(|field| field == "c")
        })
        .filter_map(etc_path_in_output)
        .collect()
}

/// Parse `dpkg-query --search '/etc/*'` output (`package: /etc/path`).
pub fn parse_dpkg_owned(output: &str) -> BTreeSet<String> {
    output
        .lines()
        .filter_map(|line| line.split_once(": ").map(|(_, path)| path))
        .filter_map(etc_relative)
        .collect()
}

/// Parse `dpkg --verify` output.  dpkg prints only mismatches; the final path
/// begins at `/etc/`, after its verification flags and optional `c` marker.
pub fn parse_dpkg_verify(output: &str) -> BTreeSet<String> {
    output.lines().filter_map(etc_path_in_output).collect()
}

/// Parse `pacman -Qlq`, whose file list is absolute.
pub fn parse_pacman_owned(output: &str) -> BTreeSet<String> {
    output.lines().filter_map(etc_relative).collect()
}

/// Parse `pacman -Qkk` warnings such as
/// `pkg: /etc/foo (Modification time mismatch)`.
pub fn parse_pacman_verify(output: &str) -> BTreeSet<String> {
    output.lines().filter_map(etc_path_in_output).collect()
}

fn command_text(
    program: &str,
    args: &[&str],
    difference_status_ok: bool,
    include_stderr: bool,
) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .with_context(|| format!("failed to execute {program} {}", args.join(" ")))?;
    let status_ok = output.status.success()
        || (difference_status_ok && output.status.code().is_some_and(|code| code == 1));
    if !status_ok {
        bail!(
            "{program} {} exited {}: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    if include_stderr {
        text.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    Ok(text)
}

/// Read ownership and verification evidence from the live package database.
/// Failures are returned as an incomplete inventory rather than an empty,
/// authoritative one: callers consequently carry every uncertain live path.
pub fn gather_live(db: PackageDb) -> PackageEtcInventory {
    let mut inventory = PackageEtcInventory {
        complete: true,
        ..Default::default()
    };
    let (owned, verified) = match db {
        PackageDb::Rpm => (
            command_text("rpm", &["-qal"], false, false).map(|s| parse_rpm_owned(&s)),
            command_text(
                "rpm",
                &["-Va", "--nomtime", "--nosize", "--nouser", "--nogroup"],
                true,
                true,
            )
            .map(|s| parse_rpm_verify(&s)),
        ),
        PackageDb::Dpkg => (
            command_text("dpkg-query", &["--search", "/etc/*"], false, false)
                .map(|s| parse_dpkg_owned(&s)),
            command_text("dpkg", &["--verify"], true, true).map(|s| parse_dpkg_verify(&s)),
        ),
        PackageDb::Pacman => (
            command_text("pacman", &["-Qlq"], false, false).map(|s| parse_pacman_owned(&s)),
            command_text("pacman", &["-Qkk"], true, true).map(|s| parse_pacman_verify(&s)),
        ),
    };
    match owned {
        Ok(paths) => inventory.owned = paths,
        Err(error) => {
            inventory.complete = false;
            inventory
                .warnings
                .push(format!("package ownership could not be read: {error:#}"));
        }
    }
    match verified {
        Ok(paths) => inventory.modified = paths,
        Err(error) => {
            inventory.complete = false;
            inventory
                .warnings
                .push(format!("package verification could not be read: {error:#}"));
        }
    }
    inventory
}

/// Package-host `/etc` plan.  `policy` is the shared cross-family plan over
/// classifier-adjusted states; `rebase_new` adds the package-host-specific
/// inverse sidecar where the live edit wins.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PackageEtcPlan {
    pub policy: EtcCrossFamilyPlan,
    pub classes: BTreeMap<String, PackagePathClass>,
    pub rebase_new: Vec<String>,
    /// User-removed packaged paths.  The deletion is retained and the
    /// target's version is available only as `.rebase-new`.
    pub removed: Vec<String>,
}

impl PackageEtcPlan {
    fn overrides(&self) -> EtcDriftManifest {
        let mut manifest = self.policy.overrides();
        manifest
            .decisions
            .extend(self.policy.carried.iter().map(|path| (path.clone(), true)));
        manifest
            .decisions
            .extend(self.removed.iter().map(|path| (path.clone(), true)));
        manifest
    }
}

/// Reuse [`cross_family::plan_etc`] after translating package evidence into
/// its "factory path + user-modified" vocabulary.
pub fn plan_etc(states: &[EtcPathState], inventory: &PackageEtcInventory) -> PackageEtcPlan {
    let mut adjusted = Vec::with_capacity(states.len());
    let mut classes = BTreeMap::new();
    let mut rebase_new = Vec::new();
    let mut removed = Vec::new();

    for state in states {
        let mut state = state.clone();
        if state.in_current && !mergetc::is_identity_db(&state.path) {
            let class = inventory.classify(&state.path);
            classes.insert(state.path.clone(), class);
            match class {
                PackagePathClass::VendorDefault => {
                    state.in_source_default = true;
                    state.user_modified = false;
                }
                PackagePathClass::UserModified
                | PackagePathClass::UserAdded
                | PackagePathClass::Unknown => {
                    if state.in_target_default {
                        rebase_new.push(state.path.clone());
                    }
                    // Make the shared planner see a user-only path, so it is
                    // carried.  The real target presence remains recorded by
                    // `rebase_new` for the sidecar copy.
                    state.in_source_default = false;
                    state.in_target_default = false;
                    state.user_modified = true;
                }
                PackagePathClass::MachineState => {}
            }
        } else if !mergetc::is_identity_db(&state.path)
            && inventory.complete
            && inventory.owned.contains(&state.path)
            && inventory.modified.contains(&state.path)
        {
            classes.insert(state.path.clone(), PackagePathClass::UserModified);
            if state.in_target_default {
                rebase_new.push(state.path.clone());
                removed.push(state.path.clone());
                state.in_target_default = false;
                state.user_modified = true;
            }
        }
        adjusted.push(state);
    }

    rebase_new.sort();
    rebase_new.dedup();
    removed.sort();
    removed.dedup();
    PackageEtcPlan {
        policy: cross_family::plan_etc(&adjusted),
        classes,
        rebase_new,
        removed,
    }
}

pub fn render_plan(plan: &PackageEtcPlan, inventory: &PackageEtcInventory) -> String {
    let count = |class| plan.classes.values().filter(|&&c| c == class).count();
    let mut out = String::from("=== Package-host /etc migration report ===\n");
    let _ = writeln!(
        out,
        "  {} packaged vendor-default path(s) replaced by target defaults",
        count(PackagePathClass::VendorDefault)
    );
    let _ = writeln!(
        out,
        "  {} packaged user-modified path(s) carried",
        count(PackagePathClass::UserModified)
    );
    let _ = writeln!(
        out,
        "  {} unpackaged path(s) carried",
        count(PackagePathClass::UserAdded)
    );
    let _ = writeln!(
        out,
        "  {} machine-state path(s) carried",
        count(PackagePathClass::MachineState)
    );
    if !plan.rebase_new.is_empty() {
        let _ = writeln!(
            out,
            "  {} carried path(s) also ship in the target; target copies kept as <path>{REBASE_NEW_SUFFIX}:",
            plan.rebase_new.len()
        );
        for path in &plan.rebase_new {
            let _ = writeln!(out, "    /etc/{path}");
        }
    }
    for warning in &inventory.warnings {
        let _ = writeln!(out, "  warning: {warning}");
    }
    out
}

/// `UID_MIN` from `login.defs`; shadow-utils defaults to 1000 when absent.
pub fn parse_uid_min(login_defs: &str) -> u32 {
    login_defs
        .lines()
        .filter_map(|line| {
            let line = line.split('#').next()?.trim();
            let mut fields = line.split_whitespace();
            (fields.next()? == "UID_MIN").then(|| fields.next()?.parse::<u32>().ok())?
        })
        .next()
        .unwrap_or(1000)
}

#[derive(Debug, Clone)]
struct PasswdLine {
    name: String,
    fields: Vec<String>,
    uid: u32,
    gid: u32,
}

fn parse_passwd_lines(text: &str) -> Vec<PasswdLine> {
    text.lines()
        .filter_map(|line| {
            let fields: Vec<String> = line.split(':').map(str::to_string).collect();
            if fields.len() < 7 {
                return None;
            }
            Some(PasswdLine {
                name: fields[0].clone(),
                uid: fields[2].parse().ok()?,
                gid: fields[3].parse().ok()?,
                fields,
            })
        })
        .collect()
}

#[derive(Debug, Clone)]
struct GroupLine {
    name: String,
    fields: Vec<String>,
    gid: u32,
}

impl GroupLine {
    fn members(&self) -> impl Iterator<Item = &str> {
        self.fields
            .get(3)
            .into_iter()
            .flat_map(|members| members.split(','))
            .filter(|member| !member.is_empty())
    }
}

fn parse_group_lines(text: &str) -> Vec<GroupLine> {
    text.lines()
        .filter_map(|line| {
            let fields: Vec<String> = line.split(':').map(str::to_string).collect();
            if fields.len() < 4 {
                return None;
            }
            Some(GroupLine {
                name: fields[0].clone(),
                gid: fields[2].parse().ok()?,
                fields,
            })
        })
        .collect()
}

fn line_key(line: &str) -> &str {
    line.split(':').next().unwrap_or("")
}

fn append_named_lines(target: &str, source: &str, names: &BTreeSet<String>) -> String {
    let mut present: BTreeSet<&str> = target
        .lines()
        .map(line_key)
        .filter(|key| !key.is_empty())
        .collect();
    let mut out = String::with_capacity(target.len() + source.len());
    for line in target.lines() {
        out.push_str(line);
        out.push('\n');
    }
    for line in source.lines() {
        let key = line_key(line);
        if names.contains(key) && present.insert(key) {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn next_free_id(used: &mut BTreeSet<u32>, minimum: u32) -> u32 {
    let mut candidate = minimum;
    while used.contains(&candidate) {
        candidate = candidate.saturating_add(1);
    }
    used.insert(candidate);
    candidate
}

fn record_mapping(mappings: &mut BTreeMap<u32, u32>, old: u32, new: u32, kind: &str) -> Result<()> {
    if let Some(previous) = mappings.insert(old, new)
        && previous != new
    {
        bail!("source {kind} {old} would map to both {previous} and {new}");
    }
    Ok(())
}

fn merge_member_field(target: &mut String, source: &str, human_names: &BTreeSet<String>) {
    let mut target_fields: Vec<String> = target.split(':').map(str::to_string).collect();
    let source_fields: Vec<&str> = source.split(':').collect();
    if target_fields.len() < 4 || source_fields.len() < 4 {
        return;
    }
    let mut members: BTreeSet<String> = target_fields[3]
        .split(',')
        .filter(|member| !member.is_empty())
        .map(str::to_string)
        .collect();
    for member in source_fields[3]
        .split(',')
        .filter(|member| human_names.contains(*member))
    {
        members.insert(member.to_string());
    }
    target_fields[3] = members.into_iter().collect::<Vec<_>>().join(",");
    *target = target_fields.join(":");
}

/// Result of target-first identity merging.  `remap` applies only to carried
/// source data; target-owned files already have target ids.
#[derive(Debug, Clone)]
pub struct IdentityMerge {
    pub passwd: String,
    pub group: String,
    pub shadow: String,
    pub gshadow: String,
    pub remap: RemapPlan,
    pub human_users: Vec<String>,
    pub carried_groups: Vec<String>,
}

/// Merge the target identity DBs first, then append source human users and the
/// primary/supplementary groups needed by those users.  Source system accounts
/// never cross the boundary.  Numeric collisions are allocated fresh ids and
/// returned as a cycle-safe ownership remap.
#[allow(clippy::too_many_arguments)]
pub fn merge_identity(
    source_passwd: &str,
    source_group: &str,
    source_shadow: &str,
    source_gshadow: &str,
    target_passwd: &str,
    target_group: &str,
    target_shadow: &str,
    target_gshadow: &str,
    login_defs: &str,
) -> Result<IdentityMerge> {
    let uid_min = parse_uid_min(login_defs);
    let source_users: Vec<PasswdLine> = parse_passwd_lines(source_passwd)
        .into_iter()
        .filter(|user| user.uid >= uid_min)
        .collect();
    let human_names: BTreeSet<String> = source_users.iter().map(|u| u.name.clone()).collect();
    let target_users = parse_passwd_lines(target_passwd);
    if target_users.is_empty() {
        bail!("target passwd contains no valid accounts");
    }
    let target_user_names: BTreeSet<String> = target_users.iter().map(|u| u.name.clone()).collect();
    if let Some(name) = human_names.intersection(&target_user_names).next() {
        bail!(
            "source human account `{name}` already exists in the target image; refusing to choose one password or home implicitly"
        );
    }

    let source_groups = parse_group_lines(source_group);
    let target_groups = parse_group_lines(target_group);
    if target_groups.is_empty() {
        bail!("target group contains no valid groups");
    }
    let primary_gids: BTreeSet<u32> = source_users.iter().map(|u| u.gid).collect();
    let selected_groups: Vec<GroupLine> = source_groups
        .into_iter()
        .filter(|group| {
            primary_gids.contains(&group.gid)
                || group.members().any(|member| human_names.contains(member))
        })
        .collect();

    let mut used_uids: BTreeSet<u32> = target_users
        .iter()
        .map(|u| u.uid)
        .chain(source_users.iter().map(|u| u.uid))
        .collect();
    let mut used_gids: BTreeSet<u32> = target_groups
        .iter()
        .map(|g| g.gid)
        .chain(selected_groups.iter().map(|g| g.gid))
        .chain(primary_gids.iter().copied())
        .collect();
    let target_uid_owner: BTreeMap<u32, &str> = target_users
        .iter()
        .map(|u| (u.uid, u.name.as_str()))
        .collect();
    let target_gid_owner: BTreeMap<u32, &str> = target_groups
        .iter()
        .map(|g| (g.gid, g.name.as_str()))
        .collect();

    let mut uid_mapping = BTreeMap::new();
    let mut gid_mapping = BTreeMap::new();
    let mut remaps = Vec::new();

    for user in &source_users {
        let chosen = if target_uid_owner.contains_key(&user.uid) {
            next_free_id(&mut used_uids, uid_min)
        } else {
            user.uid
        };
        if chosen != user.uid {
            record_mapping(&mut uid_mapping, user.uid, chosen, "uid")?;
            remaps.push(RemapEntry {
                name: user.name.clone(),
                kind: IdKind::Uid,
                old_id: user.uid,
                new_id: chosen,
            });
        }
    }

    let target_group_by_name: BTreeMap<String, u32> = target_groups
        .iter()
        .map(|g| (g.name.clone(), g.gid))
        .collect();
    let mut carried_group_names = Vec::new();
    for group in &selected_groups {
        let chosen = if let Some(&target_gid) = target_group_by_name.get(&group.name) {
            target_gid
        } else if target_gid_owner.contains_key(&group.gid) {
            next_free_id(&mut used_gids, uid_min)
        } else {
            group.gid
        };
        record_mapping(&mut gid_mapping, group.gid, chosen, "gid")?;
        if chosen != group.gid {
            remaps.push(RemapEntry {
                name: group.name.clone(),
                kind: IdKind::Gid,
                old_id: group.gid,
                new_id: chosen,
            });
        }
        carried_group_names.push(group.name.clone());
    }
    // A primary gid can be absent from /etc/group.  It still needs a free id
    // if the target has assigned that number to another group.
    for &gid in &primary_gids {
        if gid_mapping.contains_key(&gid) {
            continue;
        }
        let chosen = if target_gid_owner.contains_key(&gid) {
            next_free_id(&mut used_gids, uid_min)
        } else {
            gid
        };
        record_mapping(&mut gid_mapping, gid, chosen, "gid")?;
        if chosen != gid {
            remaps.push(RemapEntry {
                name: format!("primary-gid-{gid}"),
                kind: IdKind::Gid,
                old_id: gid,
                new_id: chosen,
            });
        }
    }

    let mut passwd = String::with_capacity(target_passwd.len() + source_passwd.len());
    for line in target_passwd.lines() {
        passwd.push_str(line);
        passwd.push('\n');
    }
    for mut user in source_users.clone() {
        user.fields[2] = uid_mapping
            .get(&user.uid)
            .copied()
            .unwrap_or(user.uid)
            .to_string();
        user.fields[3] = gid_mapping
            .get(&user.gid)
            .copied()
            .unwrap_or(user.gid)
            .to_string();
        passwd.push_str(&user.fields.join(":"));
        passwd.push('\n');
    }

    let mut group_lines: Vec<String> = target_group.lines().map(str::to_string).collect();
    let target_group_index: BTreeMap<String, usize> = target_groups
        .iter()
        .enumerate()
        .map(|(index, group)| (group.name.clone(), index))
        .collect();
    for mut group in selected_groups.clone() {
        if let Some(&index) = target_group_index.get(&group.name) {
            merge_member_field(
                &mut group_lines[index],
                &group.fields.join(":"),
                &human_names,
            );
        } else {
            group.fields[2] = gid_mapping
                .get(&group.gid)
                .copied()
                .unwrap_or(group.gid)
                .to_string();
            group_lines.push(group.fields.join(":"));
        }
    }
    let group = group_lines.join("\n") + "\n";

    let shadow = append_named_lines(target_shadow, source_shadow, &human_names);
    let carried_group_set: BTreeSet<String> = carried_group_names.iter().cloned().collect();
    let mut gshadow_lines: Vec<String> = target_gshadow.lines().map(str::to_string).collect();
    let gshadow_index: BTreeMap<String, usize> = gshadow_lines
        .iter()
        .enumerate()
        .map(|(index, line)| (line_key(line).to_string(), index))
        .collect();
    for source_line in source_gshadow.lines() {
        let name = line_key(source_line);
        if !carried_group_set.contains(name) {
            continue;
        }
        if let Some(&index) = gshadow_index.get(name) {
            merge_member_field(&mut gshadow_lines[index], source_line, &human_names);
        } else {
            gshadow_lines.push(source_line.to_string());
        }
    }
    let gshadow = gshadow_lines.join("\n") + "\n";

    let remap = remap::plan_explicit(remaps, used_uids.into_iter().chain(used_gids));
    Ok(IdentityMerge {
        passwd,
        group,
        shadow,
        gshadow,
        remap,
        human_users: human_names.into_iter().collect(),
        carried_groups: carried_group_names,
    })
}

fn copy_target_sidecar(target_etc: &Path, output_etc: &Path, rel: &str) -> Result<()> {
    let source = target_etc.join(rel);
    let destination = output_etc.join(format!("{rel}{REBASE_NEW_SUFFIX}"));
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let metadata = fs::symlink_metadata(&source).with_context(|| {
        format!(
            "failed to stat target /etc sidecar source {}",
            source.display()
        )
    })?;
    if destination.exists() || destination.is_symlink() {
        fs::remove_file(&destination)?;
    }
    if metadata.file_type().is_symlink() {
        let target = fs::read_link(&source)?;
        std::os::unix::fs::symlink(target, &destination)?;
    } else if metadata.is_file() {
        crate::xattr::copy_file_with_xattrs(&source, &destination)?;
    }
    Ok(())
}

fn clear_dir_contents(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir)?;
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.symlink_metadata()?.is_dir() {
            fs::remove_dir_all(path)?;
        } else {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn read_required(path: &Path, description: &str) -> Result<String> {
    fs::read_to_string(path)
        .with_context(|| format!("failed to read {description} at {}", path.display()))
}

fn read_optional(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

/// Complete package-host `/etc` application over caller-supplied trees.  This
/// is the seam `Strategy::PackageInstall` calls after bootc stages the target.
pub fn merge_etc(
    current_etc: &Path,
    target_etc: &Path,
    output_etc: &Path,
    inventory: &PackageEtcInventory,
) -> Result<(PackageEtcPlan, IdentityMerge)> {
    let empty = tempfile::tempdir()?;
    let states = mergetc::etc_path_states(empty.path(), current_etc, target_etc)?;
    let plan = plan_etc(&states, inventory);
    let overrides = plan.overrides();

    // Validate and plan identity before clearing the staged tree.  A missing
    // password database must not leave a half-written deployment `/etc`.
    let source_passwd = read_required(&current_etc.join("passwd"), "source passwd")?;
    let source_group = read_required(&current_etc.join("group"), "source group")?;
    let source_shadow = read_required(&current_etc.join("shadow"), "source shadow")?;
    let source_gshadow = read_required(&current_etc.join("gshadow"), "source gshadow")?;
    let target_passwd = read_required(&target_etc.join("passwd"), "target passwd")?;
    let target_group = read_required(&target_etc.join("group"), "target group")?;
    let target_shadow = read_required(&target_etc.join("shadow"), "target shadow")?;
    let target_gshadow = read_required(&target_etc.join("gshadow"), "target gshadow")?;
    let login_defs = read_optional(&current_etc.join("login.defs"));
    let identity = merge_identity(
        &source_passwd,
        &source_group,
        &source_shadow,
        &source_gshadow,
        &target_passwd,
        &target_group,
        &target_shadow,
        &target_gshadow,
        &login_defs,
    )?;

    clear_dir_contents(output_etc)?;
    mergetc::merge_etc_files_with_policy(
        empty.path(),
        current_etc,
        target_etc,
        output_etc,
        &MergePolicy {
            overrides: Some(&overrides),
            identity: IdentityMergePolicy::TargetFirst,
        },
    )?;
    for path in &plan.rebase_new {
        copy_target_sidecar(target_etc, output_etc, path)?;
    }

    for (name, content) in [
        ("passwd", &identity.passwd),
        ("group", &identity.group),
        ("shadow", &identity.shadow),
        ("gshadow", &identity.gshadow),
    ] {
        fs::write(output_etc.join(name), content)?;
    }

    let fstab = output_etc.join("fstab");
    if let Ok(contents) = fs::read_to_string(&fstab) {
        fs::write(&fstab, rewrite_fstab(&contents))?;
    }
    Ok((plan, identity))
}

/// Remove the package host's root mount, move `/home` to `/var/home`, and keep
/// every other mount unchanged.  Comments, options, and field spacing survive.
pub fn rewrite_fstab(contents: &str) -> String {
    let mut out = String::with_capacity(contents.len());
    for line in contents.split_inclusive('\n') {
        let body = line.strip_suffix('\n').unwrap_or(line);
        let mut fields = body.match_indices(|c: char| !c.is_whitespace());
        let Some((first_start, _)) = fields.next() else {
            out.push_str(line);
            continue;
        };
        if body[first_start..].starts_with('#') {
            out.push_str(line);
            continue;
        }
        let first_end = body[first_start..]
            .find(char::is_whitespace)
            .map_or(body.len(), |offset| first_start + offset);
        let Some(second_start_rel) = body[first_end..].find(|c: char| !c.is_whitespace()) else {
            out.push_str(line);
            continue;
        };
        let second_start = first_end + second_start_rel;
        let second_end = body[second_start..]
            .find(char::is_whitespace)
            .map_or(body.len(), |offset| second_start + offset);
        match &body[second_start..second_end] {
            "/" => continue,
            "/home" => {
                out.push_str(&body[..second_start]);
                out.push_str("/var/home");
                out.push_str(&body[second_end..]);
                if line.ends_with('\n') {
                    out.push('\n');
                }
            }
            _ => out.push_str(line),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const RPM_OWNED: &str = include_str!("../testdata/package-etc/rpm-owned.txt");
    const RPM_VERIFY: &str = include_str!("../testdata/package-etc/rpm-verify.txt");
    const DPKG_OWNED: &str = include_str!("../testdata/package-etc/dpkg-owned.txt");
    const DPKG_VERIFY: &str = include_str!("../testdata/package-etc/dpkg-verify.txt");

    #[test]
    fn package_output_parsers_table() {
        let cases: &[(&str, BTreeSet<String>, BTreeSet<String>)] = &[
            (
                "rpm",
                parse_rpm_owned(RPM_OWNED),
                parse_rpm_verify(RPM_VERIFY),
            ),
            (
                "dpkg",
                parse_dpkg_owned(DPKG_OWNED),
                parse_dpkg_verify(DPKG_VERIFY),
            ),
        ];
        for (label, owned, modified) in cases {
            assert_eq!(
                owned,
                &BTreeSet::from([
                    "NetworkManager/system-connections/office.nmconnection".into(),
                    "nsswitch.conf".into(),
                    "pam.d/system-auth".into(),
                    "removed.conf".into(),
                    "ssh/sshd_config.d/10-admin.conf".into(),
                ]),
                "{label} owned"
            );
            assert_eq!(
                modified,
                &BTreeSet::from([
                    "removed.conf".into(),
                    "ssh/sshd_config.d/10-admin.conf".into(),
                ]),
                "{label} modified"
            );
        }
    }

    #[test]
    fn pacman_verify_parser_strips_diagnostic_suffix() {
        let output = "warning: openssh: /etc/ssh/sshd_config (Modification time mismatch)\n\
                      openssh: 42 total files, 1 altered file\n";
        assert_eq!(
            parse_pacman_verify(output),
            BTreeSet::from(["ssh/sshd_config".into()])
        );
    }

    #[test]
    fn package_plan_reuses_cross_family_shape() {
        let inventory = PackageEtcInventory {
            owned: BTreeSet::from([
                "nsswitch.conf".into(),
                "pam.d/system-auth".into(),
                "ssh/sshd_config.d/10-admin.conf".into(),
            ]),
            modified: BTreeSet::from(["ssh/sshd_config.d/10-admin.conf".into()]),
            complete: true,
            warnings: vec![],
        };
        let states = vec![
            EtcPathState {
                path: "nsswitch.conf".into(),
                in_source_default: false,
                in_current: true,
                in_target_default: true,
                user_modified: true,
            },
            EtcPathState {
                path: "pam.d/system-auth".into(),
                in_source_default: false,
                in_current: true,
                in_target_default: true,
                user_modified: true,
            },
            EtcPathState {
                path: "ssh/sshd_config.d/10-admin.conf".into(),
                in_source_default: false,
                in_current: true,
                in_target_default: true,
                user_modified: true,
            },
            EtcPathState {
                path: "myapp.conf".into(),
                in_source_default: false,
                in_current: true,
                in_target_default: false,
                user_modified: true,
            },
        ];
        let plan = plan_etc(&states, &inventory);
        assert_eq!(
            plan.policy.take_target,
            vec!["nsswitch.conf", "pam.d/system-auth"]
        );
        assert_eq!(
            plan.policy.carried,
            vec!["myapp.conf", "ssh/sshd_config.d/10-admin.conf"]
        );
        assert_eq!(plan.rebase_new, vec!["ssh/sshd_config.d/10-admin.conf"]);
    }

    #[test]
    fn merge_keeps_edits_and_machine_state_but_target_policy_wins() {
        let temp = tempfile::tempdir().unwrap();
        let current = temp.path().join("current");
        let target = temp.path().join("target");
        let output = temp.path().join("output");
        for root in [&current, &target] {
            fs::create_dir_all(root.join("pam.d")).unwrap();
            fs::create_dir_all(root.join("ssh/sshd_config.d")).unwrap();
            fs::create_dir_all(root.join("NetworkManager/system-connections")).unwrap();
        }
        fs::write(current.join("nsswitch.conf"), "host nsswitch\n").unwrap();
        fs::write(target.join("nsswitch.conf"), "target nsswitch\n").unwrap();
        fs::write(current.join("pam.d/system-auth"), "host pam\n").unwrap();
        fs::write(target.join("pam.d/system-auth"), "target pam\n").unwrap();
        fs::write(
            current.join("ssh/sshd_config.d/10-admin.conf"),
            "PasswordAuthentication yes\n",
        )
        .unwrap();
        fs::write(
            target.join("ssh/sshd_config.d/10-admin.conf"),
            "PasswordAuthentication no\n",
        )
        .unwrap();
        fs::write(target.join("removed.conf"), "target restored default\n").unwrap();
        fs::write(
            current.join("NetworkManager/system-connections/office.nmconnection"),
            "host network\n",
        )
        .unwrap();
        fs::write(current.join("myapp.conf"), "user app\n").unwrap();
        for root in [&current, &target] {
            fs::write(root.join("passwd"), "root:x:0:0:root:/root:/bin/bash\n").unwrap();
            fs::write(root.join("group"), "root:x:0:\n").unwrap();
            fs::write(root.join("shadow"), "root:!:1:0:99999:7:::\n").unwrap();
            fs::write(root.join("gshadow"), "root:!::\n").unwrap();
        }
        fs::write(current.join("login.defs"), "UID_MIN 1000\n").unwrap();
        let inventory = PackageEtcInventory {
            owned: BTreeSet::from([
                "nsswitch.conf".into(),
                "pam.d/system-auth".into(),
                "ssh/sshd_config.d/10-admin.conf".into(),
                "removed.conf".into(),
            ]),
            modified: BTreeSet::from([
                "removed.conf".into(),
                "ssh/sshd_config.d/10-admin.conf".into(),
            ]),
            complete: true,
            warnings: vec![],
        };

        let (plan, _) = merge_etc(&current, &target, &output, &inventory).unwrap();
        assert_eq!(
            fs::read_to_string(output.join("nsswitch.conf")).unwrap(),
            "target nsswitch\n"
        );
        assert_eq!(
            fs::read_to_string(output.join("pam.d/system-auth")).unwrap(),
            "target pam\n"
        );
        assert_eq!(
            fs::read_to_string(output.join("ssh/sshd_config.d/10-admin.conf")).unwrap(),
            "PasswordAuthentication yes\n"
        );
        assert_eq!(
            fs::read_to_string(output.join("ssh/sshd_config.d/10-admin.conf.rebase-new")).unwrap(),
            "PasswordAuthentication no\n"
        );
        assert!(!output.join("removed.conf").exists());
        assert_eq!(
            fs::read_to_string(output.join("removed.conf.rebase-new")).unwrap(),
            "target restored default\n"
        );
        assert_eq!(
            fs::read_to_string(
                output.join("NetworkManager/system-connections/office.nmconnection")
            )
            .unwrap(),
            "host network\n"
        );
        assert_eq!(
            fs::read_to_string(output.join("myapp.conf")).unwrap(),
            "user app\n"
        );
        assert!(render_plan(&plan, &inventory).contains(".rebase-new"));
    }

    #[test]
    fn identity_merge_carries_only_humans_and_remaps_collisions() {
        let merged = merge_identity(
            "root:x:0:0:root:/root:/bin/bash\ndaemon:x:2:2::/:/usr/sbin/nologin\nalice:x:1000:1000:Alice:/home/alice:/bin/bash\n",
            "root:x:0:\ndaemon:x:2:\nalice:x:1000:\nwheel:x:10:alice\n",
            "root:$6$root:1:0:99999:7:::\ndaemon:!:1:0:99999:7:::\nalice:$6$alicehash:1:0:99999:7:::\n",
            "root:!::\ndaemon:!::\nalice:!::\nwheel:!::alice\n",
            "root:x:0:0:root:/root:/bin/bash\nfirstboot:x:1000:1000::/:/usr/sbin/nologin\nchrony:x:2:2::/:/usr/sbin/nologin\n",
            "root:x:0:\nfirstboot:x:1000:\nwheel:x:10:\nchrony:x:2:\n",
            "root:!:1:0:99999:7:::\nfirstboot:!:1:0:99999:7:::\nchrony:!:1:0:99999:7:::\n",
            "root:!::\nfirstboot:!::\nwheel:!::\nchrony:!::\n",
            "UID_MIN 1000\n",
        )
        .unwrap();

        assert!(merged.passwd.starts_with("root:x:0:0:"));
        assert!(merged.passwd.contains("chrony:x:2:2:"));
        assert!(!merged.passwd.contains("daemon:x:2:2:"));
        assert!(
            merged
                .passwd
                .contains("alice:x:1001:1001:Alice:/home/alice:/bin/bash")
        );
        assert!(merged.shadow.contains("alice:$6$alicehash:"));
        assert!(merged.group.contains("alice:x:1001:"));
        assert!(merged.group.contains("wheel:x:10:alice"));
        assert_eq!(merged.human_users, vec!["alice"]);
        assert_eq!(merged.carried_groups, vec!["alice", "wheel"]);
        assert!(merged.remap.remaps.iter().any(|entry| {
            entry.kind == IdKind::Uid && entry.old_id == 1000 && entry.new_id == 1001
        }));
        assert!(merged.remap.remaps.iter().any(|entry| {
            entry.kind == IdKind::Gid && entry.old_id == 1000 && entry.new_id == 1001
        }));
        assert_eq!(merged.remap.steps.len(), 4);
    }

    #[test]
    fn fstab_rewrite_drops_root_moves_home_and_keeps_other_mounts() {
        let input = "# host mounts\nUUID=root / btrfs subvol=root 0 0\nUUID=home   /home   btrfs subvol=home 0 0\nUUID=boot /boot ext4 defaults 0 2\nserver:/srv /srv nfs defaults 0 0\n";
        assert_eq!(
            rewrite_fstab(input),
            "# host mounts\nUUID=home   /var/home   btrfs subvol=home 0 0\nUUID=boot /boot ext4 defaults 0 2\nserver:/srv /srv nfs defaults 0 0\n"
        );
    }

    #[test]
    fn incomplete_inventory_carries_uncertain_paths() {
        let inventory = PackageEtcInventory {
            complete: false,
            warnings: vec!["rpm failed".into()],
            ..Default::default()
        };
        assert_eq!(
            inventory.classify("pam.d/system-auth"),
            PackagePathClass::Unknown
        );
        assert_eq!(inventory.classify("fstab"), PackagePathClass::MachineState);
    }
}
