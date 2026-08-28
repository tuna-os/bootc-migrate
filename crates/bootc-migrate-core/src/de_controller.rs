//! Orchestration for deciding whether a re-base needs desktop migration.
//!
//! Detection and passwd parsing live in their focused modules; this module
//! owns the policy that combines their results into one plan for CLI callers.

use crate::de_detect::{self, DesktopDetection};
use crate::de_migrate::{self, DesktopEnvironment, UserHome, print_hook_results};
use anyhow::{Context, Result};
use std::path::Path;

/// Where a user's stash lives, relative to their home. Same value as the
/// `de-migrate stash|restore --stash-dir` default, so a stash written by the
/// standalone subcommand is found by the `rebase` flow and vice versa.
const DE_STASH_SUBDIR: &str = ".local/share/de-migrate";

/// A cross-desktop migration that should run for the listed users.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopMigrationPlan {
    pub from: DesktopEnvironment,
    pub to: DesktopEnvironment,
    pub users: Vec<UserHome>,
}

/// Why planning did or did not produce work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DesktopMigrationDecision {
    Disabled,
    NotCrossDesktop {
        host: DesktopDetection,
        target: DesktopDetection,
    },
    NoUsers {
        from: DesktopEnvironment,
        to: DesktopEnvironment,
    },
    Planned(DesktopMigrationPlan),
}

/// Inputs and I/O boundary for desktop-migration planning.
#[derive(Debug)]
pub struct DesktopMigrationController<'a> {
    enabled: bool,
    target_image: &'a str,
}

impl<'a> DesktopMigrationController<'a> {
    pub fn new(enabled: bool, target_image: &'a str) -> Self {
        Self {
            enabled,
            target_image,
        }
    }

    /// Detect both desktops and enumerate human accounts, without mutating
    /// either the host or the target image.
    pub fn plan(&self) -> Result<DesktopMigrationDecision> {
        if !self.enabled {
            return Ok(DesktopMigrationDecision::Disabled);
        }

        let host = de_detect::detect_host_desktop().context("detecting this host's desktop")?;
        let target = de_detect::detect_image_desktop(self.target_image)
            .with_context(|| format!("detecting the desktop shipped by {}", self.target_image))?;
        let passwd = std::fs::read_to_string("/etc/passwd").context("reading /etc/passwd")?;

        Ok(decide(host, target, &passwd))
    }

    /// Plan, and report why there is nothing to do when there isn't.
    ///
    /// Every way this can come up short — the flag off, an ambiguous or
    /// unrecognized desktop on either side, an unreachable registry, no human
    /// accounts — degrades to "do nothing" rather than failing the re-base, but
    /// none of them degrade quietly. Errors are reported once, here, so the
    /// stash/restore functions themselves can propagate normally: a *failed*
    /// half-move of a user's config must abort, unlike a decision not to move
    /// anything at all.
    pub fn plan_or_report(&self) -> Option<DesktopMigrationPlan> {
        match self.try_plan_or_report() {
            Ok(plan) => plan,
            Err(e) => {
                eprintln!("Warning: DE migration skipped — {e:#}");
                None
            }
        }
    }

    fn try_plan_or_report(&self) -> Result<Option<DesktopMigrationPlan>> {
        match self.plan()? {
            DesktopMigrationDecision::Disabled => {
                println!(
                    "DE migration: skipped (--de-migrate not passed); per-user desktop config is \
                     left exactly as it is."
                );
                Ok(None)
            }
            DesktopMigrationDecision::NotCrossDesktop { host, target } => {
                println!(
                    "DE migration: nothing to do (this host: {host}, {}: {target}).",
                    self.target_image
                );
                Ok(None)
            }
            DesktopMigrationDecision::NoUsers { from, to } => {
                println!(
                    "DE migration: {from} -> {to}, but /etc/passwd has no human accounts to stash."
                );
                Ok(None)
            }
            DesktopMigrationDecision::Planned(plan) => Ok(Some(plan)),
        }
    }

    /// Stash the outgoing DE's config for every planned user and run the
    /// `pre-switch.d` hooks, before the target is staged. Hook ordering matches
    /// `de-migrate stash --run-hooks`: the stash is in place by the time a hook
    /// runs, so a hook can read what was moved out of the way.
    ///
    /// A failure here aborts the re-base before anything is staged, which is the
    /// point of running it first: a half-moved home is worse than no re-base.
    pub fn run_pre_switch(&self, plan: &DesktopMigrationPlan, dry_run: bool) -> Result<()> {
        println!(
            "DE migration: {} -> {} for {} user(s){}.",
            plan.from,
            plan.to,
            plan.users.len(),
            if dry_run { " [DRY RUN]" } else { "" }
        );
        let hooks = de_migrate::discover_hooks(Path::new(de_migrate::PRE_SWITCH_HOOK_DIR))
            .context("discovering pre-switch hooks")?;

        for user in &plan.users {
            let stash_dir = user.home.join(DE_STASH_SUBDIR);
            let moved = de_migrate::stash(plan.from, &user.home, &stash_dir, dry_run)
                .with_context(|| format!("stashing {} config for {}", plan.from, user.name))?;
            if moved.is_empty() {
                println!("  {}: no {} config to stash.", user.name, plan.from);
            } else {
                println!(
                    "  {}: stashed {} path(s) into {}",
                    user.name,
                    moved.len(),
                    stash_dir.display()
                );
                for p in &moved {
                    println!("    {p}");
                }
            }
            // No hooks installed is the common case; don't print an empty
            // "no hooks" block once per user for it.
            if !hooks.is_empty() {
                let env = de_migrate::build_hook_env(plan.from, plan.to, &stash_dir, &user.home);
                let results = de_migrate::run_hooks(&hooks, &env, dry_run)
                    .with_context(|| format!("running pre-switch hooks for {}", user.name))?;
                print_hook_results(&results);
            }
        }
        Ok(())
    }

    /// Re-expose the incoming DE's stash — the one a previous re-base in the
    /// other direction left behind — and run the `post-switch.d` hooks, after
    /// the target is staged. A first-time switch to a DE simply has no stash to
    /// restore, which is reported, not treated as an error.
    pub fn run_post_switch(&self, plan: &DesktopMigrationPlan, dry_run: bool) -> Result<()> {
        let hooks = de_migrate::discover_hooks(Path::new(de_migrate::POST_SWITCH_HOOK_DIR))
            .context("discovering post-switch hooks")?;

        println!(
            "DE migration: re-exposing any previous {} stash{}.",
            plan.to,
            if dry_run { " [DRY RUN]" } else { "" }
        );
        for user in &plan.users {
            let stash_dir = user.home.join(DE_STASH_SUBDIR);
            let restored = de_migrate::restore(plan.to, &user.home, &stash_dir, dry_run)
                .with_context(|| format!("restoring {} config for {}", plan.to, user.name))?;
            if restored.is_empty() {
                println!(
                    "  {}: no previous {} stash to restore (first switch to it).",
                    user.name, plan.to
                );
            } else {
                println!(
                    "  {}: restored {} previously-stashed {} path(s).",
                    user.name,
                    restored.len(),
                    plan.to
                );
            }
            if !hooks.is_empty() {
                let env = de_migrate::build_hook_env(plan.from, plan.to, &stash_dir, &user.home);
                let results = de_migrate::run_hooks(&hooks, &env, dry_run)
                    .with_context(|| format!("running post-switch hooks for {}", user.name))?;
                print_hook_results(&results);
            }
        }
        Ok(())
    }

    /// Run both halves against a plan without staging anything, so `--dry-run`
    /// prints the whole DE step instead of only the part that precedes staging.
    pub fn preview(&self, plan: &DesktopMigrationPlan) -> Result<()> {
        self.run_pre_switch(plan, true)?;
        self.run_post_switch(plan, true)
    }
}

fn decide(
    host: DesktopDetection,
    target: DesktopDetection,
    passwd: &str,
) -> DesktopMigrationDecision {
    let Some((from, to)) = de_detect::cross_desktop_pair(&host, &target) else {
        return DesktopMigrationDecision::NotCrossDesktop { host, target };
    };

    let users = de_migrate::parse_user_homes(passwd);
    if users.is_empty() {
        DesktopMigrationDecision::NoUsers { from, to }
    } else {
        DesktopMigrationDecision::Planned(DesktopMigrationPlan { from, to, users })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planning_decisions_are_separate_from_io() {
        let cases = [
            (
                DesktopDetection::Single(DesktopEnvironment::Gnome),
                DesktopDetection::Single(DesktopEnvironment::Gnome),
                "alice:x:1000:1000::/home/alice:/bin/bash\n",
                "same",
            ),
            (
                DesktopDetection::Single(DesktopEnvironment::Gnome),
                DesktopDetection::Single(DesktopEnvironment::Kde),
                "root:x:0:0::/root:/bin/bash\n",
                "no-users",
            ),
            (
                DesktopDetection::Single(DesktopEnvironment::Gnome),
                DesktopDetection::Single(DesktopEnvironment::Kde),
                "alice:x:1000:1000::/home/alice:/bin/bash\n",
                "planned",
            ),
        ];

        for (host, target, passwd, expected) in cases {
            let actual = match decide(host, target, passwd) {
                DesktopMigrationDecision::NotCrossDesktop { .. } => "same",
                DesktopMigrationDecision::NoUsers { .. } => "no-users",
                DesktopMigrationDecision::Planned(_) => "planned",
                DesktopMigrationDecision::Disabled => "disabled",
            };
            assert_eq!(actual, expected);
        }
    }
}
