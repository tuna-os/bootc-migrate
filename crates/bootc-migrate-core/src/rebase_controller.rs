//! Strategy executors for `bootc-rebase`.
//!
//! Each route the CLI can take is a method here, driven by a typed
//! configuration the CLI fills in once from its flags. Phase ordering belongs
//! to this module, not to the binary: the CLI translates arguments and
//! invokes a strategy, and nothing else.

use anyhow::{Context, Result, bail};

use crate::cross_base;
use crate::cross_family;
use crate::de_controller::DesktopMigrationController;
use crate::migration;
use crate::preflight::{self, readiness};
use crate::selinux;

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
    /// Proceed onto a target from another OS family with the cross-family
    /// `/etc` policy (bootc-migrate#256). Deliberately not implied by
    /// `force`: that flag waives warnings, this one selects a policy.
    pub accept_cross_base: bool,
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
        // This entry point is the ostree→composefs conversion specifically.
        // A composefs host reaching it means the router picked the wrong
        // strategy, so say which one it should have picked rather than
        // converting a repo that is not there.
        readiness::MigrationGate::ImageSwap => bail!(
            "System is already composefs-backed, so there is no backend to convert. \
             Use the image swap instead: `bootc-rebase --source-backend composefs \
             --target-backend composefs --target-image <image>`."
        ),
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

        // #256: a target from another OS family is refused unless accepted,
        // and when accepted Phase 4 applies the cross-family /etc policy.
        // Before the DE step and before any mutation, so a --dry-run shows
        // the verdict too.
        cross_family::gate(self.target_image, self.accept_cross_base)?;

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
            migration::EtcPolicy {
                overrides: None,
                accept_cross_base: self.accept_cross_base,
            },
        )?;

        if !self.dry_run
            && let Some(plan) = &de_plan
        {
            de.run_post_switch(plan, false)?;
        }
        Ok(())
    }
}

/// Stage `target_image` with `bootc switch` and verify via `bootc status
/// --json` that the staged deployment is exactly the requested image. Shared
/// by the OstreeDeploy and ImageSwap strategies — on both backends, `bootc
/// switch` performs the native staging (3-way /etc merge, shared /var) and
/// leaves the previous deployment as the rollback entry.
pub fn stage_via_bootc_switch(target_image: &str) -> Result<()> {
    println!("Staging deployment of {target_image} via `bootc switch`...");
    let status = std::process::Command::new("bootc")
        .args(["switch", target_image])
        .status()
        .map_err(|e| anyhow::anyhow!("failed to execute bootc switch: {e}"))?;
    if !status.success() {
        bail!("bootc switch {target_image} failed (exit {status})");
    }

    let out = std::process::Command::new("bootc")
        .args(["status", "--json"])
        .output()
        .map_err(|e| anyhow::anyhow!("failed to execute bootc status: {e}"))?;
    if !out.status.success() {
        bail!(
            "bootc status failed after switch: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let json: serde_json::Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| anyhow::anyhow!("parsing bootc status json: {e}"))?;
    match staged_image_from_status(&json) {
        Some(img) if staged_image_matches(target_image, img) => {
            println!("Staged deployment verified: {img}");
        }
        Some(img) => {
            bail!("bootc switch staged '{img}' but the requested target was '{target_image}'")
        }
        None => bail!("no staged deployment found after bootc switch"),
    }
    Ok(())
}

/// Where a composefs host keeps each deployment's `/etc` and `.origin`.
const COMPOSEFS_DEPLOY_ROOT: &str = "/sysroot/state/deploy";

/// The verity digest in `composefs=` on the kernel command line. A leading
/// `?` marks an image booted without signature enforcement; it is not part
/// of the digest.
pub fn booted_composefs_verity(cmdline: &str) -> Option<&str> {
    cmdline
        .split_whitespace()
        .find_map(|arg| arg.strip_prefix("composefs="))
        .map(|v| v.trim_start_matches('?'))
        .filter(|v| !v.is_empty())
}

/// The staged deployment's verity: bootc's own report when it gives one,
/// else the most recently written deployment directory other than the
/// booted one. `candidates` are `(verity, modified)` pairs for the
/// directories that carry a `.origin`.
fn pick_staged_verity(
    reported: Option<&str>,
    booted: Option<&str>,
    candidates: &[(String, std::time::SystemTime)],
) -> Option<String> {
    if let Some(v) = reported.filter(|v| !v.is_empty()) {
        return Some(v.to_string());
    }
    candidates
        .iter()
        .filter(|(v, _)| Some(v.as_str()) != booted)
        .max_by_key(|(_, t)| *t)
        .map(|(v, _)| v.clone())
}

/// The directory of the deployment `bootc switch` just staged on a
/// composefs host.
fn staged_composefs_deployment() -> Result<std::path::PathBuf> {
    let out = std::process::Command::new("bootc")
        .args(["status", "--json"])
        .output()
        .context("failed to execute bootc status")?;
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    let reported = json
        .pointer("/status/staged/composefs/verity")
        .and_then(|v| v.as_str());
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    let booted = booted_composefs_verity(&cmdline);
    let mut candidates = Vec::new();
    for entry in std::fs::read_dir(COMPOSEFS_DEPLOY_ROOT)
        .with_context(|| format!("failed to list {COMPOSEFS_DEPLOY_ROOT}"))?
    {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !entry.path().join(format!("{name}.origin")).exists() {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        candidates.push((name, modified));
    }
    let verity = pick_staged_verity(reported, booted, &candidates)
        .context("no staged composefs deployment found beside the booted one")?;
    Ok(std::path::Path::new(COMPOSEFS_DEPLOY_ROOT).join(verity))
}

/// Arm a first-boot SELinux relabel in the staged deployment when the
/// target enforces a policy the host did not label for.
///
/// `bootc switch` writes the merged `/etc` with the host's labels. A host
/// with no SELinux policy (Dakota) writes none, so a target that boots
/// enforcing (Utah) denies its own services every file in `/etc` and
/// `/var`: D-Bus fails and the machine never finishes booting. The
/// cross-family first-boot unit runs the target's `restorecon` over both
/// trees before `sysinit.target`, with the target's own policy. The unit
/// and its links are labelled here so the booting system can read them.
fn schedule_staged_composefs_relabel() -> Result<()> {
    let deploy = staged_composefs_deployment()?;
    let host = selinux::read_host_selinux_config();
    let target = selinux::read_deployment_selinux_config(&deploy);
    if !cross_family::relabel_needed(host.as_ref(), target.as_ref()) {
        return Ok(());
    }
    let unit = cross_family::render_firstboot_unit(&[], true)
        .context("the first-boot relabel unit rendered empty")?;
    let etc = deploy.join("etc");
    cross_family::install_firstboot_unit(&etc, &unit)?;
    let unit_ctx = "system_u:object_r:systemd_unit_file_t:s0";
    let etc_ctx = "system_u:object_r:etc_t:s0";
    let unit_dir = etc.join("systemd/system");
    let marker = etc.join(cross_family::FIRSTBOOT_MARKER);
    let labels = [
        (unit_dir.join(cross_family::FIRSTBOOT_UNIT), unit_ctx),
        (unit_dir.join("sysinit.target.wants"), unit_ctx),
        (
            unit_dir
                .join("sysinit.target.wants")
                .join(cross_family::FIRSTBOOT_UNIT),
            unit_ctx,
        ),
        (
            marker
                .parent()
                .map(std::path::Path::to_path_buf)
                .unwrap_or_default(),
            etc_ctx,
        ),
        (marker, etc_ctx),
    ];
    for (path, ctx) in &labels {
        let mut value = ctx.as_bytes().to_vec();
        value.push(0);
        if let Err(e) = rustix::fs::lsetxattr(
            path,
            c"security.selinux",
            &value,
            rustix::fs::XattrFlags::empty(),
        ) {
            eprintln!("Warning: could not label {} as {ctx}: {e}", path.display());
        }
    }
    println!(
        "[selinux] the target enforces SELinux and the host labelled nothing for it: \
         {} will relabel /etc and /var on first boot",
        cross_family::FIRSTBOOT_UNIT
    );
    Ok(())
}

/// libostree's record of a staged deployment awaiting finalization.
const STAGED_DEPLOYMENT_MARKER: &str = "/run/ostree/staged-deployment";

/// Complete the staged deployment now instead of at shutdown (#262).
///
/// libostree finalizes a staged deployment from the `ExecStop=` of
/// `ostree-finalize-staged.service`, i.e. while the machine is going down.
/// On a `bootc install to-disk` layout with no separate /boot partition,
/// /boot is a bind mount of the physical root's boot directory that systemd
/// can drop before that `ExecStop=` runs; libostree then remounts /boot
/// read-write without checking that it is still a mountpoint and gets
/// `EINVAL` (ostreedev/ostree#3365). The finalization fails, the bootloader
/// entries are never written, and the next boot lands in the previous
/// deployment with nothing on the console to say why.
///
/// Finalizing here, while /boot is mounted and the caller is still watching,
/// writes the bootloader entries at once and turns that silent misboot into
/// an error. Only an ostree-backed staging leaves the marker this reads; a
/// composefs host's `bootc switch` is finalized by bootc itself and is left
/// alone.
///
/// Call this last. Finalization marks the deployment root immutable, so every
/// write into it (the cross-base remap and /etc policy, their reports, the
/// SELinux relabel marker, the desktop-migration step) must come before.
fn finalize_staged_now() -> Result<()> {
    if !std::path::Path::new(STAGED_DEPLOYMENT_MARKER).exists() {
        return Ok(());
    }
    ensure_boot_mounted();
    println!("Finalizing the staged deployment now (writing the bootloader entries)...");
    // Stopping the unit runs its ExecStop= — the same `ostree admin
    // finalize-staged`, in the same sandbox, that shutdown would have run —
    // and leaves nothing for shutdown to repeat. Fall back to the command
    // itself where the unit is not active.
    let unit = "ostree-finalize-staged.service";
    let unit_active = std::process::Command::new("systemctl")
        .args(["is-active", "--quiet", unit])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let status = if unit_active {
        std::process::Command::new("systemctl")
            .args(["stop", unit])
            .status()
            .map_err(|e| anyhow::anyhow!("failed to execute systemctl stop {unit}: {e}"))?
    } else {
        std::process::Command::new("ostree")
            .args(["admin", "finalize-staged"])
            .status()
            .map_err(|e| anyhow::anyhow!("failed to execute ostree admin finalize-staged: {e}"))?
    };
    if !status.success() {
        bail!(
            "finalizing the staged deployment failed (exit {status}); the previous \
             deployment would boot next. See `journalctl -u {unit}` and \
             https://github.com/ostreedev/ostree/issues/3365."
        );
    }
    let out = std::process::Command::new("ostree")
        .args(["admin", "status"])
        .output()
        .map_err(|e| anyhow::anyhow!("failed to execute ostree admin status: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    match finalized_state(&stdout) {
        FinalizedState::Pending => {
            println!("Staged deployment finalized: it is the pending boot target.");
            Ok(())
        }
        FinalizedState::StillStaged => bail!(
            "the deployment is still staged after finalization; the previous deployment \
             would boot next"
        ),
        FinalizedState::NoOtherDeployment => bail!(
            "no pending deployment after finalization:\n{}",
            stdout.trim()
        ),
    }
}

/// Mount /boot if it is not, so the finalization that follows has a
/// mountpoint to remount. Best effort: a failure here surfaces as the
/// finalization's own error.
fn ensure_boot_mounted() {
    let mounted = std::process::Command::new("findmnt")
        .args(["-n", "/boot"])
        .stdout(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if mounted {
        return;
    }
    eprintln!("Note: /boot is not mounted; mounting it before finalization (#262).");
    let started = std::process::Command::new("systemctl")
        .args(["start", "boot.mount"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !started {
        let _ = std::process::Command::new("mount").arg("/boot").status();
    }
}

/// What `ostree admin status` says about the deployment that is not booted.
#[derive(Debug, PartialEq, Eq)]
enum FinalizedState {
    /// A deployment is listed as `(pending)`: finalized, boots next.
    Pending,
    /// A deployment is still listed as `(staged)`.
    StillStaged,
    /// Only the booted deployment is listed.
    NoOtherDeployment,
}

/// Classify `ostree admin status` output after a finalization attempt.
fn finalized_state(status_stdout: &str) -> FinalizedState {
    let lines: Vec<&str> = status_stdout.lines().map(str::trim).collect();
    if lines.iter().any(|l| l.ends_with("(staged)")) {
        FinalizedState::StillStaged
    } else if lines.iter().any(|l| l.ends_with("(pending)")) {
        FinalizedState::Pending
    } else {
        FinalizedState::NoOtherDeployment
    }
}

/// The staged deployment's image spec from `bootc status --json`, if any.
/// (Schema: `.status.staged.image.image.image` — ImageStatus → ImageReference
/// → image spec string; stable across bootc 1.x.)
fn staged_image_from_status(status: &serde_json::Value) -> Option<&str> {
    status
        .pointer("/status/staged/image/image/image")
        .and_then(|v| v.as_str())
}

/// Whether the image bootc reports as staged is the one the user asked for.
///
/// Compares by equality after stripping the transport prefix from both sides
/// (`docker://`, `ostree-unverified-registry:`, …) — bootc's status output
/// omits the transport the user may have typed. Deliberately NOT a substring
/// match: `bluefin:gts-testing` must not "verify" a request for
/// `bluefin:gts`.
fn staged_image_matches(requested: &str, staged: &str) -> bool {
    fn strip_transport(image: &str) -> &str {
        // `scheme://rest` transports first, then the `prefix:name` transports
        // whose remainder still contains a registry path (so a plain
        // `registry/image:tag` — whose only ':' precedes the tag — survives).
        if let Some((_, rest)) = image.split_once("://") {
            return rest;
        }
        for prefix in [
            "ostree-unverified-registry:",
            "ostree-image-signed:",
            "ostree-remote-registry:",
            "containers-storage:",
            "registry:",
        ] {
            if let Some(rest) = image.strip_prefix(prefix) {
                return rest;
            }
        }
        image
    }
    strip_transport(requested) == strip_transport(staged)
}

/// Everything the ComposeFS image-swap strategy needs, translated from CLI
/// flags exactly once by the caller.
#[derive(Debug, Clone, Copy)]
pub struct ImageSwapConfig<'a> {
    pub target_image: &'a str,
    pub dry_run: bool,
    pub force: bool,
    pub de_migrate: bool,
    /// Proceed onto a target from another OS family (bootc-migrate#256).
    /// This route stages with `bootc switch` and applies no `/etc` policy
    /// of its own, so acceptance here means the native merge's result.
    pub accept_cross_base: bool,
}

/// Scenario A' (issue #66): swap the image on a composefs-backed system —
/// no backend conversion. `bootc switch` stages the target natively; this
/// route is gating + switch + verification. The degenerate direct-store path
/// (for targets whose bootc cannot switch) is out of scope until the #13
/// store-selection work lands.
impl ImageSwapConfig<'_> {
    pub fn run(&self) -> Result<()> {
        validate_target_image(self.target_image)?;

        if self.dry_run {
            println!("*** DRY RUN MODE — no changes will be made ***");
        }
        println!("Checking system state...");

        // The booted deployment must actually be composefs-backed: the router
        // may have been told --source-backend composefs explicitly, but staging
        // relies on the running bootc's composefs support.
        let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
        if !cmdline.contains("composefs=") && !self.force {
            bail!(
                "System is not booted from a composefs deployment (/proc/cmdline has no \
                 composefs= parameter). Use --force to override, or re-run with \
                 --source-backend auto."
            );
        }

        // #256: refuse a cross-family target unless accepted. Unlike the
        // conversion route there is no policy to apply afterwards — `bootc
        // switch`'s native merge stages the /etc — so say so.
        cross_family::gate(self.target_image, self.accept_cross_base)?;
        if self.accept_cross_base {
            eprintln!(
                "Note: the image swap stages /etc with `bootc switch`'s native merge; the \
                 cross-family /etc policy is not applied on this route. If the target is \
                 from another OS family, expect to reconcile family-specific configuration \
                 by hand after the reboot."
            );
        }

        // #68: decide the DE step before staging so a --dry-run shows it too.
        let de = DesktopMigrationController::new(self.de_migrate, self.target_image);
        let de_plan = de.plan_or_report();

        if self.dry_run {
            if let Some(plan) = &de_plan {
                de.preview(plan)?;
            }
            println!("[DRY RUN] Would run: bootc switch {}", self.target_image);
            return Ok(());
        }

        let _sleep_guard = Some(migration::SleepGuard::new("bootc image swap in progress"));

        if let Some(plan) = &de_plan {
            de.run_pre_switch(plan, false)?;
        }

        stage_via_bootc_switch(self.target_image)?;

        if let Some(plan) = &de_plan {
            de.run_post_switch(plan, false)?;
        }

        schedule_staged_composefs_relabel()?;

        finalize_staged_now()?;

        println!(
            "Image swap staged. Reboot to enter the new deployment; the previous \
             deployment remains in the boot menu as rollback."
        );
        Ok(())
    }
}

/// Everything the OSTree-deploy strategy needs, translated from CLI flags
/// exactly once by the caller.
#[derive(Debug, Clone, Copy)]
pub struct OstreeDeployConfig<'a> {
    pub target_image: &'a str,
    pub dry_run: bool,
    pub force: bool,
    pub skip_preflight: bool,
    pub accept_cross_base: bool,
    pub de_migrate: bool,
}

/// Scenario A (issue #30): re-base to another image as a plain OSTree
/// deployment. `bootc switch` already does the heavy lifting on an
/// OSTree-backed system — staging the target with OSTree's native 3-way /etc
/// merge and shared /var — so this route is preflight + gating + `bootc
/// switch` + verification. The previous deployment stays as the rollback
/// entry, matching the engine's two-phase contract.
///
/// The one place that merge is second-guessed is a cross-base re-base, where
/// "keep the user's value" is the wrong answer for a path whose vendor
/// default also moved — see [`cross_base::apply_cross_base_etc_policy`].
///
/// Bootloader: per the decision on issue #64, this route will migrate to
/// systemd-boot when the system is ready — wired in once #65's audited
/// bootloader entry point lands. Until then the current bootloader is kept.
impl OstreeDeployConfig<'_> {
    pub fn run(&self) -> Result<()> {
        validate_target_image(self.target_image)?;

        if self.dry_run {
            println!("*** DRY RUN MODE — no changes will be made ***");
        }
        println!("Checking system state...");
        let report = preflight::run_preflight_checks()?;

        if report.booted_backend != Some(crate::rebase_plan::Backend::Ostree) && !self.force {
            bail!(
                "System is not booted into an OSTree deployment. Cannot perform an ostree re-base."
            );
        }
        if report.pending_transaction != preflight::PendingTransactionStatus::Clean
            && !self.force
            && !self.skip_preflight
        {
            bail!(
                "Pending OSTree transaction detected: {}. Complete or undeploy it first \
                 (see `ostree admin status`).",
                report.pending_transaction
            );
        }
        if report.esp_ready_for_systemd_boot && report.nvram_writable {
            println!(
                "Note: system is ready for systemd-boot; bootloader migration will be \
                 integrated into this route via the migrate-bootloader work (#65). \
                 Keeping the current bootloader for this re-base."
            );
        }

        // Cross-base gate (#67 part 1): always print the remap report before
        // anything is staged, and refuse without --accept-cross-base so the
        // blast radius is visible first — including in --dry-run.
        let cross_base_plan =
            cross_base::gate_cross_base(self.target_image, self.accept_cross_base, self.force)?;

        // #80: advisory identity-DB gap check, independent of cross-base status
        // (the motivating case — Bluefin GNOME → Aurora KDE — is same-base).
        cross_base::warn_identity_merge_gap(self.target_image);

        // #68: decide the DE step before staging so a --dry-run shows it too.
        let de = DesktopMigrationController::new(self.de_migrate, self.target_image);
        let de_plan = de.plan_or_report();

        if self.dry_run {
            if let Some(plan) = &de_plan {
                de.preview(plan)?;
            }
            println!("[DRY RUN] Would run: bootc switch {}", self.target_image);
            return Ok(());
        }

        let _sleep_guard = Some(migration::SleepGuard::new(
            "bootc ostree re-base in progress",
        ));

        if let Some(plan) = &de_plan {
            de.run_pre_switch(plan, false)?;
        }

        stage_via_bootc_switch(self.target_image)?;

        if let Some(plan) = &cross_base_plan {
            let staged_root = cross_base::staged_deployment_root()
                .context("failed to locate the staged deployment for cross-base post-processing")?;
            cross_base::apply_cross_base_remap(&staged_root, plan)?;
            cross_base::apply_cross_base_etc_policy(&staged_root)?;

            // #67: when the base family changes, the SELinux policy type may
            // differ — schedule /.autorelabel so the target's policy is applied
            // to every file on first boot.
            match selinux::check_and_schedule_autorelabel(&staged_root) {
                Ok(true) => println!(
                    "SELinux policy type changed for the cross-base target; \
                     scheduled /.autorelabel in the staged deployment."
                ),
                Ok(false) => {}
                Err(e) => eprintln!("Warning: failed to check SELinux policy compatibility: {e:#}"),
            }
        }

        if let Some(plan) = &de_plan {
            de.run_post_switch(plan, false)?;
        }

        // #262: last, after every write into the deployment root above.
        finalize_staged_now()?;

        println!(
            "Re-base staged. Reboot to enter the new deployment; the previous \
             deployment remains in the boot menu as rollback."
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn booted_composefs_verity_table() {
        assert_eq!(
            booted_composefs_verity("root=UUID=1 composefs=abc12 rw"),
            Some("abc12")
        );
        assert_eq!(
            booted_composefs_verity("composefs=?abc12 quiet"),
            Some("abc12")
        );
        assert_eq!(booted_composefs_verity("rd.composefs=1 root=UUID=1"), None);
        assert_eq!(booted_composefs_verity("ostree=/ostree/boot.1/x/0"), None);
        assert_eq!(booted_composefs_verity("composefs= quiet"), None);
    }

    #[test]
    fn pick_staged_verity_table() {
        use std::time::{Duration, UNIX_EPOCH};
        let t = |s| UNIX_EPOCH + Duration::from_secs(s);
        let c = vec![("base".to_string(), t(10)), ("new".to_string(), t(20))];
        // bootc's report wins.
        assert_eq!(
            pick_staged_verity(Some("x"), Some("base"), &c).as_deref(),
            Some("x")
        );
        // Otherwise the newest non-booted deployment.
        assert_eq!(
            pick_staged_verity(None, Some("base"), &c).as_deref(),
            Some("new")
        );
        // The booted one is never picked, even when newest.
        let c2 = vec![("old".to_string(), t(5)), ("base".to_string(), t(30))];
        assert_eq!(
            pick_staged_verity(None, Some("base"), &c2).as_deref(),
            Some("old")
        );
        assert_eq!(pick_staged_verity(Some(""), Some("base"), &c[..1]), None);
    }

    #[test]
    fn finalized_state_table() {
        let staged = "  default 594b.0 (staged)\n    origin: <unknown origin type>\n* default bef2.0\n    origin: <unknown origin type>\n";
        let pending = "  default 594b.0 (pending)\n    origin: <unknown origin type>\n* default bef2.0\n    origin: <unknown origin type>\n";
        let booted_only = "* default bef2.0\n    origin: <unknown origin type>\n";
        let rollback_only =
            "* default bef2.0\n    origin: <unknown origin type>\n  default 1234.0 (rollback)\n";
        for (input, want) in [
            (staged, FinalizedState::StillStaged),
            (pending, FinalizedState::Pending),
            (booted_only, FinalizedState::NoOtherDeployment),
            (rollback_only, FinalizedState::NoOtherDeployment),
            ("", FinalizedState::NoOtherDeployment),
        ] {
            assert_eq!(finalized_state(input), want, "input: {input:?}");
        }
    }

    #[test]
    fn staged_image_extracted_from_status_json() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{"status":{"staged":{"image":{"image":{"image":"ghcr.io/projectbluefin/bluefin:gts","transport":"registry"}}}}}"#,
        )
        .unwrap();
        assert_eq!(
            staged_image_from_status(&json),
            Some("ghcr.io/projectbluefin/bluefin:gts")
        );
    }

    #[test]
    fn staged_match_exact() {
        assert!(staged_image_matches(
            "ghcr.io/projectbluefin/bluefin:gts",
            "ghcr.io/projectbluefin/bluefin:gts"
        ));
    }

    #[test]
    fn staged_match_strips_requested_transport() {
        assert!(staged_image_matches(
            "docker://ghcr.io/projectbluefin/bluefin:gts",
            "ghcr.io/projectbluefin/bluefin:gts"
        ));
        assert!(staged_image_matches(
            "ostree-unverified-registry:ghcr.io/projectbluefin/bluefin:gts",
            "ghcr.io/projectbluefin/bluefin:gts"
        ));
    }

    #[test]
    fn staged_match_rejects_tag_extension() {
        // The old substring check accepted this: gts-testing contains gts.
        assert!(!staged_image_matches(
            "ghcr.io/projectbluefin/bluefin:gts",
            "ghcr.io/projectbluefin/bluefin:gts-testing"
        ));
        assert!(!staged_image_matches(
            "ghcr.io/projectbluefin/bluefin:gts-testing",
            "ghcr.io/projectbluefin/bluefin:gts"
        ));
    }

    #[test]
    fn staged_match_rejects_different_image() {
        assert!(!staged_image_matches(
            "ghcr.io/projectbluefin/bluefin:gts",
            "ghcr.io/projectbluefin/dakota:stable"
        ));
    }

    #[test]
    fn staged_match_plain_tag_colon_survives_transport_strip() {
        // A bare registry/image:tag has a ':' but no transport — it must not
        // get mangled by the prefix stripping.
        assert!(staged_image_matches(
            "quay.io/fedora/fedora-bootc:42",
            "quay.io/fedora/fedora-bootc:42"
        ));
    }

    #[test]
    fn staged_image_absent_when_nothing_staged() {
        let json: serde_json::Value =
            serde_json::from_str(r#"{"status":{"staged":null,"booted":{}}}"#).unwrap();
        assert_eq!(staged_image_from_status(&json), None);
    }

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
