//! `bootc-rebase commit` (issue #315): the CLI face of
//! `bootc_migrate_core::ostree_install_commit` — plan presentation, the
//! refusal on failed checks, and the destructive-apply confirmation.

use anyhow::{Context, Result, bail};
use bootc_migrate_core::ostree_install_commit::{self as commit, HostFacts, Layout};

use crate::CommitArgs;
use crate::boot_entries::{APPLY_CONFIRMATION, confirmation_accepted};

/// Ask for the typed confirmation on stdin.
fn prompt_for_apply_confirmation(total: &str) -> Result<bool> {
    use std::io::Write;
    print!("Type '{APPLY_CONFIRMATION}' to permanently delete the paths above ({total}): ");
    std::io::stdout().flush().context("flushing prompt")?;
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .context("reading confirmation from stdin")?;
    Ok(confirmation_accepted(&answer))
}

pub(crate) fn run(args: &CommitArgs) -> Result<()> {
    println!("=== Commit: composefs -> ostree (removing the old composefs state) ===");
    if !args.apply {
        println!("*** DRY RUN — nothing will be deleted (pass --apply to delete) ***");
    }
    let plan = commit::plan(&Layout::host(), &HostFacts::gather())?;
    plan.print();

    let blocking = plan.blocking(args.force);
    if !blocking.is_empty() {
        let names: Vec<&str> = blocking.iter().map(|c| c.name).collect();
        let forceable = blocking.iter().all(|c| !c.hard);
        bail!(
            "commit refused, failed check(s): {}.{}",
            names.join("; "),
            if forceable {
                " Review the report above; --force proceeds anyway."
            } else {
                " --force does not override these."
            }
        );
    }
    if args.force && !plan.blocking(false).is_empty() {
        eprintln!("Warning: --force given; proceeding past the failed check(s) above.");
    }
    if plan.targets.is_empty() {
        println!("Nothing to delete.");
        return Ok(());
    }
    if !args.apply {
        println!("Dry run complete: nothing was deleted. Re-run with --apply to delete.");
        return Ok(());
    }
    let total = commit::format_bytes(plan.total_bytes());
    if !args.yes && !prompt_for_apply_confirmation(&total)? {
        println!("Not confirmed — nothing was deleted.");
        return Ok(());
    }
    let outcome = commit::execute_host(&plan, args.force)?;
    println!(
        "Commit complete: {} path(s) deleted, {} reclaimed. The OSTree deployment is now the \
         only installed system.",
        outcome.deleted.len(),
        commit::format_bytes(outcome.reclaimed_bytes)
    );
    Ok(())
}
