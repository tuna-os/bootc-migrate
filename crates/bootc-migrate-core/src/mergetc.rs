use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs as unix_fs;
use std::path::Path;

/// Result of reading a file for 3-way merge. None means the file does not exist
/// in that version.
#[derive(Debug)]
pub struct MergeContext {
    pub old_default: Option<Vec<u8>>,
    pub current: Option<Vec<u8>>,
    pub new_default: Option<Vec<u8>>,
}

/// Recorded entry from scanning a directory tree: either a regular file
/// (with its content hash-ish) or a symlink target.
#[derive(Debug)]
enum DirEntry {
    File,
    Symlink(String),
}

/// Perform a 3-way /etc merge following the algorithm used by
/// bootc's composefs finalize logic:
///
/// For each file/symlink in the union of (old_default, current, new_default):
///   - If current == old_default: take new_default (upstream update wins)
///   - Otherwise: keep current (user customization wins)
///
/// Symlinks: if the user hasn't changed the target, take upstream's target.
/// Permissions and xattrs are preserved from the chosen source.
pub fn merge_etc_files(
    old_default_dir: &Path,
    current_dir: &Path,
    new_default_dir: &Path,
    output_dir: &Path,
) -> Result<()> {
    merge_etc_files_with_overrides(
        old_default_dir,
        current_dir,
        new_default_dir,
        output_dir,
        None,
    )
}

/// Same as [`merge_etc_files`], but accepts per-path override decisions from
/// an interactive Config Drift Review (issue #15). See [`EtcDriftManifest`]
/// for the override semantics; `merge_etc_files` is just this function
/// called with `overrides: None`.
pub fn merge_etc_files_with_overrides(
    old_default_dir: &Path,
    current_dir: &Path,
    new_default_dir: &Path,
    output_dir: &Path,
    overrides: Option<&EtcDriftManifest>,
) -> Result<()> {
    fs::create_dir_all(output_dir)?;

    // Collect all paths relative to their roots (files + symlinks).
    let old_entries = collect_relative_entries(old_default_dir)?;
    let cur_entries = collect_relative_entries(current_dir)?;
    let new_entries = collect_relative_entries(new_default_dir)?;

    let mut all_paths: Vec<String> = old_entries
        .keys()
        .chain(cur_entries.keys())
        .chain(new_entries.keys())
        .cloned()
        .collect();
    all_paths.sort();
    all_paths.dedup();

    for rel_path in &all_paths {
        let old_entry = old_entries.get(rel_path);
        let cur_entry = cur_entries.get(rel_path);
        let new_entry = new_entries.get(rel_path);

        // Decide whether the user has modified this path relative to the
        // source's factory. If old==cur in *both type and content*, the user
        // didn't touch it — the 3-way result is whatever `new` provides
        // (including type changes like symlink→file across image lineages).
        // Otherwise the user's version wins.
        let user_modified = match (old_entry, cur_entry) {
            (Some(DirEntry::File), Some(DirEntry::File)) => {
                read_file_at(old_default_dir, rel_path) != read_file_at(current_dir, rel_path)
            }
            (Some(DirEntry::Symlink(o)), Some(DirEntry::Symlink(c))) => o != c,
            (None, Some(_)) => true,    // user added
            (Some(_), None) => true,    // user deleted
            (Some(_), Some(_)) => true, // type change (file↔symlink) by user
            (None, None) => false,
        };

        // A drift-review decision for this path overrides the default rule:
        // `Some(false)` forces the target's new default even if the user
        // modified the path (`forced_default`); `Some(true)` forces keeping
        // current. `None` (no decision, or an identity-DB path — see
        // `EtcDriftManifest::decision_for`) falls back to `user_modified`.
        let decision = overrides.and_then(|m| m.decision_for(rel_path));
        let (chosen_entry, forced_default): (Option<&DirEntry>, bool) = match decision {
            Some(true) => (cur_entry, false),
            Some(false) => (new_entry, true),
            None => (if user_modified { cur_entry } else { new_entry }, false),
        };

        // When a path the user genuinely changed is force-defaulted, keep
        // their version around instead of silently discarding it — mirrors
        // the `.rebase-old` sidecar convention already documented for
        // cross-base /etc conflicts (see ROADMAP.md's decision log).
        if forced_default
            && user_modified
            && let Some(cur) = cur_entry
        {
            write_rebase_old_sidecar(cur, current_dir, output_dir, rel_path)?;
        }

        match (chosen_entry, cur_entry, new_entry) {
            (Some(DirEntry::Symlink(target)), _, _) => {
                let dest = output_dir.join(rel_path);
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)?;
                }
                if dest.exists() || dest.is_symlink() {
                    let _ = fs::remove_file(&dest);
                }
                std::os::unix::fs::symlink(target, &dest)
                    .with_context(|| format!("failed to create symlink: {}", dest.display()))?;
            }
            (Some(DirEntry::File), _, _) => {
                // File merge path — also covers cross-image preservation
                // for files that existed in source factory but the target
                // image doesn't ship.
                let old = read_file_at(old_default_dir, rel_path);
                let cur = read_file_at(current_dir, rel_path);
                let new = read_file_at(new_default_dir, rel_path);

                let chosen = if forced_default {
                    // Drift-review override: take the target's new default
                    // outright, bypassing the normal 3-way rule. Identity-DB
                    // paths never reach this branch (`decision_for` excludes
                    // them), so the union-merge safety net below still
                    // applies to them unconditionally.
                    new.clone()
                } else if is_identity_db(rel_path) {
                    // Line-merge identity DBs (passwd/shadow/group/gshadow/sub{uid,gid}):
                    // start with the source's live file (preserves accumulated user
                    // state), then append entries from the target whose first colon-
                    // delimited key (username/groupname) isn't already present. This
                    // covers two failure modes the plain 3-way rule mishandles:
                    //   1. Bluefin uses dbus-broker so its passwd has no `messagebus`
                    //      user; Dakota uses classic dbus and needs it. Without this,
                    //      dbus.service fails with "Unknown user 'messagebus'" 217/USER
                    //      and the whole bus/polkit/logind/sshd stack cascade-fails.
                    //   2. The reverse: target's factory file would otherwise drop
                    //      every system user the source had accumulated (e.g. polkitd).
                    // machine-id keeps current verbatim (identity preservation).
                    merge_identity_db(rel_path, cur.as_deref(), new.as_deref())
                } else {
                    choose_merged_content(&MergeContext {
                        old_default: old,
                        current: cur,
                        new_default: new,
                    })
                };

                if let Some(content) = chosen {
                    let dest = output_dir.join(rel_path);
                    if let Some(parent) = dest.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::write(&dest, &content).with_context(|| {
                        format!("failed to write merged file: {}", dest.display())
                    })?;

                    // Preserve xattrs and permissions from the best available source.
                    // A forced-default path prefers the target's own metadata over
                    // the user's stale file; otherwise prefer current, then
                    // new_default, then old_default.
                    let xattr_src = if forced_default && new_default_dir.join(rel_path).exists() {
                        new_default_dir.join(rel_path)
                    } else if current_dir.join(rel_path).exists() {
                        current_dir.join(rel_path)
                    } else if new_default_dir.join(rel_path).exists() {
                        new_default_dir.join(rel_path)
                    } else {
                        old_default_dir.join(rel_path)
                    };
                    copy_file_metadata(&xattr_src, &dest)?;
                }
            }
            (None, _, _) => {
                // Dropped on purpose: 3-way result is "absent" (e.g. user
                // didn't modify and new image doesn't ship it; or user
                // deleted and image agreed; or a forced-default override and
                // the target doesn't ship this path either).
            }
        }
    }

    Ok(())
}

/// Preserve the user's pre-override version of `rel_path` as a
/// `<rel_path>.rebase-old` sidecar in `output_dir`, before a drift-review
/// override replaces the merged result with the target's default. Mirrors
/// the `.rebase-old` convention already documented for cross-base /etc
/// conflicts (ROADMAP.md's decision log: "target defaults win, user value
/// kept as `.rebase-old` sidecar").
fn write_rebase_old_sidecar(
    cur_entry: &DirEntry,
    current_dir: &Path,
    output_dir: &Path,
    rel_path: &str,
) -> Result<()> {
    let sidecar_rel = format!("{rel_path}.rebase-old");
    let dest = output_dir.join(&sidecar_rel);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    match cur_entry {
        DirEntry::Symlink(target) => {
            if dest.exists() || dest.is_symlink() {
                let _ = fs::remove_file(&dest);
            }
            std::os::unix::fs::symlink(target, &dest).with_context(|| {
                format!(
                    "failed to write .rebase-old symlink sidecar: {}",
                    dest.display()
                )
            })?;
        }
        DirEntry::File => {
            let src = current_dir.join(rel_path);
            let content = fs::read(&src).with_context(|| {
                format!("failed to read {} for .rebase-old sidecar", src.display())
            })?;
            fs::write(&dest, &content).with_context(|| {
                format!("failed to write .rebase-old sidecar: {}", dest.display())
            })?;
            copy_file_metadata(&src, &dest)?;
        }
    }
    Ok(())
}

/// How a path under live `/etc` differs from the OSTree factory default
/// (issue #15's "Config Drift Review" categorization: Added/Modified/Removed,
/// plus the symlink↔file type-change case `mergetc`'s own 3-way merge
/// already treats specially — see `merge_etc_files`'s `user_modified` logic).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DriftKind {
    /// Present in live `/etc`, absent from the factory default.
    Added,
    /// Present in both, content or symlink target differs.
    Modified,
    /// Present in the factory default, absent from live `/etc`.
    Removed,
    /// Present in both but as different types (file vs symlink).
    TypeChanged,
}

/// One path's drift entry, relative to `/etc`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EtcDriftEntry {
    pub path: String,
    pub kind: DriftKind,
}

/// Per-path override decisions produced by the interactive "Config Drift
/// Review" checklist (issue #15). Maps a path relative to `/etc` (matching
/// [`EtcDriftEntry::path`]) to the user's decision for that path:
///
/// - `true`  — keep the current (live) version. This is a no-op relative to
///   `merge_etc_files`'s normal 3-way rule for a path the user actually
///   modified, but for a path the user did *not* modify it pins the current
///   version instead of upgrading to the target's new default.
/// - `false` — take the target image's new default instead, even if the
///   normal 3-way rule would otherwise have kept the user's version. If the
///   user's version genuinely differed, it is preserved next to the merged
///   output as a `<path>.rebase-old` sidecar instead of being discarded.
///
/// Paths absent from `decisions` are not overridden: `merge_etc_files`
/// falls back to its normal rule for them, so an empty or missing manifest
/// reproduces today's default behavior exactly (see `default_for_drift`).
///
/// Identity-DB files (passwd/shadow/group/gshadow/sub{uid,gid}/machine-id)
/// are never overridden even if present in `decisions` — see
/// [`EtcDriftManifest::decision_for`]. Replacing them outright with a
/// target's factory copy can strand local accounts, so they always go
/// through `merge_identity_db`'s union-merge regardless of the drift-review
/// choice.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EtcDriftManifest {
    pub decisions: HashMap<String, bool>,
}

impl EtcDriftManifest {
    /// Build the manifest matching today's default merge behavior: every
    /// drift entry pre-selected "keep current" (checked). Applying this
    /// manifest without the user changing anything is a no-op relative to
    /// not supplying a manifest at all — matching issue #15's requirement
    /// that behavior doesn't silently change for anyone who doesn't
    /// interact with the review screen.
    pub fn default_for_drift(drift: &[EtcDriftEntry]) -> Self {
        let decisions = drift.iter().map(|e| (e.path.clone(), true)).collect();
        Self { decisions }
    }

    /// Parse a manifest from its JSON representation. Pure — no I/O; callers
    /// do their own file reading (see `bootc-migrate`'s CLI wiring).
    pub fn parse(json: &str) -> Result<Self> {
        serde_json::from_str(json).context("failed to parse etc-drift manifest JSON")
    }

    /// Serialize to pretty JSON.
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string_pretty(self).context("failed to serialize etc-drift manifest")
    }

    /// The effective override decision for `rel_path`, or `None` if the
    /// path isn't in the manifest or is an identity-DB file (see the struct
    /// docs — those are never overridden).
    fn decision_for(&self, rel_path: &str) -> Option<bool> {
        if is_identity_db(rel_path) {
            return None;
        }
        self.decisions.get(rel_path).copied()
    }
}

/// Diff the OSTree factory default `/etc` against live `/etc` (issue #15).
/// Pure and read-only — computes what a "Config Drift Review" screen would
/// show, without touching the 3-way merge's `new_default` (target image)
/// side at all: this is specifically the *user's own* drift, independent of
/// which target they migrate to.
pub fn diff_etc_factory_vs_live(factory_dir: &Path, live_dir: &Path) -> Result<Vec<EtcDriftEntry>> {
    let factory_entries = collect_relative_entries(factory_dir)?;
    let live_entries = collect_relative_entries(live_dir)?;

    let mut all_paths: Vec<&String> = factory_entries.keys().chain(live_entries.keys()).collect();
    all_paths.sort();
    all_paths.dedup();

    let mut drift = Vec::new();
    for rel_path in all_paths {
        let factory = factory_entries.get(rel_path);
        let live = live_entries.get(rel_path);
        let kind = match (factory, live) {
            (None, Some(_)) => Some(DriftKind::Added),
            (Some(_), None) => Some(DriftKind::Removed),
            (Some(DirEntry::File), Some(DirEntry::File)) => {
                if read_file_at(factory_dir, rel_path) != read_file_at(live_dir, rel_path) {
                    Some(DriftKind::Modified)
                } else {
                    None
                }
            }
            (Some(DirEntry::Symlink(f)), Some(DirEntry::Symlink(l))) => {
                if f != l {
                    Some(DriftKind::Modified)
                } else {
                    None
                }
            }
            (Some(_), Some(_)) => Some(DriftKind::TypeChanged),
            (None, None) => None,
        };
        if let Some(kind) = kind {
            drift.push(EtcDriftEntry {
                path: rel_path.clone(),
                kind,
            });
        }
    }
    Ok(drift)
}

/// Union-merge an identity-DB file by colon-delimited first field.
/// `current` lines come first (verbatim, preserving order and state); any line
/// from `new` whose first field isn't already represented gets appended.
/// machine-id is opaque — return current as-is.
fn merge_identity_db(
    rel_path: &str,
    current: Option<&[u8]>,
    new: Option<&[u8]>,
) -> Option<Vec<u8>> {
    if rel_path == "machine-id" {
        return current
            .map(|s| s.to_vec())
            .or_else(|| new.map(|s| s.to_vec()));
    }
    let cur_text = match current {
        Some(c) => std::str::from_utf8(c).ok()?,
        None => return new.map(|s| s.to_vec()),
    };
    let new_text = match new {
        Some(n) => std::str::from_utf8(n).ok()?,
        None => return Some(cur_text.as_bytes().to_vec()),
    };
    let key_of = |line: &str| line.split(':').next().unwrap_or("").to_string();
    let mut keys: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out = String::with_capacity(cur_text.len() + new_text.len());
    for line in cur_text.lines() {
        if !line.is_empty() {
            keys.insert(key_of(line));
        }
        out.push_str(line);
        out.push('\n');
    }
    for line in new_text.lines() {
        if line.is_empty() {
            continue;
        }
        let k = key_of(line);
        if !keys.contains(&k) {
            out.push_str(line);
            out.push('\n');
            keys.insert(k);
        }
    }
    Some(out.into_bytes())
}

/// Identity-database files whose contents accumulate system state and must
/// never be replaced by the target image's factory copy during /etc merge.
fn is_identity_db(rel_path: &str) -> bool {
    matches!(
        rel_path,
        "passwd"
            | "passwd-"
            | "shadow"
            | "shadow-"
            | "group"
            | "group-"
            | "gshadow"
            | "gshadow-"
            | "subuid"
            | "subuid-"
            | "subgid"
            | "subgid-"
            | "machine-id"
    )
}

/// Walk `etc_dir` and remove any symlink whose target doesn't exist. The
/// target is resolved against:
///   - `target_root` for absolute targets pointing under `/usr/*` (the new
///     image's read-only root)
///   - the symlink's own parent directory for relative targets
///   - the merged `etc_dir` itself for absolute targets pointing under
///     `/etc/*` (the symlink references something within /etc that another
///     merge step may or may not have produced)
///
/// Why: the 3-way merge brings forward enablement symlinks from the source
/// OS's /etc — `/etc/systemd/system/dbus.service → /usr/lib/systemd/system/dbus-broker.service`
/// (target lacks dbus-broker) or `/etc/pam.d/password-auth → /etc/authselect/password-auth`
/// (target doesn't use authselect). Either kind leaves systemd or PAM
/// reporting "No such file or directory" after pivot.
pub fn prune_dangling_symlinks(etc_dir: &Path, target_root: &Path) -> Result<usize> {
    let mut removed = 0usize;
    prune_recursive(etc_dir, etc_dir, target_root, &mut removed)?;
    Ok(removed)
}

/// Legacy name retained so existing call sites keep working. Same behavior
/// as `prune_dangling_symlinks` now — the implementation no longer restricts
/// to `/usr/*` targets.
#[allow(dead_code)]
pub fn prune_dangling_usr_symlinks(etc_dir: &Path, target_root: &Path) -> Result<usize> {
    prune_dangling_symlinks(etc_dir, target_root)
}

fn prune_recursive(
    dir: &Path,
    etc_root: &Path,
    target_root: &Path,
    removed: &mut usize,
) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            let target = match fs::read_link(&path) {
                Ok(t) => t,
                Err(_) => continue,
            };
            let target_str = target.to_string_lossy().to_string();
            let resolved = if target_str.starts_with('/') {
                if let Some(usr_rel) = target_str.strip_prefix("/usr/") {
                    target_root.join("usr").join(usr_rel)
                } else if let Some(etc_rel) = target_str.strip_prefix("/etc/") {
                    etc_root.join(etc_rel)
                } else {
                    // Other absolute target (e.g. /run, /var) — resolve against
                    // target_root; if it doesn't exist there it's still likely
                    // fine at runtime (e.g. /run/...), so skip the check.
                    continue;
                }
            } else {
                // Relative target — resolve against the symlink's parent.
                path.parent().unwrap_or(Path::new("/")).join(&target_str)
            };
            if fs::metadata(&resolved).is_err() {
                eprintln!(
                    "[phase4] pruning dangling /etc symlink: {} -> {}",
                    path.display(),
                    target_str
                );
                fs::remove_file(&path).with_context(|| {
                    format!("failed to remove dangling symlink {}", path.display())
                })?;
                *removed += 1;
            }
        } else if ft.is_dir() {
            prune_recursive(&path, etc_root, target_root, removed)?;
        }
    }
    Ok(())
}

/// Copy extended attributes and permissions from src file to dst file (no data copy).
fn copy_file_metadata(src: &Path, dst: &Path) -> Result<()> {
    // Copy permissions
    if let Ok(meta) = fs::metadata(src) {
        let mode = unix_fs::PermissionsExt::mode(&meta.permissions());
        let mut perms = fs::metadata(dst)?.permissions();
        unix_fs::PermissionsExt::set_mode(&mut perms, mode);
        let _ = fs::set_permissions(dst, perms);
    }
    // Copy xattrs (SELinux labels, capabilities) via the shared helper.
    crate::xattr::copy_xattrs(src, dst)?;
    Ok(())
}

fn read_file_at(base: &Path, rel_path: &str) -> Option<Vec<u8>> {
    let full = base.join(rel_path);
    // Use symlink_metadata to avoid following symlinks.
    match fs::symlink_metadata(&full) {
        Ok(meta) if meta.is_file() => fs::read(&full).ok(),
        _ => None,
    }
}

/// Core 3-way merge decision. Public so unit tests can exercise it directly.
pub fn choose_merged_content(ctx: &MergeContext) -> Option<Vec<u8>> {
    match (&ctx.old_default, &ctx.current, &ctx.new_default) {
        // File exists in all three. If the user left it untouched (old == cur),
        // take the target's version; otherwise the user changed it, so keep
        // theirs (last-writer-wins on a genuine three-way conflict).
        (Some(old), Some(cur), Some(new)) => {
            if old == cur {
                Some(new.clone())
            } else {
                Some(cur.clone())
            }
        }
        // File in old and current, absent in new.
        // ComposeFS 3-way merge (bootc etc-merge crate) semantic:
        // - If old==cur (user didn't touch it), the diff is empty for this
        //   file and merge uses the new target's version — which doesn't
        //   have it, so the file is dropped. This correctly removes
        //   source-specific system files like sshd_config.d/40-redhat-*
        //   that would break the target.
        // - If old!=cur (user modified the file), the diff captures the
        //   user's change and merge applies it to new. Since new doesn't
        //   have the file, the user's version is kept.
        (Some(old), Some(cur), None) => {
            if old == cur {
                None
            } else {
                Some(cur.clone())
            }
        }
        // File only in current (user-added file)
        (None, Some(cur), None) => Some(cur.clone()),
        // File in new only (upstream-added file)
        (None, None, Some(new)) => Some(new.clone()),
        // File in old and new, not in current (user deleted it)
        (Some(_old), None, Some(_new)) => None,
        // File only in old (deleted upstream, also deleted by user) — drop
        (Some(_old), None, None) => None,
        // File in current and new but never in old (unusual): keep current.
        (None, Some(cur), Some(_new)) => Some(cur.clone()),
        _ => None,
    }
}

/// Collect relative paths for files AND symlinks under a directory tree.
fn collect_relative_entries(root: &Path) -> Result<HashMap<String, DirEntry>> {
    let mut entries = HashMap::new();
    if !root.exists() {
        return Ok(entries);
    }
    collect_entries_recursive(root, root, &mut entries)?;
    Ok(entries)
}

fn collect_entries_recursive(
    dir: &Path,
    root: &Path,
    entries: &mut HashMap<String, DirEntry>,
) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();
        let ft = entry.file_type()?;

        if ft.is_dir() {
            collect_entries_recursive(&path, root, entries)?;
        } else if ft.is_symlink() {
            let target = fs::read_link(&path)?.to_string_lossy().to_string();
            entries.insert(rel, DirEntry::Symlink(target));
        } else if ft.is_file() {
            entries.insert(rel, DirEntry::File);
        }
        // Skip special files.
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn merge_passwd_preserves_current_and_adds_target_only_users() {
        // Source (Bluefin) uses dbus-broker → no `messagebus` user in its passwd.
        // Target (Dakota) uses classic dbus → has `messagebus`. Result must contain
        // every current user (root, polkitd, sshd) AND the target-only `messagebus`,
        // otherwise dbus.service 217/USERs at start.
        let old = tempdir().unwrap();
        let cur = tempdir().unwrap();
        let new = tempdir().unwrap();
        let out = tempdir().unwrap();

        let bluefin = "root:x:0:0::/root:/bin/bash\npolkitd:x:973:973::/:/usr/sbin/nologin\nsshd:x:74:74::/usr/share/empty.sshd:/sbin/nologin\n";
        fs::write(old.path().join("passwd"), bluefin).unwrap();
        fs::write(cur.path().join("passwd"), bluefin).unwrap();
        let dakota =
            "root:x:0:0::/root:/bin/bash\nmessagebus:x:81:81::/run/dbus:/usr/sbin/nologin\n";
        fs::write(new.path().join("passwd"), dakota).unwrap();

        merge_etc_files(old.path(), cur.path(), new.path(), out.path()).unwrap();
        let merged = fs::read_to_string(out.path().join("passwd")).unwrap();
        assert!(
            merged.contains("messagebus:"),
            "messagebus must be added from target: {merged}"
        );
        assert!(
            merged.contains("polkitd:"),
            "polkitd from current must survive: {merged}"
        );
        assert!(
            merged.contains("sshd:"),
            "sshd from current must survive: {merged}"
        );
        // Current entries appear before any new-only additions.
        let msg_idx = merged.find("messagebus:").unwrap();
        let polk_idx = merged.find("polkitd:").unwrap();
        assert!(polk_idx < msg_idx);
    }

    #[test]
    fn merge_symlink_to_file_type_change_takes_new() {
        // Regression: Bluefin's /etc/pam.d/password-auth is a symlink
        // → /etc/authselect/password-auth. Dakota ships it as a regular file.
        // The user didn't modify it (old==cur), so the merge MUST take new
        // (the regular file). Previously the dispatcher unconditionally kept
        // the cur symlink, leaving PAM broken and sshd disconnecting.
        let old = tempdir().unwrap();
        let cur = tempdir().unwrap();
        let new = tempdir().unwrap();
        let out = tempdir().unwrap();
        std::os::unix::fs::symlink(
            "/etc/authselect/password-auth",
            old.path().join("password-auth"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            "/etc/authselect/password-auth",
            cur.path().join("password-auth"),
        )
        .unwrap();
        fs::write(
            new.path().join("password-auth"),
            b"auth required pam_unix.so\n",
        )
        .unwrap();

        merge_etc_files(old.path(), cur.path(), new.path(), out.path()).unwrap();

        let dest = out.path().join("password-auth");
        let meta = fs::symlink_metadata(&dest).unwrap();
        assert!(
            meta.is_file(),
            "result must be a regular file, not a symlink"
        );
        let content = fs::read(&dest).unwrap();
        assert!(
            content.starts_with(b"auth"),
            "content must come from new image: {:?}",
            content
        );
    }

    #[test]
    fn merge_file_to_symlink_type_change_takes_new() {
        // Mirror of the above: source had a regular file, target ships it
        // as a symlink. User unchanged → take new (symlink).
        let old = tempdir().unwrap();
        let cur = tempdir().unwrap();
        let new = tempdir().unwrap();
        let out = tempdir().unwrap();
        fs::write(old.path().join("nsswitch.conf"), b"factory\n").unwrap();
        fs::write(cur.path().join("nsswitch.conf"), b"factory\n").unwrap();
        std::os::unix::fs::symlink("/usr/etc/nsswitch.conf", new.path().join("nsswitch.conf"))
            .unwrap();

        merge_etc_files(old.path(), cur.path(), new.path(), out.path()).unwrap();

        let dest = out.path().join("nsswitch.conf");
        let meta = fs::symlink_metadata(&dest).unwrap();
        assert!(meta.file_type().is_symlink(), "result must be a symlink");
        let target = fs::read_link(&dest).unwrap();
        assert_eq!(target.to_string_lossy(), "/usr/etc/nsswitch.conf");
    }

    #[test]
    fn merge_user_changed_file_to_symlink_keeps_user_symlink() {
        // User replaced a regular file with a symlink. Target still has the
        // factory file. The user's modification wins (keep cur symlink).
        let old = tempdir().unwrap();
        let cur = tempdir().unwrap();
        let new = tempdir().unwrap();
        let out = tempdir().unwrap();
        fs::write(old.path().join("resolv.conf"), b"factory\n").unwrap();
        std::os::unix::fs::symlink(
            "/run/NetworkManager/resolv.conf",
            cur.path().join("resolv.conf"),
        )
        .unwrap();
        fs::write(new.path().join("resolv.conf"), b"factory\n").unwrap();

        merge_etc_files(old.path(), cur.path(), new.path(), out.path()).unwrap();

        let dest = out.path().join("resolv.conf");
        let meta = fs::symlink_metadata(&dest).unwrap();
        assert!(
            meta.file_type().is_symlink(),
            "user's symlink modification must survive"
        );
        assert_eq!(
            fs::read_link(&dest).unwrap().to_string_lossy(),
            "/run/NetworkManager/resolv.conf"
        );
    }

    #[test]
    fn merge_machine_id_takes_current_verbatim() {
        let old = tempdir().unwrap();
        let cur = tempdir().unwrap();
        let new = tempdir().unwrap();
        let out = tempdir().unwrap();
        fs::write(old.path().join("machine-id"), "OLD\n").unwrap();
        fs::write(cur.path().join("machine-id"), "CUR\n").unwrap();
        fs::write(new.path().join("machine-id"), "NEW\n").unwrap();
        merge_etc_files(old.path(), cur.path(), new.path(), out.path()).unwrap();
        assert_eq!(
            fs::read_to_string(out.path().join("machine-id")).unwrap(),
            "CUR\n"
        );
    }

    #[test]
    fn prune_drops_dangling_usr_symlink() {
        let etc = tempdir().unwrap();
        let target = tempdir().unwrap();

        // /etc/systemd/system/dbus.service -> /usr/lib/systemd/system/dbus-broker.service
        // (target image has /usr/lib/systemd/system/dbus.service but NOT dbus-broker)
        fs::create_dir_all(etc.path().join("systemd/system")).unwrap();
        unix_fs::symlink(
            "/usr/lib/systemd/system/dbus-broker.service",
            etc.path().join("systemd/system/dbus.service"),
        )
        .unwrap();
        // Sibling symlink whose target DOES exist — must be preserved.
        unix_fs::symlink(
            "/usr/lib/systemd/system/getty@.service",
            etc.path().join("systemd/system/autovt@.service"),
        )
        .unwrap();

        fs::create_dir_all(target.path().join("usr/lib/systemd/system")).unwrap();
        fs::write(
            target.path().join("usr/lib/systemd/system/getty@.service"),
            "",
        )
        .unwrap();

        let removed = prune_dangling_usr_symlinks(etc.path(), target.path()).unwrap();
        assert_eq!(removed, 1);
        assert!(
            !etc.path().join("systemd/system/dbus.service").exists()
                && fs::symlink_metadata(etc.path().join("systemd/system/dbus.service")).is_err()
        );
        assert!(fs::symlink_metadata(etc.path().join("systemd/system/autovt@.service")).is_ok());
    }

    #[test]
    fn prune_drops_dangling_etc_symlink() {
        // Regression: Bluefin's PAM files are symlinks into
        // /etc/authselect/, which Dakota doesn't ship. Without pruning the
        // dangling link survives the merge and sshd's PAM stack fails to
        // load.
        let etc = tempdir().unwrap();
        let target = tempdir().unwrap();
        fs::create_dir_all(etc.path().join("pam.d")).unwrap();
        unix_fs::symlink(
            "/etc/authselect/password-auth",
            etc.path().join("pam.d/password-auth"),
        )
        .unwrap();

        let removed = prune_dangling_symlinks(etc.path(), target.path()).unwrap();
        assert_eq!(removed, 1);
        assert!(fs::symlink_metadata(etc.path().join("pam.d/password-auth")).is_err());
    }

    #[test]
    fn prune_keeps_etc_symlink_when_target_exists_in_merged_etc() {
        // /etc/foo → /etc/bar where /etc/bar exists in the merged etc — keep.
        let etc = tempdir().unwrap();
        let target = tempdir().unwrap();
        fs::write(etc.path().join("bar"), b"x").unwrap();
        unix_fs::symlink("/etc/bar", etc.path().join("foo")).unwrap();
        let removed = prune_dangling_symlinks(etc.path(), target.path()).unwrap();
        assert_eq!(removed, 0);
        assert!(fs::symlink_metadata(etc.path().join("foo")).is_ok());
    }

    #[test]
    fn prune_ignores_non_usr_symlinks() {
        let etc = tempdir().unwrap();
        let target = tempdir().unwrap();
        // A symlink pointing outside /usr — leave alone even if dangling.
        unix_fs::symlink("/var/run/foo", etc.path().join("foo")).unwrap();
        let removed = prune_dangling_usr_symlinks(etc.path(), target.path()).unwrap();
        assert_eq!(removed, 0);
        assert!(fs::symlink_metadata(etc.path().join("foo")).is_ok());
    }

    // TDD tests for 3-way /etc merge.

    #[test]
    fn merge_unchanged_file_takes_new_default() {
        let ctx = MergeContext {
            old_default: Some(b"old-content".to_vec()),
            current: Some(b"old-content".to_vec()),
            new_default: Some(b"new-content".to_vec()),
        };
        let result = choose_merged_content(&ctx);
        assert_eq!(result, Some(b"new-content".to_vec()));
    }

    #[test]
    fn merge_user_modified_file_keeps_current() {
        let ctx = MergeContext {
            old_default: Some(b"old".to_vec()),
            current: Some(b"customized".to_vec()),
            new_default: Some(b"new".to_vec()),
        };
        let result = choose_merged_content(&ctx);
        assert_eq!(result, Some(b"customized".to_vec()));
    }

    #[test]
    fn merge_upstream_unchanged_user_changed_keeps_current() {
        let ctx = MergeContext {
            old_default: Some(b"same".to_vec()),
            current: Some(b"user-changed".to_vec()),
            new_default: Some(b"same".to_vec()),
        };
        let result = choose_merged_content(&ctx);
        assert_eq!(result, Some(b"user-changed".to_vec()));
    }

    #[test]
    fn merge_drops_source_system_file_when_target_lacks_it() {
        // ComposeFS 3-way merge: old==cur (user didn't touch it), new==None
        // (target doesn't ship it). Drop it — this is a source-specific system
        // file, not a user customization. E.g. sshd_config.d/40-redhat-*.
        let ctx = MergeContext {
            old_default: Some(b"source-system-file".to_vec()),
            current: Some(b"source-system-file".to_vec()),
            new_default: None,
        };
        let result = choose_merged_content(&ctx);
        assert_eq!(result, None);
    }

    #[test]
    fn merge_keeps_file_removed_upstream_when_modified() {
        let ctx = MergeContext {
            old_default: Some(b"removed".to_vec()),
            current: Some(b"removed-but-modified".to_vec()),
            new_default: None,
        };
        let result = choose_merged_content(&ctx);
        assert_eq!(result, Some(b"removed-but-modified".to_vec()));
    }

    #[test]
    fn merge_takes_new_file_from_upstream() {
        let ctx = MergeContext {
            old_default: None,
            current: None,
            new_default: Some(b"new-upstream-file".to_vec()),
        };
        let result = choose_merged_content(&ctx);
        assert_eq!(result, Some(b"new-upstream-file".to_vec()));
    }

    #[test]
    fn merge_keeps_user_added_file() {
        let ctx = MergeContext {
            old_default: None,
            current: Some(b"user-added".to_vec()),
            new_default: None,
        };
        let result = choose_merged_content(&ctx);
        assert_eq!(result, Some(b"user-added".to_vec()));
    }

    #[test]
    fn merge_honors_user_deletion_when_upstream_added() {
        let ctx = MergeContext {
            old_default: Some(b"old-file".to_vec()),
            current: None,
            new_default: Some(b"new-version".to_vec()),
        };
        let result = choose_merged_content(&ctx);
        assert_eq!(result, None);
    }

    #[test]
    fn merge_all_three_different_keeps_current() {
        let ctx = MergeContext {
            old_default: Some(b"v1".to_vec()),
            current: Some(b"v2-user".to_vec()),
            new_default: Some(b"v3-upstream".to_vec()),
        };
        let result = choose_merged_content(&ctx);
        assert_eq!(result, Some(b"v2-user".to_vec()));
    }

    #[test]
    fn merge_user_and_upstream_made_same_change() {
        let ctx = MergeContext {
            old_default: Some(b"orig".to_vec()),
            current: Some(b"both".to_vec()),
            new_default: Some(b"both".to_vec()),
        };
        let result = choose_merged_content(&ctx);
        assert_eq!(result, Some(b"both".to_vec()));
    }

    #[test]
    fn merge_preserves_symlinks() {
        // Symlinks in /etc must be preserved.
        let dir = tempdir().unwrap();
        let old = dir.path().join("old");
        let cur = dir.path().join("cur");
        let new = dir.path().join("new");
        let out = dir.path().join("out");

        fs::create_dir_all(&old).unwrap();
        fs::create_dir_all(&cur).unwrap();
        fs::create_dir_all(&new).unwrap();

        // Setup a symlink that exists unchanged in all three
        fs::write(old.join("target.txt"), b"target").unwrap();
        fs::write(cur.join("target.txt"), b"target").unwrap();
        fs::write(new.join("target.txt"), b"updated-target").unwrap();
        std::os::unix::fs::symlink("target.txt", old.join("link.txt")).unwrap();
        std::os::unix::fs::symlink("target.txt", cur.join("link.txt")).unwrap();
        std::os::unix::fs::symlink("target.txt", new.join("link.txt")).unwrap();

        merge_etc_files(&old, &cur, &new, &out).unwrap();

        // The symlink should be preserved
        let link_meta = fs::symlink_metadata(out.join("link.txt")).unwrap();
        assert!(
            link_meta.file_type().is_symlink(),
            "symlink should be preserved"
        );
        assert_eq!(
            fs::read_link(out.join("link.txt"))
                .unwrap()
                .to_string_lossy(),
            "target.txt",
            "unchanged symlink target should carry forward"
        );
    }

    #[test]
    fn merge_preserves_user_changed_symlink() {
        let dir = tempdir().unwrap();
        let old = dir.path().join("old");
        let cur = dir.path().join("cur");
        let new = dir.path().join("new");
        let out = dir.path().join("out");

        fs::create_dir_all(&old).unwrap();
        fs::create_dir_all(&cur).unwrap();
        fs::create_dir_all(&new).unwrap();

        // User changed the symlink target locally
        std::os::unix::fs::symlink("old-target", old.join("my.link")).unwrap();
        std::os::unix::fs::symlink("my-custom-target", cur.join("my.link")).unwrap();
        std::os::unix::fs::symlink("new-upstream-target", new.join("my.link")).unwrap();

        merge_etc_files(&old, &cur, &new, &out).unwrap();

        let link_target = fs::read_link(out.join("my.link")).unwrap();
        assert_eq!(
            link_target.to_string_lossy(),
            "my-custom-target",
            "user-changed symlink target should be kept"
        );
    }

    #[test]
    fn collect_relative_entries_includes_symlinks() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("etc");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("real.txt"), b"data").unwrap();
        std::os::unix::fs::symlink("real.txt", root.join("link.txt")).unwrap();

        let entries = collect_relative_entries(&root).unwrap();
        assert!(entries.contains_key("real.txt"), "should have real.txt");
        assert!(entries.contains_key("link.txt"), "should have link.txt");
        match entries.get("link.txt") {
            Some(DirEntry::Symlink(target)) => assert_eq!(target, "real.txt"),
            other => panic!("expected Symlink, got {other:?}"),
        }
    }

    #[test]
    fn merge_preserves_user_added_symlink() {
        let dir = tempdir().unwrap();
        let cur = dir.path().join("cur");
        let new = dir.path().join("new");
        let out = dir.path().join("out");

        fs::create_dir_all(&cur).unwrap();
        fs::create_dir_all(&new).unwrap();
        std::os::unix::fs::symlink("target-file", cur.join("custom.link")).unwrap();
        fs::write(cur.join("target-file"), b"data").unwrap();

        let old_empty = tempdir().unwrap();
        let result = merge_etc_files(old_empty.path(), &cur, &new, &out);
        assert!(result.is_ok(), "merge should succeed: {:?}", result.err());

        let link_target = fs::read_link(out.join("custom.link")).unwrap();
        assert_eq!(
            link_target.to_string_lossy(),
            "target-file",
            "user-added symlink should be preserved"
        );
    }

    #[test]
    fn merge_etc_files_integration() {
        let dir = tempdir().unwrap();
        let old = dir.path().join("old");
        let cur = dir.path().join("cur");
        let new = dir.path().join("new");
        let out = dir.path().join("out");

        // Setup: file1 unchanged, file2 user-modified
        fs::create_dir_all(old.join("sub")).unwrap();
        fs::create_dir_all(cur.join("sub")).unwrap();
        fs::create_dir_all(new.join("sub")).unwrap();

        fs::write(old.join("unchanged.cfg"), b"orig").unwrap();
        fs::write(cur.join("unchanged.cfg"), b"orig").unwrap();
        fs::write(new.join("unchanged.cfg"), b"new-upstream").unwrap();

        fs::write(old.join("modified.cfg"), b"orig").unwrap();
        fs::write(cur.join("modified.cfg"), b"my-changes").unwrap();
        fs::write(new.join("modified.cfg"), b"new-upstream").unwrap();

        // sub/file3 is new upstream
        fs::write(new.join("sub/new-upstream.cfg"), b"brand-new").unwrap();

        // sub/file4 exists in source factory + live but not in target image.
        // old==cur (user didn't touch it) → composefs merge drops it.
        // This correctly removes source-specific system files like
        // sshd_config.d/40-redhat-crypto-policies.conf.
        fs::write(old.join("sub/removed.cfg"), b"stale").unwrap();
        fs::write(cur.join("sub/removed.cfg"), b"stale").unwrap();

        merge_etc_files(&old, &cur, &new, &out).unwrap();

        assert_eq!(
            fs::read_to_string(out.join("unchanged.cfg")).unwrap(),
            "new-upstream"
        );
        assert_eq!(
            fs::read_to_string(out.join("modified.cfg")).unwrap(),
            "my-changes"
        );
        assert_eq!(
            fs::read_to_string(out.join("sub/new-upstream.cfg")).unwrap(),
            "brand-new"
        );
        assert!(
            !out.join("sub/removed.cfg").exists(),
            "source-only system file with no user changes should be dropped"
        );
    }

    #[test]
    fn merge_keeps_user_created_unit_when_target_lacks_it() {
        // User-created file (only in cur, not in old or new). Must survive the
        // merge so that the sockets.target.wants symlink doesn't dangle post-pivot.
        // This matches the e2e-sshd.socket use case: injected into the live /etc
        // after OSTree install, not part of the OSTree factory default.
        let dir = tempdir().unwrap();
        let old = dir.path().join("old");
        let cur = dir.path().join("cur");
        let new = dir.path().join("new");
        let out = dir.path().join("out");
        for d in [&old, &cur, &new] {
            fs::create_dir_all(d.join("systemd/system")).unwrap();
        }
        let unit = "[Socket]\nListenStream=22\n";
        // Only in cur — user injected it into the live system.
        fs::write(cur.join("systemd/system/e2e-sshd.socket"), unit).unwrap();
        merge_etc_files(&old, &cur, &new, &out).unwrap();
        assert_eq!(
            fs::read_to_string(out.join("systemd/system/e2e-sshd.socket")).unwrap(),
            unit,
        );
    }

    // Config Drift Review (issue #15): diff_etc_factory_vs_live.

    #[test]
    fn drift_detects_added_file() {
        let factory = tempdir().unwrap();
        let live = tempdir().unwrap();
        fs::write(live.path().join("new.conf"), b"user-added").unwrap();
        let drift = diff_etc_factory_vs_live(factory.path(), live.path()).unwrap();
        assert_eq!(
            drift,
            vec![EtcDriftEntry {
                path: "new.conf".to_string(),
                kind: DriftKind::Added,
            }]
        );
    }

    #[test]
    fn drift_detects_removed_file() {
        let factory = tempdir().unwrap();
        let live = tempdir().unwrap();
        fs::write(factory.path().join("gone.conf"), b"factory-default").unwrap();
        let drift = diff_etc_factory_vs_live(factory.path(), live.path()).unwrap();
        assert_eq!(
            drift,
            vec![EtcDriftEntry {
                path: "gone.conf".to_string(),
                kind: DriftKind::Removed,
            }]
        );
    }

    #[test]
    fn drift_detects_modified_file() {
        let factory = tempdir().unwrap();
        let live = tempdir().unwrap();
        fs::write(factory.path().join("sshd_config"), b"factory").unwrap();
        fs::write(live.path().join("sshd_config"), b"user-edited").unwrap();
        let drift = diff_etc_factory_vs_live(factory.path(), live.path()).unwrap();
        assert_eq!(
            drift,
            vec![EtcDriftEntry {
                path: "sshd_config".to_string(),
                kind: DriftKind::Modified,
            }]
        );
    }

    #[test]
    fn drift_detects_modified_symlink_target() {
        let factory = tempdir().unwrap();
        let live = tempdir().unwrap();
        unix_fs::symlink(
            "/usr/lib/systemd/system/dbus-broker.service",
            factory.path().join("dbus.service"),
        )
        .unwrap();
        unix_fs::symlink(
            "/usr/lib/systemd/system/dbus.service",
            live.path().join("dbus.service"),
        )
        .unwrap();
        let drift = diff_etc_factory_vs_live(factory.path(), live.path()).unwrap();
        assert_eq!(
            drift,
            vec![EtcDriftEntry {
                path: "dbus.service".to_string(),
                kind: DriftKind::Modified,
            }]
        );
    }

    #[test]
    fn drift_detects_type_change() {
        let factory = tempdir().unwrap();
        let live = tempdir().unwrap();
        fs::write(factory.path().join("resolv.conf"), b"factory").unwrap();
        unix_fs::symlink(
            "/run/NetworkManager/resolv.conf",
            live.path().join("resolv.conf"),
        )
        .unwrap();
        let drift = diff_etc_factory_vs_live(factory.path(), live.path()).unwrap();
        assert_eq!(
            drift,
            vec![EtcDriftEntry {
                path: "resolv.conf".to_string(),
                kind: DriftKind::TypeChanged,
            }]
        );
    }

    #[test]
    fn drift_unchanged_file_produces_no_entry() {
        let factory = tempdir().unwrap();
        let live = tempdir().unwrap();
        fs::write(factory.path().join("same.conf"), b"identical").unwrap();
        fs::write(live.path().join("same.conf"), b"identical").unwrap();
        let drift = diff_etc_factory_vs_live(factory.path(), live.path()).unwrap();
        assert!(drift.is_empty());
    }

    #[test]
    fn drift_empty_both_sides() {
        let factory = tempdir().unwrap();
        let live = tempdir().unwrap();
        let drift = diff_etc_factory_vs_live(factory.path(), live.path()).unwrap();
        assert!(drift.is_empty());
    }

    #[test]
    fn drift_results_are_sorted_by_path() {
        let factory = tempdir().unwrap();
        let live = tempdir().unwrap();
        fs::write(live.path().join("zzz.conf"), b"a").unwrap();
        fs::write(live.path().join("aaa.conf"), b"b").unwrap();
        let drift = diff_etc_factory_vs_live(factory.path(), live.path()).unwrap();
        let paths: Vec<&str> = drift.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["aaa.conf", "zzz.conf"]);
    }

    // Config Drift Review manifest (issue #15): EtcDriftManifest.

    #[test]
    fn drift_manifest_json_round_trip() {
        let cases: Vec<(&str, EtcDriftManifest)> = vec![
            ("empty", EtcDriftManifest::default()),
            (
                "mixed",
                EtcDriftManifest {
                    decisions: HashMap::from([
                        ("sshd_config".to_string(), false),
                        ("motd".to_string(), true),
                    ]),
                },
            ),
        ];
        for (name, manifest) in cases {
            let json = manifest.to_json().unwrap();
            let parsed = EtcDriftManifest::parse(&json).unwrap();
            assert_eq!(parsed, manifest, "case {name}: round trip mismatch");
        }
    }

    #[test]
    fn drift_manifest_parse_rejects_malformed_json() {
        assert!(EtcDriftManifest::parse("not json").is_err());
    }

    #[test]
    fn drift_manifest_default_for_drift_keeps_everything_checked() {
        let drift = vec![
            EtcDriftEntry {
                path: "a.conf".to_string(),
                kind: DriftKind::Added,
            },
            EtcDriftEntry {
                path: "b.conf".to_string(),
                kind: DriftKind::Modified,
            },
            EtcDriftEntry {
                path: "c.conf".to_string(),
                kind: DriftKind::Removed,
            },
        ];
        let manifest = EtcDriftManifest::default_for_drift(&drift);
        for entry in &drift {
            assert_eq!(
                manifest.decisions.get(&entry.path),
                Some(&true),
                "{} should default to keep-current",
                entry.path
            );
        }
    }

    // Config Drift Review override wiring into the 3-way merge (issue #15).

    struct OverrideCase {
        name: &'static str,
        rel_path: &'static str,
        old: Option<&'static [u8]>,
        cur: Option<&'static [u8]>,
        new: Option<&'static [u8]>,
        decision: Option<bool>,
        expect_content: Option<&'static [u8]>,
        expect_sidecar: bool,
    }

    #[test]
    fn merge_honors_drift_review_overrides() {
        let cases = vec![
            OverrideCase {
                name: "override_take_default_replaces_user_change",
                rel_path: "sshd_config",
                old: Some(b"orig"),
                cur: Some(b"user-edit"),
                new: Some(b"target-default"),
                decision: Some(false),
                expect_content: Some(b"target-default"),
                expect_sidecar: true,
            },
            OverrideCase {
                name: "no_override_keeps_default_merge_behavior",
                rel_path: "other.conf",
                old: Some(b"orig"),
                cur: Some(b"user-edit"),
                new: Some(b"target-default"),
                decision: None,
                expect_content: Some(b"user-edit"),
                expect_sidecar: false,
            },
            OverrideCase {
                name: "override_keep_current_is_noop_when_user_modified",
                rel_path: "keep.conf",
                old: Some(b"orig"),
                cur: Some(b"user-edit"),
                new: Some(b"target-default"),
                decision: Some(true),
                expect_content: Some(b"user-edit"),
                expect_sidecar: false,
            },
            OverrideCase {
                name: "override_take_default_when_unchanged_writes_no_sidecar",
                rel_path: "unchanged.conf",
                old: Some(b"same"),
                cur: Some(b"same"),
                new: Some(b"target-default"),
                decision: Some(false),
                expect_content: Some(b"target-default"),
                expect_sidecar: false,
            },
            OverrideCase {
                name: "override_take_default_on_user_added_file_drops_it",
                rel_path: "user-added.conf",
                old: None,
                cur: Some(b"unit-file"),
                new: None,
                decision: Some(false),
                expect_content: None,
                expect_sidecar: true,
            },
            OverrideCase {
                name: "override_take_default_on_identity_db_is_ignored",
                rel_path: "passwd",
                old: Some(b"root:x:0:0::/root:/bin/bash\n"),
                cur: Some(
                    b"root:x:0:0::/root:/bin/bash\nuser1:x:1000:1000::/home/user1:/bin/bash\n",
                ),
                new: Some(b"root:x:0:0::/root:/bin/bash\n"),
                decision: Some(false),
                expect_content: Some(
                    b"root:x:0:0::/root:/bin/bash\nuser1:x:1000:1000::/home/user1:/bin/bash\n",
                ),
                expect_sidecar: false,
            },
        ];

        for case in cases {
            let old_dir = tempdir().unwrap();
            let cur_dir = tempdir().unwrap();
            let new_dir = tempdir().unwrap();
            let out_dir = tempdir().unwrap();

            if let Some(content) = case.old {
                fs::write(old_dir.path().join(case.rel_path), content).unwrap();
            }
            if let Some(content) = case.cur {
                fs::write(cur_dir.path().join(case.rel_path), content).unwrap();
            }
            if let Some(content) = case.new {
                fs::write(new_dir.path().join(case.rel_path), content).unwrap();
            }

            let manifest = case.decision.map(|decision| EtcDriftManifest {
                decisions: HashMap::from([(case.rel_path.to_string(), decision)]),
            });

            merge_etc_files_with_overrides(
                old_dir.path(),
                cur_dir.path(),
                new_dir.path(),
                out_dir.path(),
                manifest.as_ref(),
            )
            .unwrap_or_else(|e| panic!("case {}: merge failed: {e:#}", case.name));

            let out_path = out_dir.path().join(case.rel_path);
            match case.expect_content {
                Some(expected) => assert_eq!(
                    fs::read(&out_path).unwrap(),
                    expected,
                    "case {}: merged content mismatch",
                    case.name
                ),
                None => assert!(
                    !out_path.exists(),
                    "case {}: expected no output file, found one",
                    case.name
                ),
            }

            let sidecar = out_dir.path().join(format!("{}.rebase-old", case.rel_path));
            assert_eq!(
                sidecar.exists(),
                case.expect_sidecar,
                "case {}: .rebase-old sidecar presence mismatch",
                case.name
            );
            if case.expect_sidecar {
                assert_eq!(
                    fs::read(&sidecar).unwrap(),
                    case.cur.unwrap(),
                    "case {}: .rebase-old sidecar content mismatch",
                    case.name
                );
            }
        }
    }
}
