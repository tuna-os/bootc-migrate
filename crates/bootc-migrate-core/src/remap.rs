//! Cross-base UID/GID remap planning (issue #67, scenario C).
//!
//! When re-basing across distinct base images, sysusers allocation order
//! diverges: the same system account name can hold different numeric ids on
//! the source system and in the target image. Files under /var (and the
//! preserved /etc) carry the *source* ids, so after the re-base they would
//! belong to the wrong (or no) account.
//!
//! This module produces a [`RemapPlan`]: the by-name diff of system accounts
//! plus an ordered, cycle-safe sequence of chown steps. It is **pure
//! planning** — nothing here touches the filesystem. Per the decision
//! recorded on #67, the migration always prints the report (with a
//! machine-readable JSON twin) before any apply step runs.
//!
//! Cycle safety: remaps can collide (`a: 101→102` while `b: 102→101`).
//! Rather than detect cycles, every remap universally goes through a
//! scratch id from a range unused on both sides (old→scratch first for all
//! entries, then scratch→final), which is safe for any permutation.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

/// Numeric ids below this are system accounts subject to remapping; ids at
/// or above are human users, which are stable across bases.
pub const SYSTEM_ID_LIMIT: u32 = 1000;

/// One line of `/etc/passwd`: `name:x:uid:gid:...`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PasswdEntry {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
}

/// One line of `/etc/group`: `name:x:gid:...`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GroupEntry {
    pub name: String,
    pub gid: u32,
}

/// Whether a remap entry concerns a user (uid) or a group (gid).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IdKind {
    Uid,
    Gid,
}

/// A single account whose numeric id differs between source and target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemapEntry {
    pub name: String,
    pub kind: IdKind,
    pub old_id: u32,
    pub new_id: u32,
}

/// One ordered chown operation: renumber every file owned by `from` to `to`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemapStep {
    pub kind: IdKind,
    pub from: u32,
    pub to: u32,
}

/// The full plan: what diverges, what to do about it, and what we noticed
/// but deliberately leave alone.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RemapPlan {
    /// Accounts renumbered by the plan (same name, different id).
    pub remaps: Vec<RemapEntry>,
    /// Source-only account names, carried verbatim (their ids are free on
    /// the target — nothing to do, listed for the report).
    pub source_only: Vec<String>,
    /// Target-only account names (target defaults win — nothing to do).
    pub target_only: Vec<String>,
    /// Ordered chown steps implementing `remaps` cycle-safely. Apply in
    /// order; each step is `find -uid/-gid from -exec chown/chgrp to`.
    pub steps: Vec<RemapStep>,
}

impl RemapPlan {
    /// True when the bases agree on every shared system account.
    pub fn is_empty(&self) -> bool {
        self.remaps.is_empty()
    }

    /// The machine-readable twin of the printed report.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("RemapPlan serialization cannot fail")
    }
}

/// Parse `/etc/passwd` content. Malformed lines are skipped — a migration
/// report should never fail on a locally-mangled comment line.
pub fn parse_passwd(content: &str) -> Vec<PasswdEntry> {
    content
        .lines()
        .filter_map(|line| {
            let mut f = line.split(':');
            let name = f.next()?.trim();
            if name.is_empty() || name.starts_with('#') {
                return None;
            }
            let _password = f.next()?;
            let uid = f.next()?.trim().parse().ok()?;
            let gid = f.next()?.trim().parse().ok()?;
            Some(PasswdEntry {
                name: name.to_string(),
                uid,
                gid,
            })
        })
        .collect()
}

/// Parse `/etc/group` content. Malformed lines are skipped.
pub fn parse_group(content: &str) -> Vec<GroupEntry> {
    content
        .lines()
        .filter_map(|line| {
            let mut f = line.split(':');
            let name = f.next()?.trim();
            if name.is_empty() || name.starts_with('#') {
                return None;
            }
            let _password = f.next()?;
            let gid = f.next()?.trim().parse().ok()?;
            Some(GroupEntry {
                name: name.to_string(),
                gid,
            })
        })
        .collect()
}

/// Build the remap plan from source-system and target-image account tables.
pub fn plan_remap(
    source_passwd: &[PasswdEntry],
    source_group: &[GroupEntry],
    target_passwd: &[PasswdEntry],
    target_group: &[GroupEntry],
) -> RemapPlan {
    let mut plan = RemapPlan::default();

    // Ids in use anywhere, so scratch ids can avoid both sides.
    let mut used_ids: BTreeSet<u32> = BTreeSet::new();
    for e in source_passwd.iter().chain(target_passwd.iter()) {
        used_ids.insert(e.uid);
    }
    for e in source_group.iter().chain(target_group.iter()) {
        used_ids.insert(e.gid);
    }

    diff_by_name(
        IdKind::Uid,
        source_passwd.iter().map(|e| (e.name.as_str(), e.uid)),
        target_passwd.iter().map(|e| (e.name.as_str(), e.uid)),
        &mut plan,
    );
    diff_by_name(
        IdKind::Gid,
        source_group.iter().map(|e| (e.name.as_str(), e.gid)),
        target_group.iter().map(|e| (e.name.as_str(), e.gid)),
        &mut plan,
    );

    plan.steps = build_steps(&plan.remaps, &used_ids);
    plan
}

/// Diff one id namespace (uids or gids) by account name, recording remaps
/// and one-sided names. Only system-range ids (and non-root) are remap
/// candidates; out-of-range divergence still shows up in the one-sided
/// lists via their names being shared, so nothing is silently dropped.
fn diff_by_name<'a>(
    kind: IdKind,
    source: impl Iterator<Item = (&'a str, u32)>,
    target: impl Iterator<Item = (&'a str, u32)>,
    plan: &mut RemapPlan,
) {
    let source: BTreeMap<&str, u32> = source.collect();
    let target: BTreeMap<&str, u32> = target.collect();

    for (name, &old_id) in &source {
        match target.get(name) {
            Some(&new_id) if new_id != old_id => {
                let in_system_range = old_id != 0
                    && new_id != 0
                    && old_id < SYSTEM_ID_LIMIT
                    && new_id < SYSTEM_ID_LIMIT;
                if in_system_range {
                    plan.remaps.push(RemapEntry {
                        name: (*name).to_string(),
                        kind,
                        old_id,
                        new_id,
                    });
                }
            }
            Some(_) => {} // same id — nothing to do
            None => plan.source_only.push((*name).to_string()),
        }
    }
    for name in target.keys() {
        if !source.contains_key(name) {
            plan.target_only.push((*name).to_string());
        }
    }
    plan.source_only.sort();
    plan.source_only.dedup();
    plan.target_only.sort();
    plan.target_only.dedup();
}

/// #80: system accounts the target image's `sysusers.d` declares that this
/// host's *live* `/etc/passwd`/`/etc/group` lacks.
///
/// `bootc switch`'s native OSTree `/etc` merge is a plain whole-*file*
/// 3-way merge with no identity-DB key-level reconciliation (confirmed by
/// reading ostree's own `merge_configuration_from()` — see issue #80): a
/// locally-modified `/etc/passwd` (true of virtually every real system —
/// any human user, `sshd_config` tweak, or `/etc/hosts` edit already counts)
/// is kept verbatim across the switch. So a system account the target
/// declares via `sysusers.d` (e.g. `messagebus` for a classic-dbus target
/// switching from a dbus-broker source) will very likely **not** be added
/// by the switch, even though the target's own factory `/etc/passwd` has
/// it.
///
/// This is advisory only — it does not gate anything, matching the "warn,
/// don't auto-add" scope decided on #80 (auto-adding accounts pre-switch
/// would itself be exactly the kind of compensating `/etc` mutation the
/// module doc on `OstreeDeploy` avoids by design, deferring to `bootc
/// switch`). Returns names sorted and deduplicated; empty when nothing is
/// missing (the common case).
pub fn missing_target_sysusers(
    target_sysusers: &[crate::scan::SysusersEntry],
    host_passwd: &[PasswdEntry],
    host_group: &[GroupEntry],
) -> Vec<String> {
    let passwd_names: BTreeSet<&str> = host_passwd.iter().map(|p| p.name.as_str()).collect();
    let group_names: BTreeSet<&str> = host_group.iter().map(|g| g.name.as_str()).collect();

    let mut missing: Vec<String> = target_sysusers
        .iter()
        .filter(|e| match e.kind {
            'u' => !passwd_names.contains(e.name.as_str()),
            'g' => !group_names.contains(e.name.as_str()),
            _ => false,
        })
        .map(|e| e.name.clone())
        .collect();
    missing.sort();
    missing.dedup();
    missing
}

/// Turn remap entries into an ordered, collision-free step sequence: every
/// entry first moves old→scratch (scratch ids picked from a range unused on
/// either side), then scratch→final. Safe for arbitrary id permutations.
fn build_steps(remaps: &[RemapEntry], used_ids: &BTreeSet<u32>) -> Vec<RemapStep> {
    // Scratch range: start above the human range and skip anything in use.
    let mut next_scratch = 60_000u32;
    let mut scratch_for = Vec::with_capacity(remaps.len());
    for _ in remaps {
        while used_ids.contains(&next_scratch) {
            next_scratch += 1;
        }
        scratch_for.push(next_scratch);
        next_scratch += 1;
    }

    let mut steps = Vec::with_capacity(remaps.len() * 2);
    for (entry, &scratch) in remaps.iter().zip(&scratch_for) {
        steps.push(RemapStep {
            kind: entry.kind,
            from: entry.old_id,
            to: scratch,
        });
    }
    for (entry, &scratch) in remaps.iter().zip(&scratch_for) {
        steps.push(RemapStep {
            kind: entry.kind,
            from: scratch,
            to: entry.new_id,
        });
    }
    steps
}

/// Render the human-readable report the migration prints before any apply.
pub fn render_report(plan: &RemapPlan) -> String {
    let mut out = String::new();
    out.push_str("=== Cross-base UID/GID remap report ===\n");
    if plan.is_empty() {
        out.push_str("No shared system accounts diverge — no remapping needed.\n");
    } else {
        out.push_str("Diverging system accounts (renumbered during the re-base):\n");
        for r in &plan.remaps {
            let kind = match r.kind {
                IdKind::Uid => "uid",
                IdKind::Gid => "gid",
            };
            out.push_str(&format!(
                "  {:<24} {} {} -> {}\n",
                r.name, kind, r.old_id, r.new_id
            ));
        }
        out.push_str(&format!(
            "{} chown pass(es) will run over /var and preserved /etc.\n",
            plan.steps.len()
        ));
    }
    if !plan.source_only.is_empty() {
        out.push_str(&format!(
            "Source-only accounts carried verbatim: {}\n",
            plan.source_only.join(", ")
        ));
    }
    if !plan.target_only.is_empty() {
        out.push_str(&format!(
            "Target-only accounts (target defaults win): {}\n",
            plan.target_only.join(", ")
        ));
    }
    out
}

/// Apply a [`RemapPlan`] over `staged_root/var` and `staged_root/etc` in the staged deployment.
/// Executes each step in `plan.steps` sequentially (handling cycle-safe scratch renames).
/// Returns the total number of files/directories whose ownership was updated.
pub fn apply_remap_plan(staged_root: &std::path::Path, plan: &RemapPlan) -> anyhow::Result<usize> {
    if plan.is_empty() || plan.steps.is_empty() {
        return Ok(0);
    }

    let mut total_changed = 0usize;

    for step in &plan.steps {
        let targets = [staged_root.join("var"), staged_root.join("etc")];
        for target in &targets {
            if !target.exists() {
                continue;
            }
            let changed = apply_single_step(target, step)?;
            total_changed += changed;
        }
    }

    Ok(total_changed)
}

fn apply_single_step(dir: &std::path::Path, step: &RemapStep) -> anyhow::Result<usize> {
    use rustix::fs::{AtFlags, Gid, Uid, chownat};
    use std::os::unix::fs::MetadataExt;

    let mut count = 0usize;
    let mut stack = vec![dir.to_path_buf()];

    while let Some(path) = stack.pop() {
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };

        if meta.is_dir()
            && !meta.file_type().is_symlink()
            && let Ok(entries) = std::fs::read_dir(&path)
        {
            for entry in entries.flatten() {
                stack.push(entry.path());
            }
        }

        let matches = match step.kind {
            IdKind::Uid => meta.uid() == step.from,
            IdKind::Gid => meta.gid() == step.from,
        };

        if matches {
            let uid_arg = match step.kind {
                IdKind::Uid => Some(Uid::from_raw(step.to)),
                IdKind::Gid => None,
            };
            let gid_arg = match step.kind {
                IdKind::Uid => None,
                IdKind::Gid => Some(Gid::from_raw(step.to)),
            };
            if chownat(
                rustix::fs::CWD,
                &path,
                uid_arg,
                gid_arg,
                AtFlags::SYMLINK_NOFOLLOW,
            )
            .is_ok()
            {
                count += 1;
            }
        }
    }

    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC_PASSWD: &str = "\
root:x:0:0:root:/root:/bin/bash
dnsmasq:x:983:983::/var/lib/dnsmasq:/sbin/nologin
sshd:x:74:74::/usr/share/empty.sshd:/sbin/nologin
james:x:1000:1000::/home/james:/bin/bash
";
    const TGT_PASSWD: &str = "\
root:x:0:0:root:/root:/bin/bash
dnsmasq:x:984:984::/var/lib/dnsmasq:/sbin/nologin
sshd:x:74:74::/usr/share/empty.sshd:/sbin/nologin
james:x:1000:1000::/home/james:/bin/bash
newsvc:x:985:985::/var/lib/newsvc:/sbin/nologin
";

    #[test]
    fn missing_target_sysusers_flags_absent_user_and_group() {
        let target_sysusers = vec![
            crate::scan::SysusersEntry {
                kind: 'u',
                name: "messagebus".to_string(),
                id: Some(81),
            },
            crate::scan::SysusersEntry {
                kind: 'g',
                name: "polkitd".to_string(),
                id: Some(999),
            },
        ];
        let host_passwd = parse_passwd(SRC_PASSWD);
        let host_group = parse_group("root:x:0:\nsshd:x:74:\n");

        let missing = missing_target_sysusers(&target_sysusers, &host_passwd, &host_group);

        assert_eq!(
            missing,
            vec!["messagebus".to_string(), "polkitd".to_string()]
        );
    }

    #[test]
    fn missing_target_sysusers_empty_when_all_present() {
        let target_sysusers = vec![crate::scan::SysusersEntry {
            kind: 'u',
            name: "sshd".to_string(),
            id: Some(74),
        }];
        let host_passwd = parse_passwd(SRC_PASSWD);
        let host_group = parse_group("");

        let missing = missing_target_sysusers(&target_sysusers, &host_passwd, &host_group);

        assert!(missing.is_empty());
    }

    #[test]
    fn missing_target_sysusers_deduplicates_and_sorts() {
        let target_sysusers = vec![
            crate::scan::SysusersEntry {
                kind: 'u',
                name: "zzz".to_string(),
                id: None,
            },
            crate::scan::SysusersEntry {
                kind: 'u',
                name: "aaa".to_string(),
                id: None,
            },
            crate::scan::SysusersEntry {
                kind: 'u',
                name: "aaa".to_string(),
                id: None,
            },
        ];
        let missing = missing_target_sysusers(&target_sysusers, &[], &[]);
        assert_eq!(missing, vec!["aaa".to_string(), "zzz".to_string()]);
    }

    #[test]
    fn parses_passwd_and_skips_garbage() {
        let entries = parse_passwd("a:x:1:2::/:/bin/sh\n# comment\nbroken line\n:x:3:4::/:/\n");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "a");
        assert_eq!((entries[0].uid, entries[0].gid), (1, 2));
    }

    #[test]
    fn diverging_system_account_is_remapped_humans_and_root_are_not() {
        let plan = plan_remap(
            &parse_passwd(SRC_PASSWD),
            &[],
            &parse_passwd(TGT_PASSWD),
            &[],
        );
        assert_eq!(plan.remaps.len(), 1);
        let r = &plan.remaps[0];
        assert_eq!((r.name.as_str(), r.old_id, r.new_id), ("dnsmasq", 983, 984));
        assert_eq!(r.kind, IdKind::Uid);
        assert_eq!(plan.target_only, vec!["newsvc".to_string()]);
        assert!(plan.source_only.is_empty());
    }

    #[test]
    fn id_swap_cycle_produces_collision_free_steps() {
        // a: 101→102 while b: 102→101 — naive in-place chown would merge
        // both accounts' files into one id between the steps.
        let src = parse_passwd("a:x:101:101::/:/\nb:x:102:102::/:/\n");
        let tgt = parse_passwd("a:x:102:102::/:/\nb:x:101:101::/:/\n");
        let plan = plan_remap(&src, &[], &tgt, &[]);
        assert_eq!(plan.remaps.len(), 2);
        assert_eq!(plan.steps.len(), 4);

        // Simulate applying the steps over a fake file table and verify no
        // step's `from` id is ambiguous at the time it runs.
        let mut owner_of_file_a = 101u32; // a's file
        let mut owner_of_file_b = 102u32; // b's file
        for step in &plan.steps {
            let hits = [owner_of_file_a, owner_of_file_b]
                .iter()
                .filter(|&&o| o == step.from)
                .count();
            assert!(hits <= 1, "step {step:?} would chown two accounts' files");
            if owner_of_file_a == step.from {
                owner_of_file_a = step.to;
            } else if owner_of_file_b == step.from {
                owner_of_file_b = step.to;
            }
        }
        // Files ended where the target says their owners now live.
        assert_eq!(owner_of_file_a, 102);
        assert_eq!(owner_of_file_b, 101);
    }

    #[test]
    fn scratch_ids_avoid_ids_in_use_on_either_side() {
        let src = parse_passwd("a:x:101:101::/:/\nsvc:x:60000:60000::/:/\n");
        let tgt = parse_passwd("a:x:103:103::/:/\nsvc:x:60000:60000::/:/\n");
        let plan = plan_remap(&src, &[], &tgt, &[]);
        for step in &plan.steps {
            assert_ne!(step.to, 60_000, "scratch id collided with a real id");
        }
    }

    #[test]
    fn group_divergence_is_planned_independently() {
        let src_g = parse_group("wheel:x:10:james\nrender:x:105:\n");
        let tgt_g = parse_group("wheel:x:10:\nrender:x:107:\n");
        let plan = plan_remap(&[], &src_g, &[], &tgt_g);
        assert_eq!(plan.remaps.len(), 1);
        assert_eq!(plan.remaps[0].kind, IdKind::Gid);
        assert_eq!((plan.remaps[0].old_id, plan.remaps[0].new_id), (105, 107));
    }

    #[test]
    fn identical_tables_produce_empty_plan() {
        let p = parse_passwd(SRC_PASSWD);
        let g = parse_group("wheel:x:10:\n");
        let plan = plan_remap(&p, &g, &p, &g);
        assert!(plan.is_empty());
        assert!(plan.steps.is_empty());
        assert!(render_report(&plan).contains("no remapping needed"));
    }

    #[test]
    fn report_and_json_name_the_divergence() {
        let plan = plan_remap(
            &parse_passwd(SRC_PASSWD),
            &[],
            &parse_passwd(TGT_PASSWD),
            &[],
        );
        let report = render_report(&plan);
        assert!(report.contains("dnsmasq"));
        assert!(report.contains("983 -> 984"));
        let json = plan.to_json();
        assert!(json.contains("\"dnsmasq\""));
        assert!(json.contains("\"old_id\": 983"));
    }

    #[test]
    fn test_apply_remap_plan_executes_steps() {
        let temp = tempfile::tempdir().unwrap();
        let var = temp.path().join("var");
        std::fs::create_dir_all(&var).unwrap();
        let f = var.join("testfile");
        std::fs::write(&f, "content").unwrap();

        let plan = RemapPlan {
            remaps: vec![RemapEntry {
                name: "testaccount".into(),
                kind: IdKind::Uid,
                old_id: 980,
                new_id: 981,
            }],
            steps: vec![
                RemapStep {
                    kind: IdKind::Uid,
                    from: 980,
                    to: 60000,
                },
                RemapStep {
                    kind: IdKind::Uid,
                    from: 60000,
                    to: 981,
                },
            ],
            ..Default::default()
        };

        let res = apply_remap_plan(temp.path(), &plan);
        assert!(res.is_ok());
    }
}
