//! `bootc-rebase commit`: route-aware plan presentation, refusal on failed
//! checks, and typed confirmation for the destructive apply.

use anyhow::{Context, Result, bail};
use bootc_migrate_core::ostree_install_commit::{self as compose_commit};
use bootc_migrate_core::package_install_commit::{self as package_commit};

use crate::CommitArgs;
use crate::boot_entries::{APPLY_CONFIRMATION, confirmation_accepted};

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

fn refuse_failed_checks(checks: &[&compose_commit::Check], force: bool) -> Result<()> {
    if checks.is_empty() {
        return Ok(());
    }
    let names: Vec<&str> = checks.iter().map(|check| check.name).collect();
    let forceable = checks.iter().all(|check| !check.hard);
    bail!(
        "commit refused, failed check(s): {}.{}",
        names.join("; "),
        if forceable && !force {
            " Review the report above; --force proceeds anyway."
        } else {
            " --force does not override these."
        }
    )
}

fn run_composefs(args: &CommitArgs) -> Result<()> {
    println!("=== Commit: composefs -> ostree (removing the old composefs state) ===");
    if !args.apply {
        println!("*** DRY RUN — nothing will be deleted (pass --apply to delete) ***");
    }
    let plan = compose_commit::plan(
        &compose_commit::Layout::host(),
        &compose_commit::HostFacts::gather(),
    )?;
    plan.print();
    refuse_failed_checks(&plan.blocking(args.force), args.force)?;
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
    let total = compose_commit::format_bytes(plan.total_bytes());
    if !args.yes && !prompt_for_apply_confirmation(&total)? {
        println!("Not confirmed — nothing was deleted.");
        return Ok(());
    }
    let outcome = compose_commit::execute_host(&plan, args.force)?;
    println!(
        "Commit complete: {} path(s) deleted, {} reclaimed. The OSTree deployment is now the \
         only installed system.",
        outcome.deleted.len(),
        compose_commit::format_bytes(outcome.reclaimed_bytes)
    );
    Ok(())
}

fn run_package(args: &CommitArgs) -> Result<()> {
    println!("=== Commit: package -> ostree (reclaiming the old package OS) ===");
    if !args.apply {
        println!("*** DRY RUN — nothing will be deleted (pass --apply to delete) ***");
    }
    let layout = package_commit::Layout::host();
    let plan = package_commit::plan(
        &layout,
        &package_commit::HostFacts::gather(&layout),
        args.move_home,
    )?;
    plan.print();
    let blocking = plan.blocking(args.force);
    if !blocking.is_empty() {
        let names: Vec<&str> = blocking.iter().map(|check| check.name).collect();
        bail!(
            "commit refused, failed check(s): {}. --force does not override package-host safety checks.",
            names.join("; ")
        );
    }
    if plan.total_bytes() == 0 && plan.home_move.is_none() {
        println!("Nothing to delete.");
        return Ok(());
    }
    if !args.apply {
        println!("Dry run complete: nothing was deleted. Re-run with --apply to delete.");
        return Ok(());
    }
    let total = compose_commit::format_bytes(plan.total_bytes());
    if !args.yes && !prompt_for_apply_confirmation(&total)? {
        println!("Not confirmed — nothing was deleted.");
        return Ok(());
    }
    let outcome = package_commit::execute_host(&plan, args.force)?;
    println!(
        "Commit complete: {} path(s) deleted, {} reclaimed. The old package OS and its \
         Previous system entry are gone.",
        outcome.deleted.len(),
        compose_commit::format_bytes(outcome.reclaimed_bytes)
    );
    Ok(())
}

pub(crate) fn run(args: &CommitArgs) -> Result<()> {
    let package_layout = package_commit::Layout::host();
    if package_commit::report_present(&package_layout) {
        run_package(args)
    } else {
        run_composefs(args)
    }
}
