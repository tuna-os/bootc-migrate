//! First-boot auto-repair (L3): fix the known migration breakage classes on
//! the migrated system itself, instead of only reporting them.
//!
//! Staging ([`stage_repair`]) writes four things into the new deployment:
//!
//! - an identity manifest of the live `/var/home` (owner, mode and mtime of
//!   every entry), the repair source for ownership/mtime drift on carried
//!   trees (#308) — the repair restores identity from it, never re-copies;
//! - the running binary, a oneshot unit that runs `repair --firstboot` once,
//!   and the marker that arms it (it holds the source machine-id on routes
//!   where a carried machine-id is breakage);
//! - a desktop autostart entry that shows the repair result once per user.
//!
//! On first boot the unit runs after the L1 verify probe (when one is staged)
//! and before user sessions start. Each repair class is allowlisted, logged
//! and independently skippable; verify findings with no known-safe fix are
//! reported, never touched. A repair never deletes a deployment, an ESP
//! artifact or a snapshot. When the L1 probe is present it runs again after
//! the repairs, and the before/after reports are kept next to the repair log
//! in `/var/lib/bootc-migrate/`.
//!
//! Opt-out: `bootc-migrate repair --disable` before the reboot, the kernel
//! argument `bootc_migrate.repair=0`, or skip single classes with
//! `bootc_migrate.repair.skip=<class>[,<class>...]` or one class per line in
//! `/etc/bootc-migrate/repair.skip`.
//!
//! Parsing and planning are pure and table-tested; the I/O takes a root
//! path, so the unit tests run every class against fixture roots.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::io::{BufRead, BufWriter, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

/// The oneshot unit's name in the staged `/etc/systemd/system`.
pub const REPAIR_UNIT: &str = "bootc-migrate-repair-firstboot.service";
/// The marker (relative to `/etc`) that arms the unit. Its content is a
/// JSON [`RepairConfig`]. The unit removes it as its last step.
pub const REPAIR_MARKER: &str = "bootc-migrate/repair-firstboot";
/// The staged copy of the migrating binary (relative to `/etc`). The target
/// image does not ship bootc-migrate, so the unit runs this copy; the unit
/// removes it with the marker.
pub const REPAIR_BIN_REL: &str = "bootc-migrate/bootc-migrate-repair";
/// Present (relative to `/etc`): the unit does not run.
pub const REPAIR_DISABLED_REL: &str = "bootc-migrate/repair.disabled";
/// Classes to skip (relative to `/etc`), one class name per line.
pub const REPAIR_SKIP_REL: &str = "bootc-migrate/repair.skip";
/// Kernel argument that disables the unit.
pub const CMDLINE_DISABLE: &str = "bootc_migrate.repair=0";
/// Kernel argument prefix that skips classes: `<prefix><class>,<class>`.
pub const CMDLINE_SKIP_PREFIX: &str = "bootc_migrate.repair.skip=";

/// State directory, relative to the `/var` root.
pub const STATE_DIR_REL: &str = "lib/bootc-migrate";
/// The identity manifest, in [`STATE_DIR_REL`].
pub const IDENTITY_MANIFEST: &str = "identity-manifest.tsv";
/// The full repair log, in [`STATE_DIR_REL`].
pub const REPAIR_LOG: &str = "repair-log.json";
/// One-line summary, in [`STATE_DIR_REL`]: `CLEAN`, or
/// `REPAIRED <n> FAILED <n> REPORTED <n>`.
pub const REPAIR_RESULT: &str = "repair-result";
/// Copies of the L1 verify report before and after the repairs.
pub const VERIFY_BEFORE: &str = "repair-verify-before.json";
pub const VERIFY_AFTER: &str = "repair-verify-after.json";

/// The L1 verify probe's unit, script (relative to `/etc`) and report
/// (in [`STATE_DIR_REL`]). L3 orders after the unit and re-runs the script
/// when they are staged; without them it repairs from its own checks.
pub const VERIFY_UNIT: &str = "bootc-migrate-verify-firstboot.service";
pub const VERIFY_SCRIPT_REL: &str = "bootc-migrate/verify-firstboot.sh";
pub const VERIFY_REPORT: &str = "verify-report.json";

/// The repair-result notification (relative to `/etc`).
pub const NOTIFY_SCRIPT_REL: &str = "bootc-migrate/repair-notify.sh";
pub const NOTIFY_DESKTOP_REL: &str = "xdg/autostart/bootc-migrate-repair-notify.desktop";

// ---- Repair classes ---------------------------------------------------------

/// The allowlist: the only breakage classes this module changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RepairClass {
    /// Owner/group/mtime drift on carried home trees, restored from the
    /// identity manifest.
    Ownership,
    /// `/etc/machine-id` still equals the source's: truncate it so the next
    /// boot generates a new one.
    MachineId,
    /// A flatpak installation whose files are not owned by its user (or by
    /// root, for the system installation): `flatpak repair`.
    Flatpak,
    /// SELinux mislabels the L1 probe attributes to the migration:
    /// `restorecon` on exactly those paths.
    SelinuxLabel,
}

impl RepairClass {
    /// Every class, in the order they run. Ownership first: the flatpak
    /// self-check runs over the restored ownership.
    pub const ALL: [RepairClass; 4] = [
        RepairClass::Ownership,
        RepairClass::MachineId,
        RepairClass::Flatpak,
        RepairClass::SelinuxLabel,
    ];

    pub fn name(self) -> &'static str {
        match self {
            RepairClass::Ownership => "ownership",
            RepairClass::MachineId => "machine-id",
            RepairClass::Flatpak => "flatpak",
            RepairClass::SelinuxLabel => "selinux-label",
        }
    }

    pub fn parse(name: &str) -> Option<RepairClass> {
        RepairClass::ALL.into_iter().find(|c| c.name() == name)
    }

    /// The class that repairs an L1 verify finding, or `None` when the
    /// finding has no known-safe fix and is only reported.
    pub fn for_verify_finding(class: &str) -> Option<RepairClass> {
        match class {
            "home-owner" | "home-entries" => Some(RepairClass::Ownership),
            "machine-id" => Some(RepairClass::MachineId),
            "flatpak" => Some(RepairClass::Flatpak),
            "selinux-label" => Some(RepairClass::SelinuxLabel),
            _ => None,
        }
    }
}

/// Parse a class list (comma-, space- or newline-separated; `#` starts a
/// comment to the end of the line). Unknown names
/// come back separately so the log can name them.
pub fn parse_skip_list(text: &str) -> (BTreeSet<RepairClass>, Vec<String>) {
    let mut skips = BTreeSet::new();
    let mut unknown = Vec::new();
    for word in text
        .lines()
        .map(|line| line.split('#').next().unwrap_or(""))
        .flat_map(|line| line.split(|c: char| c == ',' || c.is_whitespace()))
        .filter(|w| !w.is_empty())
    {
        match RepairClass::parse(word) {
            Some(class) => {
                skips.insert(class);
            }
            None => unknown.push(word.to_string()),
        }
    }
    (skips, unknown)
}

/// Whether the kernel command line disables the repair.
pub fn cmdline_disables(cmdline: &str) -> bool {
    cmdline.split_whitespace().any(|arg| arg == CMDLINE_DISABLE)
}

/// The class list the kernel command line skips (all skip arguments joined).
pub fn cmdline_skip_list(cmdline: &str) -> String {
    cmdline
        .split_whitespace()
        .filter_map(|arg| arg.strip_prefix(CMDLINE_SKIP_PREFIX))
        .collect::<Vec<_>>()
        .join(",")
}

/// What staging records for the first-boot run, stored in the marker.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepairConfig {
    /// The source system's machine-id; empty when it had none or when the
    /// route keeps the machine-id on purpose (the class is then clean).
    pub source_machine_id: String,
}

// ---- Identity manifest -----------------------------------------------------

/// One entry of the identity manifest. `rel` is relative to the `/var` root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityRecord {
    pub rel: PathBuf,
    pub uid: u32,
    pub gid: u32,
    /// Permission bits (`mode & 0o7777`).
    pub mode: u32,
    pub mtime_sec: i64,
    pub mtime_nsec: i64,
}

/// Escape a path for a manifest line: `\`, newline and tab.
fn escape_path(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    for &b in bytes {
        match b {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\t' => out.extend_from_slice(b"\\t"),
            _ => out.push(b),
        }
    }
    out
}

fn unescape_path(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut it = bytes.iter();
    while let Some(&b) = it.next() {
        if b != b'\\' {
            out.push(b);
            continue;
        }
        match it.next()? {
            b'\\' => out.push(b'\\'),
            b'n' => out.push(b'\n'),
            b't' => out.push(b'\t'),
            _ => return None,
        }
    }
    Some(out)
}

/// One manifest line, newline included: `uid gid mode sec nsec path`,
/// tab-separated, mode in octal.
pub fn format_record(r: &IdentityRecord) -> Vec<u8> {
    let mut line = format!(
        "{}\t{}\t{:o}\t{}\t{}\t",
        r.uid, r.gid, r.mode, r.mtime_sec, r.mtime_nsec
    )
    .into_bytes();
    line.extend(escape_path(r.rel.as_os_str().as_bytes()));
    line.push(b'\n');
    line
}

/// Parse one manifest line (without its newline). `None` for a malformed
/// line or a path that is not a plain relative path: a manifest entry can
/// never name `..`, an absolute path, or the `/var` root itself.
pub fn parse_record(line: &[u8]) -> Option<IdentityRecord> {
    let mut fields = line.splitn(6, |&b| b == b'\t');
    let mut num = |radix: u32| -> Option<i64> {
        let f = std::str::from_utf8(fields.next()?).ok()?;
        i64::from_str_radix(f, radix).ok()
    };
    let uid = u32::try_from(num(10)?).ok()?;
    let gid = u32::try_from(num(10)?).ok()?;
    let mode = u32::try_from(num(8)?).ok()? & 0o7777;
    let mtime_sec = num(10)?;
    let mtime_nsec = num(10)?;
    if !(0..1_000_000_000).contains(&mtime_nsec) {
        return None;
    }
    let rel = PathBuf::from(OsStr::from_bytes(&unescape_path(fields.next()?)?));
    if rel.as_os_str().is_empty() || !rel.components().all(|c| matches!(c, Component::Normal(_))) {
        return None;
    }
    Some(IdentityRecord {
        rel,
        uid,
        gid,
        mode,
        mtime_sec,
        mtime_nsec,
    })
}

fn record_for(rel: PathBuf, meta: &std::fs::Metadata) -> IdentityRecord {
    IdentityRecord {
        rel,
        uid: meta.uid(),
        gid: meta.gid(),
        mode: meta.mode() & 0o7777,
        mtime_sec: meta.mtime(),
        mtime_nsec: meta.mtime_nsec(),
    }
}

/// Record the identity of every entry under `<var_root>/home` into
/// `out`. Symlinks are recorded, never followed; the walk stays on the
/// filesystem `home` is on. Returns the number of records.
pub fn write_identity_manifest(var_root: &Path, out: &Path) -> Result<usize> {
    let home = var_root.join("home");
    let home_meta = match std::fs::symlink_metadata(&home) {
        Ok(m) if m.is_dir() => m,
        _ => {
            // No carried home tree: an empty manifest, so the repair logs
            // "nothing recorded" rather than "manifest missing".
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(out, b"")?;
            return Ok(0);
        }
    };
    let dev = home_meta.dev();
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let tmp = out.with_extension("tsv.tmp");
    let file = std::fs::File::create(&tmp)
        .with_context(|| format!("failed to create {}", tmp.display()))?;
    let mut w = BufWriter::new(file);
    let mut count = 0usize;
    let mut stack = vec![(PathBuf::from("home"), home_meta)];
    while let Some((rel, meta)) = stack.pop() {
        w.write_all(&format_record(&record_for(rel.clone(), &meta)))?;
        count += 1;
        if !meta.is_dir() || meta.dev() != dev {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(var_root.join(&rel)) else {
            continue;
        };
        for entry in entries.flatten() {
            if let Ok(m) = entry.metadata() {
                stack.push((rel.join(entry.file_name()), m));
            }
        }
    }
    w.flush()?;
    drop(w);
    std::fs::rename(&tmp, out).with_context(|| format!("failed to write {}", out.display()))?;
    Ok(count)
}

// ---- Ownership/mtime repair ------------------------------------------------

/// What differs between a manifest record and the live entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Drift {
    /// New owner and group, when either differs.
    pub chown: Option<(u32, u32)>,
    /// The recorded mtime, when it differs.
    pub mtime: Option<(i64, i64)>,
}

impl Drift {
    pub fn is_empty(&self) -> bool {
        self.chown.is_none() && self.mtime.is_none()
    }
}

/// Compare a record against the live entry's `(uid, gid, sec, nsec)`.
pub fn drift(record: &IdentityRecord, live: (u32, u32, i64, i64)) -> Drift {
    let (uid, gid, sec, nsec) = live;
    Drift {
        chown: (uid != record.uid || gid != record.gid).then_some((record.uid, record.gid)),
        mtime: (sec != record.mtime_sec || nsec != record.mtime_nsec)
            .then_some((record.mtime_sec, record.mtime_nsec)),
    }
}

/// The ownership pass's counts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct OwnershipOutcome {
    pub records: usize,
    pub missing: usize,
    pub rechowned: usize,
    pub retimed: usize,
    /// Records whose parent resolves outside `<var>/home` (a symlinked
    /// directory): never touched.
    pub refused: usize,
    pub malformed: usize,
    pub errors: Vec<String>,
}

impl OwnershipOutcome {
    fn changed(&self) -> usize {
        self.rechowned + self.retimed
    }
}

/// Restore owner, group and mtime from the manifest under `var_root`.
/// Never follows a symlink: the entry itself is changed with
/// `AT_SYMLINK_NOFOLLOW`, and a record whose parent directory resolves
/// outside `<var_root>/home` is refused. A chown that clears setuid/setgid
/// bits has the mode put back. `dry_run` only counts.
pub fn repair_ownership(
    var_root: &Path,
    manifest: &Path,
    dry_run: bool,
) -> Result<OwnershipOutcome> {
    use rustix::fs::{AtFlags, CWD, Gid, Mode, Timespec, Timestamps, UTIME_OMIT, Uid};

    let file = std::fs::File::open(manifest)
        .with_context(|| format!("failed to open {}", manifest.display()))?;
    let home = std::fs::canonicalize(var_root.join("home"))
        .with_context(|| format!("failed to resolve {}/home", var_root.display()))?;
    let mut out = OwnershipOutcome::default();
    // Records come grouped by directory: cache the last parent's verdict.
    let mut last_parent: Option<(PathBuf, bool)> = None;
    for line in std::io::BufReader::new(file).split(b'\n') {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        let Some(record) = parse_record(&line) else {
            out.malformed += 1;
            continue;
        };
        out.records += 1;
        let path = var_root.join(&record.rel);
        let parent = path.parent().unwrap_or(var_root).to_path_buf();
        let inside = match &last_parent {
            Some((p, ok)) if *p == parent => *ok,
            _ => {
                let ok = record.rel == Path::new("home")
                    || std::fs::canonicalize(&parent)
                        .map(|p| p.starts_with(&home))
                        .unwrap_or(false);
                last_parent = Some((parent, ok));
                ok
            }
        };
        if !inside {
            out.refused += 1;
            continue;
        }
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => {
                out.missing += 1;
                continue;
            }
        };
        let d = drift(
            &record,
            (meta.uid(), meta.gid(), meta.mtime(), meta.mtime_nsec()),
        );
        if d.is_empty() {
            continue;
        }
        if dry_run {
            out.rechowned += usize::from(d.chown.is_some());
            out.retimed += usize::from(d.mtime.is_some());
            continue;
        }
        if let Some((uid, gid)) = d.chown {
            let mode_before = meta.mode() & 0o7777;
            match rustix::fs::chownat(
                CWD,
                &path,
                Some(Uid::from_raw(uid)),
                Some(Gid::from_raw(gid)),
                AtFlags::SYMLINK_NOFOLLOW,
            ) {
                Ok(()) => {
                    out.rechowned += 1;
                    // chown drops setuid/setgid on files; put them back.
                    if !meta.file_type().is_symlink()
                        && mode_before & 0o6000 != 0
                        && let Ok(after) = std::fs::symlink_metadata(&path)
                        && after.mode() & 0o7777 != mode_before
                    {
                        let _ = rustix::fs::chmodat(
                            CWD,
                            &path,
                            Mode::from_raw_mode(mode_before),
                            AtFlags::empty(),
                        );
                    }
                }
                Err(e) => push_error(&mut out.errors, &path, "chown", e),
            }
        }
        if let Some((sec, nsec)) = d.mtime {
            let times = Timestamps {
                last_access: Timespec {
                    tv_sec: 0,
                    tv_nsec: UTIME_OMIT,
                },
                last_modification: Timespec {
                    tv_sec: sec,
                    tv_nsec: nsec,
                },
            };
            match rustix::fs::utimensat(CWD, &path, &times, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(()) => out.retimed += 1,
                Err(e) => push_error(&mut out.errors, &path, "set mtime", e),
            }
        }
    }
    Ok(out)
}

/// Keep the log bounded: the first 20 errors, then a count.
fn push_error(errors: &mut Vec<String>, path: &Path, what: &str, e: rustix::io::Errno) {
    if errors.len() < 20 {
        errors.push(format!("{what} {}: {e}", path.display()));
    } else if errors.len() == 20 {
        errors.push("further errors omitted".to_string());
    }
}

// ---- machine-id ------------------------------------------------------------

/// Whether the live machine-id is still the source's.
pub fn machine_id_duplicated(live: &str, source: &str) -> bool {
    let source = source.trim();
    !source.is_empty() && live.trim() == source
}

// ---- Flatpak ---------------------------------------------------------------

/// A login user from `/etc/passwd`: uid 1000..65534, a real shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HumanUser {
    pub name: String,
    pub uid: u32,
    pub home: PathBuf,
}

/// The login users in a passwd file, same rule as the L1 probe.
pub fn human_users(passwd: &str) -> Vec<HumanUser> {
    passwd
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split(':').collect();
            if f.len() < 7 {
                return None;
            }
            let uid: u32 = f[2].parse().ok()?;
            let shell = f[6];
            ((1000..65534).contains(&uid)
                && !shell.ends_with("nologin")
                && !shell.ends_with("false"))
            .then(|| HumanUser {
                name: f[0].to_string(),
                uid,
                home: PathBuf::from(f[5]),
            })
        })
        .collect()
}

/// A flatpak installation and who must own it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlatpakInstall {
    /// `None` for the system installation.
    pub user: Option<HumanUser>,
    /// The installation directory, as seen on the running system.
    pub dir: PathBuf,
}

impl FlatpakInstall {
    fn expected_uid(&self) -> u32 {
        self.user.as_ref().map_or(0, |u| u.uid)
    }

    /// The `flatpak repair` invocation for this installation.
    pub fn repair_command(&self) -> Vec<String> {
        match &self.user {
            None => ["flatpak", "repair", "--system"].map(String::from).to_vec(),
            Some(u) => vec![
                "runuser".into(),
                "-u".into(),
                u.name.clone(),
                "--".into(),
                "env".into(),
                format!("HOME={}", u.home.display()),
                "flatpak".into(),
                "repair".into(),
                "--user".into(),
            ],
        }
    }
}

/// The installations to check: the system one and each login user's.
pub fn flatpak_installs(users: &[HumanUser]) -> Vec<FlatpakInstall> {
    let mut v = vec![FlatpakInstall {
        user: None,
        dir: PathBuf::from("/var/lib/flatpak"),
    }];
    v.extend(users.iter().map(|u| FlatpakInstall {
        user: Some(u.clone()),
        dir: u.home.join(".local/share/flatpak"),
    }));
    v
}

/// The self-check: the installation directory and its `repo`, `app` and
/// `runtime` entries must be owned by the installation's user (root for the
/// system one). `root` prefixes every path (fixture roots in tests).
/// `None`: healthy or absent.
pub fn flatpak_self_check(root: &Path, install: &FlatpakInstall) -> Option<String> {
    let dir = reroot(root, &install.dir);
    let want = install.expected_uid();
    let top = std::fs::symlink_metadata(&dir).ok()?;
    let mut wrong = Vec::new();
    if top.uid() != want {
        wrong.push(format!("{} (uid {})", install.dir.display(), top.uid()));
    }
    for sub in ["repo", "app", "runtime"] {
        if let Ok(m) = std::fs::symlink_metadata(dir.join(sub))
            && m.uid() != want
        {
            wrong.push(format!("{}/{sub} (uid {})", install.dir.display(), m.uid()));
        }
    }
    (!wrong.is_empty()).then(|| format!("expected uid {want}: {}", wrong.join(", ")))
}

fn reroot(root: &Path, abs: &Path) -> PathBuf {
    root.join(abs.strip_prefix("/").unwrap_or(abs))
}

// ---- SELinux ---------------------------------------------------------------

/// One L1 verify finding.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Finding {
    pub class: String,
    pub detail: String,
}

#[derive(Debug, Deserialize)]
struct VerifyReport {
    findings: Vec<Finding>,
}

/// Parse an L1 verify report. `None` when it is not one.
pub fn parse_verify_report(json: &str) -> Option<Vec<Finding>> {
    serde_json::from_str::<VerifyReport>(json)
        .ok()
        .map(|r| r.findings)
}

/// The paths `selinux-label` findings name: the detail's first word, kept
/// only when it is an absolute path with no `..`. Sorted, deduplicated.
pub fn selinux_targets(findings: &[Finding]) -> Vec<PathBuf> {
    findings
        .iter()
        .filter(|f| f.class == "selinux-label")
        .filter_map(|f| f.detail.split_whitespace().next())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute() && !p.components().any(|c| c == Component::ParentDir))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// `restorecon` on exactly the given paths: forced, not recursive.
pub fn restorecon_command(paths: &[PathBuf]) -> Vec<String> {
    let mut cmd = ["restorecon", "-F", "-v", "--"].map(String::from).to_vec();
    cmd.extend(paths.iter().map(|p| p.display().to_string()));
    cmd
}

// ---- The first-boot run ----------------------------------------------------

/// How a class ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    /// Breakage found and fixed.
    Repaired,
    /// Nothing to fix.
    Clean,
    /// Skipped by configuration.
    Skipped,
    /// Breakage found, a fix not possible here (missing tool, no input).
    NotApplicable,
    /// The fix ran and did not succeed.
    Failed,
}

/// One class's entry in the repair log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClassOutcome {
    pub class: RepairClass,
    pub status: Status,
    pub detail: String,
    pub actions: Vec<String>,
}

/// The repair log, written as JSON.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RepairLog {
    pub generated_unix: u64,
    pub dry_run: bool,
    /// Why the run did nothing, when it was disabled.
    pub disabled: Option<String>,
    pub classes: Vec<ClassOutcome>,
    /// Skip-list entries that name no class.
    pub unknown_skips: Vec<String>,
    /// L1 findings with no known-safe fix: reported, never touched.
    pub report_only: Vec<Finding>,
    /// L1 findings before the repairs, after them, and which went away.
    pub verify_before: Option<Vec<Finding>>,
    pub verify_after: Option<Vec<Finding>>,
    pub resolved: Vec<Finding>,
}

impl RepairLog {
    fn count(&self, status: Status) -> usize {
        self.classes.iter().filter(|c| c.status == status).count()
    }

    /// The one-line summary written to [`REPAIR_RESULT`].
    pub fn summary(&self) -> String {
        if let Some(reason) = &self.disabled {
            return format!("DISABLED {reason}");
        }
        let (repaired, failed) = (self.count(Status::Repaired), self.count(Status::Failed));
        if repaired == 0 && failed == 0 && self.report_only.is_empty() {
            "CLEAN".to_string()
        } else {
            format!(
                "REPAIRED {repaired} FAILED {failed} REPORTED {}",
                self.report_only.len()
            )
        }
    }
}

/// One first-boot pass over a root.
#[derive(Debug)]
pub struct FirstbootRun<'a> {
    /// `/` in production; a fixture root in tests.
    pub root: &'a Path,
    /// The kernel command line.
    pub cmdline: &'a str,
    /// Count and log only; change nothing.
    pub dry_run: bool,
    /// Run external tools (`flatpak`, `restorecon`, the L1 probe). Off in
    /// tests: those classes then end [`Status::NotApplicable`].
    pub run_tools: bool,
}

impl FirstbootRun<'_> {
    fn etc(&self) -> PathBuf {
        self.root.join("etc")
    }

    fn var(&self) -> PathBuf {
        self.root.join("var")
    }

    fn state_dir(&self) -> PathBuf {
        self.var().join(STATE_DIR_REL)
    }

    /// Run every class not skipped, re-run the L1 probe, and return the
    /// log. Never fails: a class that cannot run is logged as such.
    pub fn run(&self) -> RepairLog {
        let mut log = RepairLog {
            generated_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            dry_run: self.dry_run,
            ..RepairLog::default()
        };
        if self.etc().join(REPAIR_DISABLED_REL).exists() {
            log.disabled = Some(format!("/etc/{REPAIR_DISABLED_REL} present"));
            return log;
        }
        if cmdline_disables(self.cmdline) {
            log.disabled = Some(format!("kernel argument {CMDLINE_DISABLE}"));
            return log;
        }
        let skip_text = format!(
            "{}\n{}",
            std::fs::read_to_string(self.etc().join(REPAIR_SKIP_REL)).unwrap_or_default(),
            cmdline_skip_list(self.cmdline)
        );
        let (skips, unknown) = parse_skip_list(&skip_text);
        log.unknown_skips = unknown;

        let config: RepairConfig = std::fs::read_to_string(self.etc().join(REPAIR_MARKER))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let before = std::fs::read_to_string(self.state_dir().join(VERIFY_REPORT))
            .ok()
            .and_then(|s| parse_verify_report(&s));
        let findings = before.clone().unwrap_or_default();

        for class in RepairClass::ALL {
            let outcome = if skips.contains(&class) {
                ClassOutcome {
                    class,
                    status: Status::Skipped,
                    detail: "skipped by configuration".into(),
                    actions: vec![],
                }
            } else {
                match class {
                    RepairClass::Ownership => self.ownership(),
                    RepairClass::MachineId => self.machine_id(&config),
                    RepairClass::Flatpak => self.flatpak(),
                    RepairClass::SelinuxLabel => self.selinux(&findings),
                }
            };
            println!(
                "repair: [{}] {:?}: {}",
                class.name(),
                outcome.status,
                outcome.detail
            );
            log.classes.push(outcome);
        }
        log.report_only = findings
            .iter()
            .filter(|f| RepairClass::for_verify_finding(&f.class).is_none())
            .cloned()
            .collect();

        if let Some(before) = before {
            let after = self.rerun_probe();
            if let Some(after) = &after {
                log.resolved = before
                    .iter()
                    .filter(|f| !after.contains(f))
                    .cloned()
                    .collect();
            }
            log.verify_before = Some(before);
            log.verify_after = after;
        }
        log
    }

    fn ownership(&self) -> ClassOutcome {
        let class = RepairClass::Ownership;
        let manifest = self.state_dir().join(IDENTITY_MANIFEST);
        if !manifest.exists() {
            return ClassOutcome {
                class,
                status: Status::NotApplicable,
                detail: format!("no identity manifest at {}", manifest.display()),
                actions: vec![],
            };
        }
        match repair_ownership(&self.var(), &manifest, self.dry_run) {
            Err(e) => ClassOutcome {
                class,
                status: Status::Failed,
                detail: format!("{e:#}"),
                actions: vec![],
            },
            Ok(o) => {
                let status = if !o.errors.is_empty() {
                    Status::Failed
                } else if o.changed() > 0 {
                    Status::Repaired
                } else {
                    Status::Clean
                };
                ClassOutcome {
                    class,
                    status,
                    detail: format!(
                        "{} recorded, {} rechowned, {} retimed, {} missing, {} refused, {} malformed",
                        o.records, o.rechowned, o.retimed, o.missing, o.refused, o.malformed
                    ),
                    actions: o.errors,
                }
            }
        }
    }

    fn machine_id(&self, config: &RepairConfig) -> ClassOutcome {
        let class = RepairClass::MachineId;
        let path = self.etc().join("machine-id");
        let live = std::fs::read_to_string(&path).unwrap_or_default();
        if config.source_machine_id.trim().is_empty() {
            return ClassOutcome {
                class,
                status: Status::Clean,
                detail: "no source machine-id recorded: this route keeps the machine-id on purpose"
                    .into(),
                actions: vec![],
            };
        }
        if !machine_id_duplicated(&live, &config.source_machine_id) {
            return ClassOutcome {
                class,
                status: Status::Clean,
                detail: "machine-id differs from the source's".into(),
                actions: vec![],
            };
        }
        let action = "truncated /etc/machine-id; the next boot generates a new one".to_string();
        if self.dry_run {
            return ClassOutcome {
                class,
                status: Status::Repaired,
                detail: "machine-id still equals the source's".into(),
                actions: vec![format!("would have {action}")],
            };
        }
        match std::fs::write(&path, b"") {
            Ok(()) => ClassOutcome {
                class,
                status: Status::Repaired,
                detail: "machine-id still equaled the source's".into(),
                actions: vec![action],
            },
            Err(e) => ClassOutcome {
                class,
                status: Status::Failed,
                detail: format!("failed to truncate /etc/machine-id: {e}"),
                actions: vec![],
            },
        }
    }

    fn flatpak(&self) -> ClassOutcome {
        let class = RepairClass::Flatpak;
        let passwd = std::fs::read_to_string(self.etc().join("passwd")).unwrap_or_default();
        let broken: Vec<(FlatpakInstall, String)> = flatpak_installs(&human_users(&passwd))
            .into_iter()
            .filter_map(|i| flatpak_self_check(self.root, &i).map(|why| (i, why)))
            .collect();
        if broken.is_empty() {
            return ClassOutcome {
                class,
                status: Status::Clean,
                detail: "every flatpak installation passes the self-check".into(),
                actions: vec![],
            };
        }
        let detail = broken
            .iter()
            .map(|(_, why)| why.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        if !self.run_tools || !self.dry_run && which("flatpak").is_none() {
            return ClassOutcome {
                class,
                status: Status::NotApplicable,
                detail: format!("{detail} (flatpak not run)"),
                actions: vec![],
            };
        }
        let mut actions = Vec::new();
        let mut failed = false;
        for (install, _) in &broken {
            let cmd = install.repair_command();
            if self.dry_run {
                actions.push(format!("would run: {}", cmd.join(" ")));
                continue;
            }
            let ok = run_tool(&cmd);
            failed |= !ok;
            actions.push(format!(
                "{}: {}",
                cmd.join(" "),
                if ok { "ok" } else { "failed" }
            ));
        }
        ClassOutcome {
            class,
            status: if failed {
                Status::Failed
            } else {
                Status::Repaired
            },
            detail,
            actions,
        }
    }

    fn selinux(&self, findings: &[Finding]) -> ClassOutcome {
        let class = RepairClass::SelinuxLabel;
        let targets: Vec<PathBuf> = selinux_targets(findings)
            .into_iter()
            .filter(|p| std::fs::symlink_metadata(reroot(self.root, p)).is_ok())
            .collect();
        if targets.is_empty() {
            return ClassOutcome {
                class,
                status: Status::Clean,
                detail: "no mislabels attributed to the migration".into(),
                actions: vec![],
            };
        }
        let detail = format!("{} mislabelled path(s)", targets.len());
        if !self.run_tools
            || !self.root.join("sys/fs/selinux/enforce").exists()
            || !self.dry_run && which("restorecon").is_none()
        {
            return ClassOutcome {
                class,
                status: Status::NotApplicable,
                detail: format!("{detail} (SELinux or restorecon not available)"),
                actions: vec![],
            };
        }
        let cmd = restorecon_command(&targets);
        if self.dry_run {
            return ClassOutcome {
                class,
                status: Status::Repaired,
                detail,
                actions: vec![format!("would run: {}", cmd.join(" "))],
            };
        }
        let ok = run_tool(&cmd);
        ClassOutcome {
            class,
            status: if ok { Status::Repaired } else { Status::Failed },
            detail,
            actions: vec![format!(
                "{}: {}",
                cmd.join(" "),
                if ok { "ok" } else { "failed" }
            )],
        }
    }

    /// Re-run the L1 probe (when staged) and read its fresh report.
    fn rerun_probe(&self) -> Option<Vec<Finding>> {
        let script = self.etc().join(VERIFY_SCRIPT_REL);
        if self.dry_run || !self.run_tools || !script.exists() {
            return None;
        }
        let state = self.state_dir();
        let _ = std::fs::copy(state.join(VERIFY_REPORT), state.join(VERIFY_BEFORE));
        if !run_tool(&["/bin/sh".to_string(), script.display().to_string()]) {
            return None;
        }
        let _ = std::fs::copy(state.join(VERIFY_REPORT), state.join(VERIFY_AFTER));
        std::fs::read_to_string(state.join(VERIFY_REPORT))
            .ok()
            .and_then(|s| parse_verify_report(&s))
    }

    /// Write the log and the one-line result into the state directory.
    pub fn write_log(&self, log: &RepairLog) -> Result<()> {
        let dir = self.state_dir();
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
        std::fs::write(
            dir.join(REPAIR_LOG),
            serde_json::to_string_pretty(log)? + "\n",
        )?;
        std::fs::write(dir.join(REPAIR_RESULT), log.summary() + "\n")?;
        Ok(())
    }
}

fn which(tool: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|d| d.join(tool))
        .find(|p| p.is_file())
}

fn run_tool(cmd: &[String]) -> bool {
    let Some((prog, args)) = cmd.split_first() else {
        return false;
    };
    match Command::new(prog).args(args).status() {
        Ok(s) => s.success(),
        Err(e) => {
            eprintln!("repair: failed to run {prog}: {e}");
            false
        }
    }
}

// ---- Staging ---------------------------------------------------------------

/// Render the oneshot unit. It runs once (the marker goes either way: the
/// repair line is `-`-prefixed), after the cross-family and L1 units when
/// they are staged (an `After=` on an absent unit is ignored), and before
/// any user session starts.
pub fn render_repair_unit() -> String {
    format!(
        "[Unit]\n\
         Description=bootc-migrate first-boot repair of known migration breakage\n\
         ConditionPathExists=/etc/{REPAIR_MARKER}\n\
         ConditionPathExists=!/etc/{REPAIR_DISABLED_REL}\n\
         ConditionKernelCommandLine=!{CMDLINE_DISABLE}\n\
         After=local-fs.target {cross} {VERIFY_UNIT}\n\
         Before=systemd-user-sessions.service display-manager.service\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=no\n\
         ExecStart=-/etc/{REPAIR_BIN_REL} repair --firstboot\n\
         ExecStart=/usr/bin/rm -f /etc/{REPAIR_MARKER} /etc/{REPAIR_BIN_REL}\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        cross = crate::cross_family::FIRSTBOOT_UNIT,
    )
}

/// The notification script: once per desktop user, when a repair log
/// exists, whatever it says. Silent without a display or `notify-send`.
pub const NOTIFY_SCRIPT: &str = r#"#!/bin/sh
# bootc-migrate first-boot repair notification (L3). Autostarted once per
# desktop user; points at the repair log whether or not anything was fixed.
set -u
STATE_DIR=${BOOTC_MIGRATE_STATE_DIR:-/var/lib/bootc-migrate}
STAMP=${XDG_STATE_HOME:-$HOME/.local/state}/bootc-migrate/repair-notified
[ -f "$STAMP" ] && exit 0
[ -f "$STATE_DIR/repair-result" ] || exit 0
[ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ] || exit 0
command -v notify-send >/dev/null 2>&1 || exit 0
notify-send --app-name=bootc-migrate "System migration check" \
    "First-boot repair: $(cat "$STATE_DIR/repair-result"). Log: $STATE_DIR/repair-log.json"
mkdir -p "$(dirname "$STAMP")" && touch "$STAMP"
exit 0
"#;

pub const NOTIFY_DESKTOP: &str = "[Desktop Entry]\n\
     Type=Application\n\
     Name=System migration check\n\
     Comment=Show the result of the first-boot migration repair once\n\
     Exec=/bin/sh /etc/bootc-migrate/repair-notify.sh\n\
     NoDisplay=true\n\
     X-GNOME-Autostart-enabled=true\n";

fn write_file(path: &Path, content: &[u8], mode: u32) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(path, content).with_context(|| format!("failed to write {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

/// Install the unit, its enablement, the marker and the notification into
/// `etc_dir`, with `binary` as the staged repair binary.
pub fn install_repair_unit(etc_dir: &Path, binary: &Path, config: &RepairConfig) -> Result<()> {
    std::fs::create_dir_all(etc_dir.join("bootc-migrate"))?;
    let bin = etc_dir.join(REPAIR_BIN_REL);
    std::fs::copy(binary, &bin)
        .with_context(|| format!("failed to stage {} as {}", binary.display(), bin.display()))?;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))?;

    let unit_dir = etc_dir.join("systemd/system");
    write_file(
        &unit_dir.join(REPAIR_UNIT),
        render_repair_unit().as_bytes(),
        0o644,
    )?;
    let wants = unit_dir.join("multi-user.target.wants");
    std::fs::create_dir_all(&wants)?;
    let link = wants.join(REPAIR_UNIT);
    if std::fs::symlink_metadata(&link).is_ok() {
        std::fs::remove_file(&link)?;
    }
    std::os::unix::fs::symlink(format!("../{REPAIR_UNIT}"), &link)
        .with_context(|| format!("failed to enable {REPAIR_UNIT}"))?;
    write_file(
        &etc_dir.join(REPAIR_MARKER),
        (serde_json::to_string(config)? + "\n").as_bytes(),
        0o644,
    )?;
    write_file(
        &etc_dir.join(NOTIFY_SCRIPT_REL),
        NOTIFY_SCRIPT.as_bytes(),
        0o755,
    )?;
    write_file(
        &etc_dir.join(NOTIFY_DESKTOP_REL),
        NOTIFY_DESKTOP.as_bytes(),
        0o644,
    )?;
    Ok(())
}

/// Stage the first-boot repair into a new deployment: record the identity
/// of the live `/var/home` into `<staged_var>/lib/bootc-migrate`, then
/// install the unit with the running binary into `etc_dir`.
///
/// `machine_id_is_breakage` arms the machine-id class. Routes whose `/etc`
/// merge keeps the machine-id on purpose (identity preservation in
/// [`crate::mergetc`]) pass `false`, so the class stays clean there.
pub fn stage_repair(etc_dir: &Path, staged_var: &Path, machine_id_is_breakage: bool) -> Result<()> {
    let manifest = staged_var.join(STATE_DIR_REL).join(IDENTITY_MANIFEST);
    let n = write_identity_manifest(Path::new("/var"), &manifest)
        .context("failed to record the /var/home identity manifest")?;
    let binary = std::env::current_exe().context("failed to locate the running binary")?;
    let config = RepairConfig {
        source_machine_id: if machine_id_is_breakage {
            std::fs::read_to_string("/etc/machine-id")
                .map(|s| s.trim().to_string())
                .unwrap_or_default()
        } else {
            String::new()
        },
    };
    install_repair_unit(etc_dir, &binary, &config)
        .context("failed to stage the first-boot repair unit")?;
    println!(
        "[firstboot] repair staged: {n} /var/home identity record(s) in {}; \
         opt out with 'repair --disable' or the kernel argument {CMDLINE_DISABLE}",
        manifest.display()
    );
    Ok(())
}

// ---- CLI -------------------------------------------------------------------

/// What `repair` was asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliAction {
    /// The unit's first-boot pass over `/`.
    Firstboot,
    /// Report what the pass would change; change nothing.
    DryRun,
    /// Stop the staged unit from running.
    Disable,
    /// Undo `Disable`.
    Enable,
}

/// The `/etc` directories `--disable` and `--enable` act on: the live one
/// and every staged deployment's that carries the repair marker.
fn staged_etc_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from("/etc")];
    for parent in [
        "/sysroot/state/deploy",
        crate::ostree_install::OSTREE_DEPLOY_DIR,
    ] {
        if let Ok(entries) = std::fs::read_dir(parent) {
            for e in entries.flatten() {
                let etc = e.path().join("etc");
                if etc.join(REPAIR_MARKER).exists() {
                    dirs.push(etc);
                }
            }
        }
    }
    dirs
}

/// Run the `repair` subcommand.
pub fn cli(action: CliAction) -> Result<()> {
    match action {
        CliAction::Firstboot | CliAction::DryRun => {
            let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
            let run = FirstbootRun {
                root: Path::new("/"),
                cmdline: &cmdline,
                dry_run: action == CliAction::DryRun,
                run_tools: true,
            };
            let log = run.run();
            if action == CliAction::Firstboot {
                run.write_log(&log)?;
                println!(
                    "repair: {}; log at /var/{STATE_DIR_REL}/{REPAIR_LOG}",
                    log.summary()
                );
            } else {
                println!("{}", serde_json::to_string_pretty(&log)?);
            }
            Ok(())
        }
        CliAction::Disable => {
            let dirs = staged_etc_dirs();
            for etc in &dirs {
                write_file(
                    &etc.join(REPAIR_DISABLED_REL),
                    b"disabled by 'bootc-migrate repair --disable'\n",
                    0o644,
                )?;
                println!("repair: disabled in {}", etc.display());
            }
            if dirs.len() == 1 {
                println!("repair: no staged deployment carries the repair unit");
            }
            Ok(())
        }
        CliAction::Enable => {
            for etc in staged_etc_dirs() {
                let flag = etc.join(REPAIR_DISABLED_REL);
                if flag.exists() {
                    std::fs::remove_file(&flag)
                        .with_context(|| format!("failed to remove {}", flag.display()))?;
                    println!("repair: enabled in {}", etc.display());
                }
            }
            Ok(())
        }
    }
}

/// Map the subcommand's flags to an action; exactly one may be given.
pub fn cli_action(
    firstboot: bool,
    dry_run: bool,
    disable: bool,
    enable: bool,
) -> Result<CliAction> {
    match (firstboot, dry_run, disable, enable) {
        (true, false, false, false) => Ok(CliAction::Firstboot),
        (false, true, false, false) | (false, false, false, false) => Ok(CliAction::DryRun),
        (false, false, true, false) => Ok(CliAction::Disable),
        (false, false, false, true) => Ok(CliAction::Enable),
        _ => bail!("give at most one of --firstboot, --dry-run, --disable, --enable"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn rec(rel: &str) -> IdentityRecord {
        IdentityRecord {
            rel: PathBuf::from(rel),
            uid: 1000,
            gid: 1000,
            mode: 0o644,
            mtime_sec: 1_700_000_000,
            mtime_nsec: 5,
        }
    }

    #[test]
    fn record_round_trips_including_awkward_paths() {
        for rel in [
            "home/u/plain",
            "home/u/with space",
            "home/u/tab\there",
            "home/u/new\nline",
            "home/u/back\\slash",
        ] {
            let r = rec(rel);
            let line = format_record(&r);
            assert_eq!(line.iter().filter(|&&b| b == b'\n').count(), 1, "{rel}");
            assert_eq!(parse_record(&line[..line.len() - 1]), Some(r), "{rel}");
        }
    }

    #[test]
    fn parse_record_rejects_unsafe_or_malformed_lines() {
        for (name, line) in [
            ("parent dir", "1000\t1000\t644\t1\t0\thome/../etc/shadow"),
            ("absolute", "1000\t1000\t644\t1\t0\t/etc/shadow"),
            ("empty path", "1000\t1000\t644\t1\t0\t"),
            ("current dir", "1000\t1000\t644\t1\t0\t./home"),
            ("bad mode", "1000\t1000\t9z\t1\t0\thome/u"),
            ("nsec range", "1000\t1000\t644\t1\t1000000000\thome/u"),
            ("negative uid", "-1\t1000\t644\t1\t0\thome/u"),
            ("short", "1000\t1000\t644"),
            ("bad escape", "1000\t1000\t644\t1\t0\thome/u\\x"),
        ] {
            assert_eq!(parse_record(line.as_bytes()), None, "{name}");
        }
    }

    #[test]
    fn drift_table() {
        let r = rec("home/u/f");
        for (name, live, want) in [
            (
                "identical",
                (1000, 1000, 1_700_000_000, 5),
                Drift::default(),
            ),
            (
                "root-owned (#308)",
                (0, 0, 1_700_000_000, 5),
                Drift {
                    chown: Some((1000, 1000)),
                    mtime: None,
                },
            ),
            (
                "group only",
                (1000, 0, 1_700_000_000, 5),
                Drift {
                    chown: Some((1000, 1000)),
                    mtime: None,
                },
            ),
            (
                "fresh mtime",
                (1000, 1000, 1_800_000_000, 0),
                Drift {
                    chown: None,
                    mtime: Some((1_700_000_000, 5)),
                },
            ),
            (
                "both",
                (0, 0, 1_800_000_000, 5),
                Drift {
                    chown: Some((1000, 1000)),
                    mtime: Some((1_700_000_000, 5)),
                },
            ),
        ] {
            assert_eq!(drift(&r, live), want, "{name}");
        }
    }

    /// Manifest written from one tree restores the mtimes of a drifted copy
    /// (a non-root test can only "chown" to itself, so mtime carries the
    /// apply path; the chown decision is covered by `drift_table`).
    #[test]
    fn manifest_restores_mtime_drift_on_a_fixture_root() {
        let src = tempdir().unwrap();
        let home = src.path().join("home/alice/.config");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("settings"), "x").unwrap();
        std::os::unix::fs::symlink("settings", home.join("link")).unwrap();
        let old = Mtime(1_600_000_000, 123);
        set_mtime(&home.join("settings"), old);

        let manifest = src.path().join(STATE_DIR_REL).join(IDENTITY_MANIFEST);
        let n = write_identity_manifest(src.path(), &manifest).unwrap();
        // home, alice, .config, settings, link
        assert_eq!(n, 5);

        // Drift: a copy that landed with a fresh mtime.
        set_mtime(&home.join("settings"), Mtime(1_900_000_000, 0));
        let dry = repair_ownership(src.path(), &manifest, true).unwrap();
        assert_eq!(dry.retimed, 1, "{dry:?}");
        assert_eq!(
            std::fs::metadata(home.join("settings")).unwrap().mtime(),
            1_900_000_000,
            "dry run must not change anything"
        );

        let out = repair_ownership(src.path(), &manifest, false).unwrap();
        assert_eq!(out.retimed, 1, "{out:?}");
        assert_eq!(out.rechowned, 0);
        assert!(out.errors.is_empty(), "{out:?}");
        let m = std::fs::metadata(home.join("settings")).unwrap();
        assert_eq!((m.mtime(), m.mtime_nsec()), (1_600_000_000, 123));

        // Idempotent.
        let again = repair_ownership(src.path(), &manifest, false).unwrap();
        assert_eq!(again.changed(), 0, "{again:?}");
    }

    #[test]
    fn ownership_refuses_records_behind_a_symlinked_directory() {
        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        std::fs::write(outside.path().join("victim"), "x").unwrap();
        set_mtime(&outside.path().join("victim"), Mtime(1_900_000_000, 0));
        std::fs::create_dir_all(root.path().join("home")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("home/evil")).unwrap();
        let manifest = root.path().join("m.tsv");
        let mut r = rec("home/evil/victim");
        r.mtime_sec = 1;
        r.mtime_nsec = 0;
        std::fs::write(&manifest, format_record(&r)).unwrap();

        let out = repair_ownership(root.path(), &manifest, false).unwrap();
        assert_eq!(out.refused, 1, "{out:?}");
        assert_eq!(out.changed(), 0);
        assert_eq!(
            std::fs::metadata(outside.path().join("victim"))
                .unwrap()
                .mtime(),
            1_900_000_000
        );
    }

    #[test]
    fn ownership_counts_missing_and_malformed() {
        let root = tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("home")).unwrap();
        let manifest = root.path().join("m.tsv");
        let mut body = format_record(&rec("home/gone"));
        body.extend_from_slice(b"garbage\n");
        std::fs::write(&manifest, body).unwrap();
        let out = repair_ownership(root.path(), &manifest, false).unwrap();
        assert_eq!((out.missing, out.malformed, out.records), (1, 1, 1));
    }

    #[test]
    fn manifest_without_home_is_empty() {
        let root = tempdir().unwrap();
        let manifest = root.path().join("out/m.tsv");
        assert_eq!(write_identity_manifest(root.path(), &manifest).unwrap(), 0);
        assert_eq!(std::fs::read(&manifest).unwrap(), b"");
    }

    #[derive(Clone, Copy)]
    struct Mtime(i64, i64);

    fn set_mtime(path: &Path, t: Mtime) {
        use rustix::fs::{AtFlags, CWD, Timespec, UTIME_OMIT};
        rustix::fs::utimensat(
            CWD,
            path,
            &rustix::fs::Timestamps {
                last_access: Timespec {
                    tv_sec: 0,
                    tv_nsec: UTIME_OMIT,
                },
                last_modification: Timespec {
                    tv_sec: t.0,
                    tv_nsec: t.1,
                },
            },
            AtFlags::SYMLINK_NOFOLLOW,
        )
        .unwrap();
    }

    #[test]
    fn machine_id_duplication_table() {
        for (live, source, want) in [
            ("abc\n", "abc", true),
            ("abc", "abc\n", true),
            ("def\n", "abc", false),
            ("", "abc", false),
            ("abc\n", "", false),
            ("", "", false),
        ] {
            assert_eq!(
                machine_id_duplicated(live, source),
                want,
                "{live:?} {source:?}"
            );
        }
    }

    #[test]
    fn skip_lists_and_cmdline() {
        let (s, unknown) = parse_skip_list("flatpak, selinux-label\n# comment\nbogus");
        assert_eq!(
            s,
            BTreeSet::from([RepairClass::Flatpak, RepairClass::SelinuxLabel])
        );
        assert_eq!(unknown, vec!["bogus"]);
        assert!(cmdline_disables("quiet bootc_migrate.repair=0 rw"));
        assert!(!cmdline_disables("quiet bootc_migrate.repair=1"));
        assert_eq!(
            cmdline_skip_list(
                "a bootc_migrate.repair.skip=flatpak b bootc_migrate.repair.skip=machine-id"
            ),
            "flatpak,machine-id"
        );
        for class in RepairClass::ALL {
            assert_eq!(RepairClass::parse(class.name()), Some(class));
        }
    }

    #[test]
    fn verify_findings_map_to_classes_or_report_only() {
        for (finding, want) in [
            ("home-owner", Some(RepairClass::Ownership)),
            ("home-entries", Some(RepairClass::Ownership)),
            ("machine-id", Some(RepairClass::MachineId)),
            ("selinux-label", Some(RepairClass::SelinuxLabel)),
            ("missing-user", None),
            ("home-missing", None),
        ] {
            assert_eq!(RepairClass::for_verify_finding(finding), want, "{finding}");
        }
    }

    #[test]
    fn human_users_and_flatpak_commands() {
        let passwd = "root:x:0:0:root:/root:/bin/bash\n\
                      dbus:x:81:81::/:/sbin/nologin\n\
                      alice:x:1000:1000::/var/home/alice:/bin/bash\n\
                      svc:x:1001:1001::/var/home/svc:/usr/bin/false\n\
                      nobody:x:65534:65534::/:/bin/sh\n";
        let users = human_users(passwd);
        assert_eq!(
            users,
            vec![HumanUser {
                name: "alice".into(),
                uid: 1000,
                home: "/var/home/alice".into()
            }]
        );
        let installs = flatpak_installs(&users);
        assert_eq!(
            installs[0].repair_command(),
            ["flatpak", "repair", "--system"]
        );
        assert_eq!(
            installs[1].repair_command(),
            [
                "runuser",
                "-u",
                "alice",
                "--",
                "env",
                "HOME=/var/home/alice",
                "flatpak",
                "repair",
                "--user"
            ]
        );
        assert_eq!(
            installs[1].dir,
            PathBuf::from("/var/home/alice/.local/share/flatpak")
        );
    }

    #[test]
    fn flatpak_self_check_on_fixture_root() {
        let root = tempdir().unwrap();
        let me = rustix::process::getuid().as_raw();
        let user = HumanUser {
            name: "me".into(),
            uid: me,
            home: "/var/home/me".into(),
        };
        let install = FlatpakInstall {
            user: Some(user.clone()),
            dir: user.home.join(".local/share/flatpak"),
        };
        // Absent: healthy.
        assert_eq!(flatpak_self_check(root.path(), &install), None);
        std::fs::create_dir_all(root.path().join("var/home/me/.local/share/flatpak/repo")).unwrap();
        assert_eq!(flatpak_self_check(root.path(), &install), None);
        // Same tree checked as someone else's: every entry is foreign.
        let other = FlatpakInstall {
            user: Some(HumanUser {
                uid: me + 1,
                ..user
            }),
            dir: install.dir.clone(),
        };
        let why = flatpak_self_check(root.path(), &other).unwrap();
        assert!(why.contains("flatpak/repo"), "{why}");
    }

    #[test]
    fn selinux_targets_keep_only_safe_absolute_paths() {
        let findings = vec![
            Finding {
                class: "selinux-label".into(),
                detail: "/var/home/alice/.ssh is user_tmp_t, expected ssh_home_t".into(),
            },
            Finding {
                class: "selinux-label".into(),
                detail: "relative/path is wrong".into(),
            },
            Finding {
                class: "selinux-label".into(),
                detail: "/var/home/../../etc is wrong".into(),
            },
            Finding {
                class: "home-owner".into(),
                detail: "/var/home/alice owned by uid 0".into(),
            },
            Finding {
                class: "selinux-label".into(),
                detail: "/var/home/alice/.ssh again".into(),
            },
        ];
        let targets = selinux_targets(&findings);
        assert_eq!(targets, vec![PathBuf::from("/var/home/alice/.ssh")]);
        assert_eq!(
            restorecon_command(&targets),
            ["restorecon", "-F", "-v", "--", "/var/home/alice/.ssh"]
        );
    }

    #[test]
    fn repair_unit_shape() {
        let unit = render_repair_unit();
        for needle in [
            format!("ConditionPathExists=/etc/{REPAIR_MARKER}"),
            format!("ConditionPathExists=!/etc/{REPAIR_DISABLED_REL}"),
            format!("ConditionKernelCommandLine=!{CMDLINE_DISABLE}"),
            format!(
                "After=local-fs.target {} {VERIFY_UNIT}",
                crate::cross_family::FIRSTBOOT_UNIT
            ),
            "Before=systemd-user-sessions.service display-manager.service".to_string(),
            format!("ExecStart=-/etc/{REPAIR_BIN_REL} repair --firstboot"),
            format!("ExecStart=/usr/bin/rm -f /etc/{REPAIR_MARKER} /etc/{REPAIR_BIN_REL}"),
            "WantedBy=multi-user.target".to_string(),
        ] {
            assert!(unit.contains(&needle), "unit lost {needle}:\n{unit}");
        }
    }

    #[test]
    fn install_writes_unit_binary_marker_and_notification() {
        let dir = tempdir().unwrap();
        let etc = dir.path().join("etc");
        let bin = dir.path().join("fake-bin");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        let config = RepairConfig {
            source_machine_id: "abc".into(),
        };
        install_repair_unit(&etc, &bin, &config).unwrap();
        // Twice: a --force re-run restages over the first.
        install_repair_unit(&etc, &bin, &config).unwrap();

        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&etc.join(REPAIR_BIN_REL)), 0o755);
        assert_eq!(
            std::fs::read_to_string(etc.join("systemd/system").join(REPAIR_UNIT)).unwrap(),
            render_repair_unit()
        );
        assert_eq!(
            std::fs::read_link(
                etc.join("systemd/system/multi-user.target.wants")
                    .join(REPAIR_UNIT)
            )
            .unwrap()
            .to_string_lossy(),
            format!("../{REPAIR_UNIT}")
        );
        let marker: RepairConfig =
            serde_json::from_str(&std::fs::read_to_string(etc.join(REPAIR_MARKER)).unwrap())
                .unwrap();
        assert_eq!(marker, config);
        assert_eq!(mode(&etc.join(NOTIFY_SCRIPT_REL)), 0o755);
        assert!(
            std::fs::read_to_string(etc.join(NOTIFY_DESKTOP_REL))
                .unwrap()
                .contains("Exec=/bin/sh /etc/bootc-migrate/repair-notify.sh")
        );
    }

    /// A fixture root with every class's breakage; the full run repairs
    /// what it can without tools and reports the rest.
    fn fixture_root() -> tempfile::TempDir {
        let root = tempdir().unwrap();
        let p = root.path();
        let me = rustix::process::getuid().as_raw();
        let home = p.join("var/home/me");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("doc"), "x").unwrap();
        set_mtime(&home.join("doc"), Mtime(1_600_000_000, 0));
        let state = p.join("var").join(STATE_DIR_REL);
        write_identity_manifest(&p.join("var"), &state.join(IDENTITY_MANIFEST)).unwrap();
        set_mtime(&home.join("doc"), Mtime(1_900_000_000, 0));

        std::fs::create_dir_all(p.join("etc/bootc-migrate")).unwrap();
        std::fs::write(p.join("etc/machine-id"), "abc\n").unwrap();
        std::fs::write(
            p.join("etc").join(REPAIR_MARKER),
            r#"{"source_machine_id":"abc"}"#,
        )
        .unwrap();
        std::fs::write(
            p.join("etc/passwd"),
            format!("me:x:{me}:{me}::/var/home/me:/bin/bash\n"),
        )
        .unwrap();
        std::fs::write(
            state.join(VERIFY_REPORT),
            r#"{"generated":"x","findings":[
                {"class":"machine-id","detail":"live machine-id abc still equals the source's"},
                {"class":"missing-user","detail":"dbus (from /usr/lib/sysusers.d/dbus.conf)"}
            ]}"#,
        )
        .unwrap();
        root
    }

    #[test]
    fn firstboot_run_repairs_and_reports_on_a_fixture_root() {
        let root = fixture_root();
        let p = root.path();
        let run = FirstbootRun {
            root: p,
            cmdline: "quiet",
            dry_run: false,
            run_tools: false,
        };
        let log = run.run();
        let status = |c: RepairClass| log.classes.iter().find(|o| o.class == c).unwrap().status;
        assert_eq!(status(RepairClass::Ownership), Status::Repaired);
        assert_eq!(status(RepairClass::MachineId), Status::Repaired);
        assert_eq!(status(RepairClass::Flatpak), Status::Clean);
        assert_eq!(status(RepairClass::SelinuxLabel), Status::Clean);
        assert_eq!(
            std::fs::read_to_string(p.join("etc/machine-id")).unwrap(),
            ""
        );
        assert_eq!(
            std::fs::metadata(p.join("var/home/me/doc"))
                .unwrap()
                .mtime(),
            1_600_000_000
        );
        assert_eq!(log.report_only.len(), 1);
        assert_eq!(log.report_only[0].class, "missing-user");
        assert_eq!(log.verify_before.as_ref().map(Vec::len), Some(2));
        assert_eq!(log.summary(), "REPAIRED 2 FAILED 0 REPORTED 1");

        run.write_log(&log).unwrap();
        let state = p.join("var").join(STATE_DIR_REL);
        assert_eq!(
            std::fs::read_to_string(state.join(REPAIR_RESULT)).unwrap(),
            "REPAIRED 2 FAILED 0 REPORTED 1\n"
        );
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(state.join(REPAIR_LOG)).unwrap())
                .unwrap();
        assert_eq!(json["classes"][0]["class"], "ownership");
        assert_eq!(json["classes"][0]["status"], "repaired");
    }

    #[test]
    fn firstboot_run_honours_skips_disable_and_dry_run() {
        // Skip via file and kernel argument.
        let root = fixture_root();
        std::fs::write(root.path().join("etc").join(REPAIR_SKIP_REL), "ownership\n").unwrap();
        let log = FirstbootRun {
            root: root.path(),
            cmdline: "bootc_migrate.repair.skip=machine-id,nope",
            dry_run: false,
            run_tools: false,
        }
        .run();
        assert_eq!(log.classes[0].status, Status::Skipped);
        assert_eq!(log.classes[1].status, Status::Skipped);
        assert_eq!(log.unknown_skips, vec!["nope"]);
        assert_eq!(
            std::fs::read_to_string(root.path().join("etc/machine-id")).unwrap(),
            "abc\n"
        );

        // Dry run: reports, changes nothing.
        let root = fixture_root();
        let log = FirstbootRun {
            root: root.path(),
            cmdline: "",
            dry_run: true,
            run_tools: false,
        }
        .run();
        assert_eq!(log.classes[0].status, Status::Repaired);
        assert_eq!(
            std::fs::read_to_string(root.path().join("etc/machine-id")).unwrap(),
            "abc\n"
        );
        assert_eq!(
            std::fs::metadata(root.path().join("var/home/me/doc"))
                .unwrap()
                .mtime(),
            1_900_000_000
        );

        // Disabled: by flag file and by kernel argument.
        for (flag, cmdline) in [(true, ""), (false, "bootc_migrate.repair=0")] {
            let root = fixture_root();
            if flag {
                std::fs::write(root.path().join("etc").join(REPAIR_DISABLED_REL), "").unwrap();
            }
            let log = FirstbootRun {
                root: root.path(),
                cmdline,
                dry_run: false,
                run_tools: false,
            }
            .run();
            assert!(log.disabled.is_some());
            assert!(log.classes.is_empty());
            assert!(log.summary().starts_with("DISABLED"));
            assert_eq!(
                std::fs::read_to_string(root.path().join("etc/machine-id")).unwrap(),
                "abc\n"
            );
        }
    }

    #[test]
    fn cli_action_flags() {
        assert_eq!(
            cli_action(true, false, false, false).unwrap(),
            CliAction::Firstboot
        );
        assert_eq!(
            cli_action(false, false, false, false).unwrap(),
            CliAction::DryRun
        );
        assert_eq!(
            cli_action(false, true, false, false).unwrap(),
            CliAction::DryRun
        );
        assert_eq!(
            cli_action(false, false, true, false).unwrap(),
            CliAction::Disable
        );
        assert_eq!(
            cli_action(false, false, false, true).unwrap(),
            CliAction::Enable
        );
        assert!(cli_action(true, false, true, false).is_err());
    }

    #[test]
    fn notify_script_shape() {
        for needle in [
            "repair-result",
            "repair-log.json",
            "notify-send",
            "repair-notified",
            "exit 0",
        ] {
            assert!(NOTIFY_SCRIPT.contains(needle), "script lost {needle}");
        }
        assert!(NOTIFY_SCRIPT.starts_with("#!/bin/sh\n"));
    }
}
