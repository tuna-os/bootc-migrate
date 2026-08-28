//! CLI adapter for the standalone `bootc-rebase de-migrate stash|restore`
//! subcommand: argument shaping, hook-environment construction, and result
//! presentation.
//!
//! Every filesystem operation and hook execution is delegated to
//! `bootc_migrate_core::de_migrate`; nothing here decides *what* to stash.
//!
//! `parse_de` and `print_hook_results` live here rather than in `main.rs`
//! because the automatic controller path needs them too, and #159 asks for
//! exactly one copy — a second implementation is how the manual and
//! automatic paths would drift apart on which desktops are accepted or how
//! a failed hook is reported.

use anyhow::{Context, Result};
use std::path::Path;

use crate::DeMigrateAction;
use crate::DeMigrateArgs;

pub fn run(args: &DeMigrateArgs) -> Result<()> {
    use bootc_migrate_core::de_migrate;

    match &args.action {
        DeMigrateAction::Stash {
            from_de,
            home,
            stash_dir,
            run_hooks,
            dry_run,
        } => {
            let de = parse_de(from_de)?;
            let moved =
                de_migrate::stash(de, home, stash_dir, *dry_run).context("stashing DE config")?;
            if moved.is_empty() {
                println!("Nothing to stash for {from_de} under {}.", home.display());
            } else {
                println!("Stashed {} path(s) for {from_de}:", moved.len());
                for p in &moved {
                    println!("  {p}");
                }
            }
            if *run_hooks {
                let hooks = de_migrate::discover_hooks(Path::new(de_migrate::PRE_SWITCH_HOOK_DIR))
                    .context("discovering pre-switch hooks")?;
                // to_de is unknown at this point (the target DE isn't detected by
                // this subcommand yet — see #68's "Depends on" note), so the
                // env var is left empty rather than guessed.
                let env = [
                    ("REBASE_FROM_DE".to_string(), from_de.clone()),
                    ("REBASE_TO_DE".to_string(), String::new()),
                    (
                        "REBASE_STASH_DIR".to_string(),
                        stash_dir.display().to_string(),
                    ),
                    ("REBASE_HOME".to_string(), home.display().to_string()),
                ];
                let results = de_migrate::run_hooks(&hooks, &env, *dry_run)
                    .context("running pre-switch hooks")?;
                print_hook_results(&results);
            }
            Ok(())
        }
        DeMigrateAction::Restore {
            to_de,
            home,
            stash_dir,
            run_hooks,
            dry_run,
        } => {
            let de = parse_de(to_de)?;
            let moved = de_migrate::restore(de, home, stash_dir, *dry_run)
                .context("restoring DE config")?;
            if moved.is_empty() {
                println!("Nothing to restore for {to_de} into {}.", home.display());
            } else {
                println!("Restored {} path(s) for {to_de}:", moved.len());
                for p in &moved {
                    println!("  {p}");
                }
            }
            if *run_hooks {
                let hooks = de_migrate::discover_hooks(Path::new(de_migrate::POST_SWITCH_HOOK_DIR))
                    .context("discovering post-switch hooks")?;
                let env = [
                    ("REBASE_FROM_DE".to_string(), String::new()),
                    ("REBASE_TO_DE".to_string(), to_de.clone()),
                    (
                        "REBASE_STASH_DIR".to_string(),
                        stash_dir.display().to_string(),
                    ),
                    ("REBASE_HOME".to_string(), home.display().to_string()),
                ];
                let results = de_migrate::run_hooks(&hooks, &env, *dry_run)
                    .context("running post-switch hooks")?;
                print_hook_results(&results);
            }
            Ok(())
        }
    }
}

pub fn parse_de(name: &str) -> Result<bootc_migrate_core::de_migrate::DesktopEnvironment> {
    bootc_migrate_core::de_migrate::parse_desktop_environment(name).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown desktop environment '{name}' \
             (expected gnome, kde, cosmic, niri, or xfce)"
        )
    })
}

pub fn print_hook_results(results: &[bootc_migrate_core::de_migrate::HookResult]) {
    if results.is_empty() {
        println!("No hooks found.");
        return;
    }
    for r in results {
        let status = if r.success { "ok" } else { "FAILED" };
        println!("  hook {} [{status}]", r.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_de_accepts_every_known_desktop_and_rejects_others() {
        use bootc_migrate_core::de_migrate::DesktopEnvironment;
        let cases: &[(&str, Option<DesktopEnvironment>)] = &[
            ("gnome", Some(DesktopEnvironment::Gnome)),
            ("GNOME", Some(DesktopEnvironment::Gnome)),
            ("kde", Some(DesktopEnvironment::Kde)),
            ("cosmic", Some(DesktopEnvironment::Cosmic)),
            ("niri", Some(DesktopEnvironment::Niri)),
            ("xfce", Some(DesktopEnvironment::Xfce)),
            ("plasma", None),
            ("", None),
        ];
        for (input, expected) in cases {
            match (parse_de(input), expected) {
                (Ok(de), Some(want)) => assert_eq!(de, *want, "parsing {input:?}"),
                (Err(e), None) => assert!(
                    e.to_string().contains("unknown desktop environment"),
                    "parsing {input:?}: unexpected error {e}"
                ),
                (got, want) => panic!("parsing {input:?}: got {got:?}, wanted {want:?}"),
            }
        }
    }
}
