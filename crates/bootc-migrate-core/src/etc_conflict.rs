//! Cross-base `/etc` conflict policy (issue #67 part 2, scenario C).
//!
//! # Why this is not a `mergetc` extension
//!
//! Within one base lineage, a 3-way `/etc` merge only ever sees *one* set of
//! vendor defaults changing, so "the user edited it, keep their version" is
//! the right answer: the user's edit was made against the same defaults the
//! new image ships. Across bases (Fedora-derived → CentOS-derived) *both*
//! sides move — the target ships a genuinely different default, and silently
//! keeping the user's Fedora-era file hides that entirely.
//!
//! [`crate::mergetc`] implements the same-lineage rule and is called by the
//! composefs conversion pipeline. The two routes that can actually be
//! cross-base — `bootc-rebase`'s `OstreeDeploy` and `ImageSwap` — do not call
//! it: they stage via `bootc switch`, whose native OSTree merge we neither
//! own nor want to replace. So the policy cannot be a `mergetc` parameter;
//! there is no call site to pass it through.
//!
//! # The seam this module uses instead
//!
//! `bootc switch` stages a deployment but does not reboot into it, and all
//! three merge inputs survive on disk afterwards:
//!
//! | input                    | where it lives after `bootc switch`        |
//! |--------------------------|--------------------------------------------|
//! | source vendor default    | `<booted deployment>/usr/etc`              |
//! | user's current value     | live `/etc`                                |
//! | target vendor default    | `<staged deployment>/usr/etc`              |
//! | what the native merge did| `<staged deployment>/etc` (writable)       |
//!
//! That makes the policy a **post-merge reconciliation pass**, not a second
//! merge: it re-derives nothing, and touches only the narrow set of paths
//! where all three inputs disagree — precisely the conflict class the native
//! merge has no way to reason about. Everything else is left exactly as
//! `bootc switch` produced it. This is the same "adjust the staged
//! deployment before first boot" seam part 1's UID/GID remap already uses
//! ([`crate::remap::apply_remap_plan`]).
//!
//! Per the decision recorded on #67: the target's default wins, and the
//! user's displaced value is preserved beside it as a
//! [`mergetc::REBASE_OLD_SUFFIX`] sidecar — nothing is ever destroyed.
//!
//! Planning ([`plan_etc_conflicts`]) is pure and operates on
//! [`EtcTriple`] values; reading the three trees ([`collect_etc_triples`])
//! and rewriting the staged `/etc` ([`apply_etc_conflict_plan`]) are
//! separate.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::mergetc::{REBASE_OLD_SUFFIX, is_identity_db};

/// `/etc`-relative paths that describe *this machine* rather than vendor
/// policy, and so are never replaced by the target image's default: the
/// target's factory copy cannot know this host's disks, and a wrong one
/// yields a deployment that does not mount its root or unlock its LUKS
/// volume. Identity databases are held exempt too, via
/// [`is_identity_db`] — on this route there is no union-merge to rescue
/// them (issue #80), so force-defaulting `/etc/passwd` would drop every
/// local account.
const EXEMPT_EXACT: &[&str] = &[
    // Storage / boot descriptors: machine-specific by construction.
    "fstab",
    "crypttab",
    "mdadm.conf",
    // Machine identity, same class as `machine-id` (already covered by
    // `is_identity_db`): taking the target's default here renames the host.
    "hostname",
];

/// `/etc`-relative path prefixes held exempt for the same reasons as
/// [`EXEMPT_EXACT`]. Matched as literal path prefixes against the
/// forward-slash-joined relative path.
const EXEMPT_PREFIXES: &[&str] = &[
    // Host keys: replacing them makes every client report a changed
    // fingerprint, and the target's factory copy has none worth taking.
    "ssh/ssh_host_",
    // Storage stacks whose config names local volumes/devices.
    "lvm/",
    "multipath/",
];

/// A path's state in one of the three `/etc` trees. Directories are not
/// represented: the policy only ever resolves leaf conflicts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EtcEntry {
    /// Regular file, with its full contents.
    File(Vec<u8>),
    /// Symlink, with its raw (unresolved) target.
    Symlink(String),
}

/// The three-way state of one `/etc`-relative path, as gathered by
/// [`collect_etc_triples`]. `None` means the path is absent from that tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EtcTriple {
    /// Path relative to `/etc`, using `/` separators (e.g. `dnf/dnf.conf`).
    pub path: String,
    /// The source image's vendor default (`<booted>/usr/etc`).
    pub source_default: Option<EtcEntry>,
    /// The user's live value (`/etc`).
    pub current: Option<EtcEntry>,
    /// The target image's vendor default (`<staged>/usr/etc`).
    pub target_default: Option<EtcEntry>,
}

/// What the policy decides for a single path. Absence of a decision
/// (`None` from [`classify`]) means "no action" — by far the common case,
/// and deliberately not recorded anywhere: the native merge's answer stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// All three inputs disagree and the path carries vendor policy: write
    /// the target's default, preserving the user's value as a sidecar.
    TakeTargetDefault,
    /// Same three-way conflict, but the path describes this machine or its
    /// identity. The user's value is carried unchanged; reported so the
    /// divergence is not invisible.
    ExemptKeepCurrent,
}

/// One path where the target's default displaced the user's value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EtcConflict {
    /// `/etc`-relative path taking the target image's default.
    pub path: String,
    /// `/etc`-relative path the user's displaced value is written to.
    pub sidecar: String,
}

/// The full policy outcome: what was rewritten, and what met the same
/// conflict condition but was held exempt.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct EtcConflictPlan {
    /// Paths where the target's default wins. Sorted by `path`.
    pub resolved: Vec<EtcConflict>,
    /// Exempt paths whose defaults also diverged, carried unchanged.
    /// Sorted.
    pub exempt: Vec<String>,
}

impl EtcConflictPlan {
    /// True when no path needs rewriting (there may still be exempt paths
    /// worth reporting).
    pub fn is_empty(&self) -> bool {
        self.resolved.is_empty()
    }

    /// The machine-readable twin of [`render_report`].
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("EtcConflictPlan serialization cannot fail")
    }
}

/// Whether `rel_path` is held exempt from being replaced by a target
/// default. See [`EXEMPT_EXACT`] / [`EXEMPT_PREFIXES`] / [`is_identity_db`].
pub fn is_exempt(rel_path: &str) -> bool {
    is_identity_db(rel_path)
        || EXEMPT_EXACT.contains(&rel_path)
        || EXEMPT_PREFIXES.iter().any(|p| rel_path.starts_with(p))
}

/// Decide a single path. Pure.
///
/// A path is a conflict only when it is present in **all three** trees with
/// the same kind, the user diverged from the source's default, the target
/// ships a *different* default (this is what makes it cross-base rather than
/// an ordinary customization), and the user's value is not already what the
/// target ships. Anything else returns `None` and is left to the native
/// merge.
pub fn classify(triple: &EtcTriple) -> Option<Decision> {
    // A sidecar left by an earlier re-base is an archive, not config; never
    // chain `.rebase-old.rebase-old` onto one.
    if triple.path.ends_with(REBASE_OLD_SUFFIX) {
        return None;
    }

    let (source, current, target) = match (
        &triple.source_default,
        &triple.current,
        &triple.target_default,
    ) {
        (Some(s), Some(c), Some(t)) => (s, c, t),
        // Absent from at least one tree: an added, deleted or newly-shipped
        // path, all of which the native 3-way merge already resolves the
        // same way this policy would.
        _ => return None,
    };

    // A file↔symlink kind change is a distinct hazard class (mergetc treats
    // it specially too). Rewriting across kinds post-merge would mean
    // deciding whether to unlink a directory-backed symlink; out of scope.
    let same_kind = matches!(
        (source, current, target),
        (EtcEntry::File(_), EtcEntry::File(_), EtcEntry::File(_))
            | (
                EtcEntry::Symlink(_),
                EtcEntry::Symlink(_),
                EtcEntry::Symlink(_)
            )
    );
    if !same_kind {
        return None;
    }

    let user_modified = current != source;
    let target_default_diverged = target != source;
    let already_at_target = current == target;

    if !user_modified || !target_default_diverged || already_at_target {
        return None;
    }

    if is_exempt(&triple.path) {
        Some(Decision::ExemptKeepCurrent)
    } else {
        Some(Decision::TakeTargetDefault)
    }
}

/// Classify every triple into a plan. Pure; output is sorted by path so the
/// report and its JSON twin are stable.
pub fn plan_etc_conflicts(triples: &[EtcTriple]) -> EtcConflictPlan {
    let mut plan = EtcConflictPlan::default();
    for triple in triples {
        match classify(triple) {
            Some(Decision::TakeTargetDefault) => plan.resolved.push(EtcConflict {
                path: triple.path.clone(),
                sidecar: format!("{}{REBASE_OLD_SUFFIX}", triple.path),
            }),
            Some(Decision::ExemptKeepCurrent) => plan.exempt.push(triple.path.clone()),
            None => {}
        }
    }
    plan.resolved.sort_by(|a, b| a.path.cmp(&b.path));
    plan.exempt.sort();
    plan
}

/// Render the end-of-run summary the #67 decision log calls for.
pub fn render_report(plan: &EtcConflictPlan) -> String {
    let mut out = String::new();
    out.push_str("=== Cross-base /etc conflict report ===\n");
    if plan.is_empty() {
        out.push_str(
            "No /etc path has a locally-modified value that the target image also \
             redefines — nothing to reconcile.\n",
        );
    } else {
        out.push_str("Target defaults taken; the previous value is preserved beside each path:\n");
        for c in &plan.resolved {
            out.push_str(&format!("  /etc/{:<44} -> /etc/{}\n", c.path, c.sidecar));
        }
    }
    if !plan.exempt.is_empty() {
        out.push_str(&format!(
            "Machine-specific paths whose defaults also diverged, kept as-is: {}\n",
            plan.exempt
                .iter()
                .map(|p| format!("/etc/{p}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    out
}

/// Read one path's entry from `base`. `Ok(None)` means the path is absent
/// (or is neither a file nor a symlink — a directory, socket, device node);
/// any other I/O failure propagates rather than masquerading as absence.
fn read_entry(base: &Path, rel_path: &str) -> Result<Option<EtcEntry>> {
    let full = base.join(rel_path);
    let meta = match fs::symlink_metadata(&full) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(e).with_context(|| format!("failed to stat {}", full.display()));
        }
    };
    if meta.is_symlink() {
        let target = fs::read_link(&full)
            .with_context(|| format!("failed to read symlink {}", full.display()))?;
        Ok(Some(EtcEntry::Symlink(
            target.to_string_lossy().into_owned(),
        )))
    } else if meta.is_file() {
        let content =
            fs::read(&full).with_context(|| format!("failed to read {}", full.display()))?;
        Ok(Some(EtcEntry::File(content)))
    } else {
        Ok(None)
    }
}

/// Collect every `/etc`-relative file/symlink path under `root`, using `/`
/// separators. Symlinks are recorded but never followed, so the walk stays
/// inside `root`.
fn collect_paths(root: &Path, prefix: &str, out: &mut Vec<String>) -> Result<()> {
    let entries = fs::read_dir(root)
        .with_context(|| format!("failed to list directory {}", root.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("failed to read entry in {}", root.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let meta = entry
            .metadata()
            .with_context(|| format!("failed to stat {}", entry.path().display()))?;
        if meta.is_dir() {
            // `DirEntry::metadata` does not traverse symlinks, so a symlink
            // to a directory lands in the `else` branch and is recorded as a
            // leaf rather than descended into.
            collect_paths(&entry.path(), &rel, out)?;
        } else {
            out.push(rel);
        }
    }
    Ok(())
}

/// Gather the three-way state for every path the source image's defaults
/// ship. Paths absent from the source's defaults are not candidates — the
/// policy only fires where *both* vendor lineages define a value — so
/// walking that tree alone bounds the work and the memory read.
///
/// Contents are held in memory so [`classify`] can stay pure and testable
/// without filesystem fixtures. The bound is the intersection of the two
/// images' vendor `/etc` trees, which is config text an image ships in its
/// own layer — single-digit megabytes, against a re-base that has just
/// pulled the target image itself.
pub fn collect_etc_triples(
    source_default_dir: &Path,
    current_dir: &Path,
    target_default_dir: &Path,
) -> Result<Vec<EtcTriple>> {
    let mut paths = Vec::new();
    collect_paths(source_default_dir, "", &mut paths)?;
    paths.sort();

    let mut triples = Vec::new();
    for path in paths {
        // Cheap rejection first: a path the target's defaults don't ship, or
        // that the policy would never rewrite anyway, is not worth reading.
        let target_default = read_entry(target_default_dir, &path)?;
        if target_default.is_none() {
            continue;
        }
        let source_default = read_entry(source_default_dir, &path)?;
        let current = read_entry(current_dir, &path)?;
        triples.push(EtcTriple {
            path,
            source_default,
            current,
            target_default,
        });
    }
    Ok(triples)
}

/// Apply `plan` to the staged deployment's `/etc`.
///
/// For each resolved conflict: the user's value (read from `current_dir`) is
/// written to the sidecar, then the target's default (read from
/// `target_default_dir`) replaces the merged file. Ownership, mode and
/// xattrs — including SELinux labels — follow the file each side's content
/// came from. Returns the number of paths rewritten.
///
/// Must run **after** [`crate::remap::apply_remap_plan`]: the content this
/// writes already carries the target's numeric ownership, and a remap pass
/// running afterwards could mistake a target id for a stale source id and
/// renumber it again. The sidecar deliberately keeps the source's ownership
/// — it is an inert archive of what the user had, not live configuration.
pub fn apply_etc_conflict_plan(
    staged_etc_dir: &Path,
    current_dir: &Path,
    target_default_dir: &Path,
    plan: &EtcConflictPlan,
) -> Result<usize> {
    for conflict in &plan.resolved {
        write_from(
            current_dir,
            &conflict.path,
            &staged_etc_dir.join(&conflict.sidecar),
        )
        .with_context(|| format!("failed to preserve /etc/{}", conflict.sidecar))?;
        write_from(
            target_default_dir,
            &conflict.path,
            &staged_etc_dir.join(&conflict.path),
        )
        .with_context(|| format!("failed to apply target default for /etc/{}", conflict.path))?;
    }
    Ok(plan.resolved.len())
}

/// Reproduce `src_base/rel_path` (file or symlink, with its metadata) at
/// `dest`, replacing whatever is there.
fn write_from(src_base: &Path, rel_path: &str, dest: &Path) -> Result<()> {
    let src = src_base.join(rel_path);
    let entry = read_entry(src_base, rel_path)?
        .ok_or_else(|| anyhow::anyhow!("{} vanished between planning and apply", src.display()))?;

    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    if fs::symlink_metadata(dest).is_ok() {
        fs::remove_file(dest).with_context(|| format!("failed to replace {}", dest.display()))?;
    }

    match entry {
        EtcEntry::Symlink(target) => {
            std::os::unix::fs::symlink(&target, dest)
                .with_context(|| format!("failed to create symlink {}", dest.display()))?;
        }
        EtcEntry::File(content) => {
            fs::write(dest, &content)
                .with_context(|| format!("failed to write {}", dest.display()))?;
            copy_mode_owner_xattrs(&src, dest)?;
        }
    }
    Ok(())
}

/// Copy mode, ownership and xattrs from `src` to `dst` (contents are already
/// written). Symlinks are excluded: their mode is meaningless and their
/// ownership is inherited from the process, matching what `bootc switch`'s
/// own merge leaves behind.
fn copy_mode_owner_xattrs(src: &Path, dst: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let meta = fs::metadata(src).with_context(|| format!("failed to stat {}", src.display()))?;
    let mut perms = fs::metadata(dst)
        .with_context(|| format!("failed to stat {}", dst.display()))?
        .permissions();
    perms.set_mode(meta.mode());
    fs::set_permissions(dst, perms)
        .with_context(|| format!("failed to set mode on {}", dst.display()))?;

    rustix::fs::chownat(
        rustix::fs::CWD,
        dst,
        Some(rustix::fs::Uid::from_raw(meta.uid())),
        Some(rustix::fs::Gid::from_raw(meta.gid())),
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
    )
    .with_context(|| format!("failed to set ownership on {}", dst.display()))?;

    crate::xattr::copy_xattrs(src, dst)
}

/// `<deployment root>/usr/etc` — where OSTree keeps an image's vendor `/etc`
/// defaults, leaving the deployment's own `/etc` as the writable merged
/// copy.
pub fn vendor_etc_dir(deployment_root: &Path) -> PathBuf {
    deployment_root.join("usr/etc")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(content: &str) -> Option<EtcEntry> {
        Some(EtcEntry::File(content.as_bytes().to_vec()))
    }

    fn link(target: &str) -> Option<EtcEntry> {
        Some(EtcEntry::Symlink(target.to_string()))
    }

    #[test]
    fn classify_covers_every_three_way_shape() {
        struct Case {
            name: &'static str,
            path: &'static str,
            source: Option<EtcEntry>,
            current: Option<EtcEntry>,
            target: Option<EtcEntry>,
            expected: Option<Decision>,
        }

        let cases = [
            Case {
                name: "all three differ on a policy file -> target default wins",
                path: "dnf/dnf.conf",
                source: file("fedora"),
                current: file("user"),
                target: file("centos"),
                expected: Some(Decision::TakeTargetDefault),
            },
            Case {
                name: "same-family: defaults agree, so the user edit is not a conflict",
                path: "dnf/dnf.conf",
                source: file("same"),
                current: file("user"),
                target: file("same"),
                expected: None,
            },
            Case {
                name: "user never diverged: native merge already took the new default",
                path: "dnf/dnf.conf",
                source: file("fedora"),
                current: file("fedora"),
                target: file("centos"),
                expected: None,
            },
            Case {
                name: "user's value already equals the target default",
                path: "dnf/dnf.conf",
                source: file("fedora"),
                current: file("centos"),
                target: file("centos"),
                expected: None,
            },
            Case {
                name: "absent from the target's defaults",
                path: "dnf/dnf.conf",
                source: file("fedora"),
                current: file("user"),
                target: None,
                expected: None,
            },
            Case {
                name: "absent from the source's defaults (user-added)",
                path: "dnf/dnf.conf",
                source: None,
                current: file("user"),
                target: file("centos"),
                expected: None,
            },
            Case {
                name: "deleted by the user",
                path: "dnf/dnf.conf",
                source: file("fedora"),
                current: None,
                target: file("centos"),
                expected: None,
            },
            Case {
                name: "symlinks all differ -> target default wins",
                path: "localtime",
                source: link("../usr/share/zoneinfo/UTC"),
                current: link("../usr/share/zoneinfo/Europe/Dublin"),
                target: link("../usr/share/zoneinfo/America/New_York"),
                expected: Some(Decision::TakeTargetDefault),
            },
            Case {
                name: "kind change between trees is out of scope",
                path: "resolv.conf",
                source: file("nameserver 1.1.1.1"),
                current: link("../run/systemd/resolve/stub-resolv.conf"),
                target: file("nameserver 8.8.8.8"),
                expected: None,
            },
            Case {
                name: "identity DB is exempt, never force-defaulted (#80)",
                path: "passwd",
                source: file("root:x:0:0"),
                current: file("root:x:0:0\njames:x:1000:1000"),
                target: file("root:x:0:0\nmessagebus:x:81:81"),
                expected: Some(Decision::ExemptKeepCurrent),
            },
            Case {
                name: "machine-id is exempt",
                path: "machine-id",
                source: file("aaaa"),
                current: file("bbbb"),
                target: file("cccc"),
                expected: Some(Decision::ExemptKeepCurrent),
            },
            Case {
                name: "fstab is exempt: the target cannot know this host's disks",
                path: "fstab",
                source: file("UUID=a / xfs defaults 0 0"),
                current: file("UUID=b / btrfs defaults 0 0"),
                target: file("UUID=c / ext4 defaults 0 0"),
                expected: Some(Decision::ExemptKeepCurrent),
            },
            Case {
                name: "crypttab is exempt",
                path: "crypttab",
                source: file("luks-a UUID=a none"),
                current: file("luks-b UUID=b none"),
                target: file("luks-c UUID=c none"),
                expected: Some(Decision::ExemptKeepCurrent),
            },
            Case {
                name: "hostname is machine identity, exempt",
                path: "hostname",
                source: file("localhost"),
                current: file("my-desktop"),
                target: file("centos"),
                expected: Some(Decision::ExemptKeepCurrent),
            },
            Case {
                name: "ssh host keys are exempt by prefix",
                path: "ssh/ssh_host_ed25519_key",
                source: file("src-key"),
                current: file("live-key"),
                target: file("tgt-key"),
                expected: Some(Decision::ExemptKeepCurrent),
            },
            Case {
                name: "ssh/sshd_config is NOT a host key and stays in policy",
                path: "ssh/sshd_config",
                source: file("PermitRootLogin no"),
                current: file("PermitRootLogin yes"),
                target: file("PermitRootLogin prohibit-password"),
                expected: Some(Decision::TakeTargetDefault),
            },
            Case {
                name: "lvm config is exempt by prefix",
                path: "lvm/lvm.conf",
                source: file("a"),
                current: file("b"),
                target: file("c"),
                expected: Some(Decision::ExemptKeepCurrent),
            },
            Case {
                name: "an existing sidecar is never re-displaced",
                path: "dnf/dnf.conf.rebase-old",
                source: file("fedora"),
                current: file("user"),
                target: file("centos"),
                expected: None,
            },
        ];

        for case in cases {
            let triple = EtcTriple {
                path: case.path.to_string(),
                source_default: case.source,
                current: case.current,
                target_default: case.target,
            };
            assert_eq!(classify(&triple), case.expected, "case: {}", case.name);
        }
    }

    #[test]
    fn plan_sorts_and_names_sidecars() {
        let triples = vec![
            EtcTriple {
                path: "zz.conf".to_string(),
                source_default: file("s"),
                current: file("c"),
                target_default: file("t"),
            },
            EtcTriple {
                path: "aa.conf".to_string(),
                source_default: file("s"),
                current: file("c"),
                target_default: file("t"),
            },
            EtcTriple {
                path: "fstab".to_string(),
                source_default: file("s"),
                current: file("c"),
                target_default: file("t"),
            },
            EtcTriple {
                path: "untouched.conf".to_string(),
                source_default: file("s"),
                current: file("s"),
                target_default: file("t"),
            },
        ];

        let plan = plan_etc_conflicts(&triples);
        assert_eq!(
            plan.resolved,
            vec![
                EtcConflict {
                    path: "aa.conf".to_string(),
                    sidecar: "aa.conf.rebase-old".to_string(),
                },
                EtcConflict {
                    path: "zz.conf".to_string(),
                    sidecar: "zz.conf.rebase-old".to_string(),
                },
            ]
        );
        assert_eq!(plan.exempt, vec!["fstab".to_string()]);
        assert!(!plan.is_empty());
    }

    #[test]
    fn empty_plan_reports_nothing_to_reconcile() {
        let plan = EtcConflictPlan::default();
        assert!(plan.is_empty());
        let report = render_report(&plan);
        assert!(
            report.contains("nothing to reconcile"),
            "unexpected report: {report}"
        );
        assert_eq!(
            plan.to_json(),
            "{\n  \"resolved\": [],\n  \"exempt\": []\n}"
        );
    }

    #[test]
    fn report_lists_each_rewrite_and_each_exemption() {
        let plan = EtcConflictPlan {
            resolved: vec![EtcConflict {
                path: "dnf/dnf.conf".to_string(),
                sidecar: "dnf/dnf.conf.rebase-old".to_string(),
            }],
            exempt: vec!["fstab".to_string()],
        };
        let report = render_report(&plan);
        assert!(
            report.contains("/etc/dnf/dnf.conf") && report.contains("/etc/dnf/dnf.conf.rebase-old"),
            "unexpected report: {report}"
        );
        assert!(report.contains("/etc/fstab"), "unexpected report: {report}");

        let json: serde_json::Value = serde_json::from_str(&plan.to_json()).unwrap();
        assert_eq!(json["resolved"][0]["path"], "dnf/dnf.conf");
        assert_eq!(json["resolved"][0]["sidecar"], "dnf/dnf.conf.rebase-old");
        assert_eq!(json["exempt"][0], "fstab");
    }

    /// Build the three trees on disk, collect, plan and apply — the whole
    /// module end to end, minus the live `bootc switch` that produces the
    /// staged deployment.
    #[test]
    fn collect_plan_apply_round_trip() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source-usr-etc");
        let current = tmp.path().join("live-etc");
        let target = tmp.path().join("target-usr-etc");
        let staged = tmp.path().join("staged-etc");
        for dir in [&source, &current, &target, &staged] {
            fs::create_dir_all(dir.join("dnf")).unwrap();
        }

        // Conflict: all three differ.
        fs::write(source.join("dnf/dnf.conf"), b"[main]\nsource\n").unwrap();
        fs::write(current.join("dnf/dnf.conf"), b"[main]\nuser\n").unwrap();
        fs::write(target.join("dnf/dnf.conf"), b"[main]\ntarget\n").unwrap();
        // The native merge kept the user's value; that is what we replace.
        fs::write(staged.join("dnf/dnf.conf"), b"[main]\nuser\n").unwrap();
        fs::set_permissions(
            target.join("dnf/dnf.conf"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();

        // Exempt: all three differ, but it is fstab.
        fs::write(source.join("fstab"), b"source\n").unwrap();
        fs::write(current.join("fstab"), b"user\n").unwrap();
        fs::write(target.join("fstab"), b"target\n").unwrap();
        fs::write(staged.join("fstab"), b"user\n").unwrap();

        // Untouched: the user never diverged.
        fs::write(source.join("issue"), b"source\n").unwrap();
        fs::write(current.join("issue"), b"source\n").unwrap();
        fs::write(target.join("issue"), b"target\n").unwrap();
        fs::write(staged.join("issue"), b"target\n").unwrap();

        // Symlink conflict.
        std::os::unix::fs::symlink("../usr/share/zoneinfo/UTC", source.join("localtime")).unwrap();
        std::os::unix::fs::symlink(
            "../usr/share/zoneinfo/Europe/Dublin",
            current.join("localtime"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            "../usr/share/zoneinfo/America/New_York",
            target.join("localtime"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            "../usr/share/zoneinfo/Europe/Dublin",
            staged.join("localtime"),
        )
        .unwrap();

        // Present only in the source's defaults: not a candidate.
        fs::write(source.join("source-only.conf"), b"source\n").unwrap();
        fs::write(current.join("source-only.conf"), b"user\n").unwrap();

        let triples = collect_etc_triples(&source, &current, &target).unwrap();
        let collected: Vec<&str> = triples.iter().map(|t| t.path.as_str()).collect();
        assert_eq!(
            collected,
            vec!["dnf/dnf.conf", "fstab", "issue", "localtime"],
            "source-only paths must not become candidates"
        );

        let plan = plan_etc_conflicts(&triples);
        assert_eq!(
            plan.resolved
                .iter()
                .map(|c| c.path.as_str())
                .collect::<Vec<_>>(),
            vec!["dnf/dnf.conf", "localtime"]
        );
        assert_eq!(plan.exempt, vec!["fstab".to_string()]);

        assert_eq!(
            apply_etc_conflict_plan(&staged, &current, &target, &plan).unwrap(),
            2
        );

        // Target default applied, user's value preserved beside it.
        assert_eq!(
            fs::read(staged.join("dnf/dnf.conf")).unwrap(),
            b"[main]\ntarget\n"
        );
        assert_eq!(
            fs::read(staged.join("dnf/dnf.conf.rebase-old")).unwrap(),
            b"[main]\nuser\n"
        );
        assert_eq!(
            fs::metadata(staged.join("dnf/dnf.conf"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "mode must follow the target's own file"
        );

        // Symlink replaced in place, old target archived as a symlink.
        assert_eq!(
            fs::read_link(staged.join("localtime")).unwrap(),
            PathBuf::from("../usr/share/zoneinfo/America/New_York")
        );
        assert_eq!(
            fs::read_link(staged.join("localtime.rebase-old")).unwrap(),
            PathBuf::from("../usr/share/zoneinfo/Europe/Dublin")
        );

        // Exempt and non-conflicting paths are untouched, with no sidecars.
        assert_eq!(fs::read(staged.join("fstab")).unwrap(), b"user\n");
        assert!(!staged.join("fstab.rebase-old").exists());
        assert_eq!(fs::read(staged.join("issue")).unwrap(), b"target\n");
        assert!(!staged.join("issue.rebase-old").exists());
    }

    #[test]
    fn vendor_etc_dir_is_usr_etc_under_the_deployment() {
        assert_eq!(
            vendor_etc_dir(Path::new("/ostree/deploy/default/deploy/abc.0")),
            PathBuf::from("/ostree/deploy/default/deploy/abc.0/usr/etc")
        );
    }
}
