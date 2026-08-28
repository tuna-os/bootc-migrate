//! Strategy executors for `bootc-rebase`.
//!
//! Each route the CLI can take is a method here, driven by a typed
//! configuration the CLI fills in once from its flags. Phase ordering belongs
//! to this module, not to the binary: the CLI translates arguments and
//! invokes a strategy, and nothing else.

use anyhow::{Result, bail};

use crate::de_controller::DesktopMigrationController;
use crate::migration;
use crate::preflight::{self, readiness};

/// Everything the OSTree-to-ComposeFS migration strategy needs, translated
/// from CLI flags exactly once by the caller.
#[derive(Debug, Clone, Copy)]
pub struct CoreMigrationConfig<'a> {
    pub target_image: &'a str,
    pub bootloader: &'a str,
    pub dry_run: bool,
    pub skip_import: bool,
    pub skip_preflight: bool,
    pub force: bool,
    pub de_migrate: bool,
}

/// Reject target images that would corrupt the `.origin` file it is written
/// into. That file is INI, so an embedded newline or NUL lets a crafted image
/// reference inject additional keys.
///
/// Shared by every strategy: one definition means the OstreeDeploy, ImageSwap,
/// and core-migration routes cannot disagree about what is accepted.
pub fn validate_target_image(target_image: &str) -> Result<()> {
    if target_image.contains('\n') || target_image.contains('\r') || target_image.contains('\0') {
        bail!("--target-image contains invalid characters (newlines, nulls).");
    }
    Ok(())
}

/// Turn a readiness gate into a proceed-or-refuse decision.
///
/// Split out from the migration itself so the refusal policy is testable
/// without a live system: `bootc-rebase` is non-interactive by design, so
/// where the interactive migrator would prompt, this refuses with the flag
/// that would accept the cost.
fn gate_decision(gate: readiness::MigrationGate) -> Result<()> {
    match gate {
        readiness::MigrationGate::Proceed => Ok(()),
        readiness::MigrationGate::Refuse(reason) => bail!("{reason}"),
        readiness::MigrationGate::ConfirmFullCopy => {
            // bootc-rebase is non-interactive by design: no prompt, just a
            // clear instruction (the migrator binary offers the y/N prompt).
            bail!(
                "Reflink support not detected on /sysroot — the migration would perform a \
                 full copy of repository objects. Re-run with --force to accept the extra \
                 disk usage."
            );
        }
    }
}

impl CoreMigrationConfig<'_> {
    /// Run the OSTree-to-ComposeFS migration: preflight and its gate, the
    /// desktop pre-switch step, the five-phase migration, then the desktop
    /// post-switch step.
    ///
    /// The desktop decision is made before the pipeline runs so a `--dry-run`
    /// shows it too, and the stash happens before anything is staged.
    pub fn run(&self) -> Result<()> {
        validate_target_image(self.target_image)?;

        if self.dry_run {
            println!("*** DRY RUN MODE — no changes will be made ***");
        }
        println!("Checking system state...");

        let report = preflight::run_preflight_checks()?;
        readiness::print_report(&report);
        readiness::print_readiness(&report);
        gate_decision(readiness::gate(&report, self.force, self.skip_preflight))?;

        // #68: decide the DE step before the pipeline runs so a --dry-run shows
        // it too, and stash before anything is staged.
        let de = DesktopMigrationController::new(self.de_migrate, self.target_image);
        let de_plan = de.plan_or_report();
        if self.dry_run {
            if let Some(plan) = &de_plan {
                de.preview(plan)?;
            }
        } else if let Some(plan) = &de_plan {
            de.run_pre_switch(plan, false)?;
        }

        println!("Starting migration to OCI image: {}...", self.target_image);
        // bootc-rebase has no interactive Config Drift Review wiring yet
        // (issue #15's TUI lives in the `bootc-migrate` binary); always fall
        // back to the default 3-way /etc merge.
        migration::run_migration(
            &report,
            self.target_image,
            self.dry_run,
            self.skip_import,
            self.bootloader,
            self.force,
            None,
        )?;

        if !self.dry_run
            && let Some(plan) = &de_plan
        {
            de.run_post_switch(plan, false)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `.origin` file this reference is written into is INI, so a newline
    /// or NUL in the image name could inject additional keys.
    #[test]
    fn target_image_validation_rejects_ini_injection() {
        for good in ["ghcr.io/org/img:tag", "quay.io/o/i@sha256:abc", ""] {
            assert!(
                validate_target_image(good).is_ok(),
                "should accept {good:?}"
            );
        }
        for bad in [
            "ghcr.io/o/i\ntag",
            "ghcr.io/o/i\r\ntag",
            "ghcr.io/o/i\0tag",
            "\n[boot]\nkey=value",
        ] {
            let err = validate_target_image(bad).unwrap_err().to_string();
            assert!(
                err.contains("invalid characters"),
                "should reject {bad:?}, got {err}"
            );
        }
    }

    /// bootc-rebase never prompts, so a gate that would ask the interactive
    /// migrator a question must refuse here and name the flag that accepts it.
    #[test]
    fn gate_decisions_are_testable_without_a_live_system() {
        assert!(gate_decision(readiness::MigrationGate::Proceed).is_ok());

        let refused = gate_decision(readiness::MigrationGate::Refuse("no space".into()))
            .unwrap_err()
            .to_string();
        assert_eq!(refused, "no space");

        let full_copy = gate_decision(readiness::MigrationGate::ConfirmFullCopy)
            .unwrap_err()
            .to_string();
        assert!(full_copy.contains("Reflink support not detected"));
        assert!(
            full_copy.contains("--force"),
            "a non-interactive refusal must name the flag that accepts it"
        );
    }
}
