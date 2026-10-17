//! Strategy executors for `bootc-rebase`.
//!
//! Each route the CLI can take is a method here, driven by a typed
//! configuration the CLI fills in once from its flags. Phase ordering belongs
//! to this module, not to the binary: the CLI translates arguments and
//! invokes a strategy, and nothing else.

use anyhow::{Context, Result, bail};

use std::path::{Path, PathBuf};

use crate::cross_base;
use crate::cross_family;
use crate::de_controller::DesktopMigrationController;
use crate::migration;
use crate::ostree_install;
use crate::preflight::{self, readiness};
use crate::selinux;
use crate::sysroot::{SYSROOT, SysrootWritable};
use crate::{remap, scan};

/// Resolve the vendor `/etc` defaults in a staged deployment.
fn target_vendor_etc_dir(root: &Path) -> PathBuf {
    let usr_etc = root.join("usr/etc");
    if usr_etc.is_dir() {
        usr_etc
    } else {
        let factory = root.join("usr/share/factory/etc");
        if factory.is_dir() {
            factory
        } else {
            root.join("etc")
        }
    }
}

/// Resolve the source deployment's vendor `/etc` defaults.
fn source_vendor_etc_dir(root: &Path) -> PathBuf {
    let usr_etc = root.join("usr/etc");
    if usr_etc.is_dir() {
        usr_etc
    } else {
        PathBuf::from("/usr/etc")
    }
}

fn cross_family_remap_plan(staged_root: &Path, target_vendor_etc: &Path) -> remap::RemapPlan {
    let source_passwd = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
    let source_group = std::fs::read_to_string("/etc/group").unwrap_or_default();
    let target_passwd = cross_family::target_identity_db(staged_root, target_vendor_etc, "passwd");
    let target_group = cross_family::target_identity_db(staged_root, target_vendor_etc, "group");
    remap::plan_remap(
        &remap::parse_passwd(&source_passwd),
        &remap::parse_group(&source_group),
        &remap::parse_passwd(&target_passwd),
        &remap::parse_group(&target_group),
    )
}

fn write_cross_family_report(
    staged_root: &Path,
    plan: &cross_family::CrossFamilyPlan,
    etc_plan: cross_family::EtcCrossFamilyPlan,
    remap_plan: remap::RemapPlan,
    rechowned_now: usize,
    var_remap_deferred: bool,
    relabel_scheduled: bool,
    firstboot_unit_installed: bool,
) -> Result<()> {
    let remap_report = staged_root.join("bootc-migrate-remap-report.json");
    std::fs::write(&remap_report, remap_plan.to_json())
        .with_context(|| format!("failed to write {}", remap_report.display()))?;
    let report = cross_family::CrossFamilyReport {
        host: plan.host.clone(),
        target: plan.target.clone(),
        etc: etc_plan,
        remap: remap_plan,
        rechowned_now,
        var_remap_deferred,
        relabel_scheduled,
        firstboot_unit_installed,
    };
    let report_path = staged_root.join(cross_family::REPORT_FILE);
    std::fs::write(
        &report_path,
        serde_json::to_string_pretty(&report).expect("CrossFamilyReport serialization cannot fail"),
    )
    .with_context(|| format!("failed to write {}", report_path.display()))?;
    println!("Cross-family report written to {}", report_path.display());
    Ok(())
}

/// Best-effort SELinux labels for first-boot units, their wants links,
/// markers, and the verify/cleanup helpers staged by the image-swap route.
fn label_firstboot_files(etc: &Path) {
    let unit_ctx = "system_u:object_r:systemd_unit_file_t:s0";
    let etc_ctx = "system_u:object_r:etc_t:s0";
    let unit_dir = etc.join("systemd/system");
    let marker = etc.join(cross_family::FIRSTBOOT_MARKER);
    let verify_marker = etc.join(crate::firstboot_verify::VERIFY_MARKER);
    let labels = [
        (unit_dir.join(cross_family::FIRSTBOOT_UNIT), unit_ctx),
        (
            unit_dir.join(crate::firstboot_verify::VERIFY_UNIT),
            unit_ctx,
        ),
        (unit_dir.join("sysinit.target.wants"), unit_ctx),
        (
            unit_dir
                .join("sysinit.target.wants")
                .join(cross_family::FIRSTBOOT_UNIT),
            unit_ctx,
        ),
        (
            unit_dir
                .join("sysinit.target.wants")
                .join(crate::firstboot_verify::VERIFY_UNIT),
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
        (
            etc.join(crate::firstboot_verify::VERIFY_SCRIPT_REL),
            etc_ctx,
        ),
        (verify_marker, etc_ctx),
        (
            etc.join(crate::firstboot_verify::PROMPT_SCRIPT_REL),
            etc_ctx,
        ),
        (
            etc.join(crate::firstboot_verify::PROMPT_DESKTOP_REL),
            etc_ctx,
        ),
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
}

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
        // Best-effort JSON snapshot alongside the human-readable report
        // (bootc-migrate#229) — never blocks the migration on a write
        // failure.
        preflight::write_snapshot(
            "bootc-rebase",
            env!("CARGO_PKG_VERSION"),
            &std::env::args().collect::<Vec<_>>(),
            &report,
        );
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
    let mut child = std::process::Command::new("bootc")
        .args(["switch", target_image])
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to execute bootc switch: {e}"))?;
    let status = wait_with_stage_progress(
        &mut child,
        std::time::Duration::from_secs(15),
        &mut std::io::stdout(),
    )?;
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

/// The `setfiles` relabel command for the staged composefs deployment's `/etc`.
pub fn composefs_relabel_command(
    target_image: &str,
    deploy_root: &Path,
    policy_type: &str,
) -> Vec<String> {
    let etc = deploy_root.join("etc");
    let mut cmds = ostree_install::relabel_command(target_image, &[(deploy_root, etc.as_path())]);
    if let Some(argv) = cmds.first_mut()
        && let Some(fc) = argv
            .iter_mut()
            .find(|a| a.as_str() == "/etc/selinux/targeted/contexts/files/file_contexts")
    {
        *fc = format!("/etc/selinux/{policy_type}/contexts/files/file_contexts");
    }
    cmds.into_iter().next().unwrap_or_default()
}

/// Relabel the staged composefs deployment's `/etc` with the target's policy.
fn relabel_staged_composefs_etc(
    target_image: &str,
    deploy_root: &Path,
    target: Option<&selinux::SelinuxConfig>,
) -> Result<()> {
    let policy_type = target
        .and_then(|t| t.selinux_type.as_deref())
        .unwrap_or("targeted");
    let argv = composefs_relabel_command(target_image, deploy_root, policy_type);
    if argv.is_empty() {
        return Ok(());
    }
    println!("[selinux] {}", argv.join(" "));
    let status = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .status()
        .with_context(|| "failed to execute setfiles in the target image")?;
    if !status.success() {
        bail!("setfiles in the target image failed with exit code {status}");
    }
    println!("[selinux] labelled merged /etc with the target's SELinux policy");
    Ok(())
}

/// Arm the image-swap first-boot unit in the staged deployment.
///
/// On composefs, `bootc switch` merges the host's `/etc` into the new
/// deployment. Two things break when the host is another distribution
/// (Dakota to Utah):
///
/// - The host's locally created account databases replace the target's,
///   so the target's system users (`dbus`) are missing. The unit runs the
///   target's `systemd-sysusers` and `ldconfig` to restore them.
/// - A host with no SELinux policy writes the merged files unlabeled, and
///   a target that boots enforcing then denies its own services every file
///   in `/etc` and `/var`. When the target enforces a policy the host did
///   not label for, the merged `/etc` is labelled here before first boot
///   with `setfiles` from the target image, and the unit runs the target's
///   `restorecon` over `/var` and `/etc` on first boot.
///
/// The unit runs before `sysinit.target`. Its own files are labelled here
/// so the booting system can read them.
fn schedule_image_swap_firstboot(target_image: &str) -> Result<()> {
    let deploy = staged_composefs_deployment()?;
    let host = selinux::read_host_selinux_config();
    let target = selinux::read_deployment_selinux_config(&deploy);
    let relabel = cross_family::relabel_needed(host.as_ref(), target.as_ref());
    let unit = cross_family::render_image_swap_firstboot_unit(relabel);
    let etc = deploy.join("etc");
    cross_family::install_firstboot_unit(&etc, &unit)?;
    crate::firstboot_verify::install_verify_probe(&etc)?;
    crate::firstboot_verify::install_cleanup_prompt(&etc)?;
    label_firstboot_files(&etc);
    println!(
        "[firstboot] {} will add the target's system users and rebuild the library cache on first boot",
        cross_family::FIRSTBOOT_UNIT
    );
    if relabel {
        println!(
            "[selinux] the target enforces SELinux and the host labelled nothing for it: \
             labelling merged /etc before first boot; {} will relabel /var",
            cross_family::FIRSTBOOT_UNIT
        );
        // bootc merges the running /etc into the staged one from the
        // ExecStop= of bootc-finalize-staged.service, at shutdown. A label
        // set now would cover only the target's own files: everything the
        // shutdown merge copies from this unlabelled host (machine-id,
        // passwd, ...) would still boot unlabelled, and PID 1 and journald
        // are denied it before any first-boot unit can run (#275). So do
        // the merge now, then label its result.
        // Fetch the image whose setfiles labels /etc before the boot entries
        // are swapped, so a missing podman or registry fails here with the
        // old deployment still the default.
        println!("[selinux] pulling {target_image} with podman for its setfiles and policy...");
        let pulled = std::process::Command::new("podman")
            .args(["pull", target_image])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !pulled {
            bail!(
                "could not pull {target_image} with podman to label the merged /etc for the \
                 target's SELinux policy. The deployment is staged but would boot unlabelled: \
                 run `bootc rollback` before you reboot, or fix podman and run again."
            );
        }
        finalize_composefs_staged_now()?;
        relabel_staged_composefs_etc(target_image, &deploy, target.as_ref()).context(
            "labelling the merged /etc failed after finalization; the new deployment boots \
             next and its first-boot unit relabels /etc, but early services can fail. Run \
             `bootc rollback` to keep the current deployment.",
        )?;
    }
    Ok(())
}

/// Apply the cross-family policy after the composefs finalization has merged
/// the live `/etc` into the staged deployment. The deployment's `/etc` is
/// writable state; `/var` remains live until the target boots, so its remap
/// steps are deferred to the combined first-boot unit.
fn apply_cross_family_image_swap_policy(
    target_image: &str,
    deploy: &Path,
    plan: &cross_family::CrossFamilyPlan,
) -> Result<()> {
    let source_defaults = source_vendor_etc_dir(Path::new("/"));
    let target_defaults = target_vendor_etc_dir(deploy);
    let staged_etc = deploy.join("etc");
    let etc_plan = cross_family::apply_staged_etc_policy(
        &source_defaults,
        Path::new("/etc"),
        &target_defaults,
        &staged_etc,
        plan,
    )?;

    let remap_plan = cross_family_remap_plan(deploy, &target_defaults);
    print!("{}", remap::render_report(&remap_plan));
    let rechowned_now = remap::apply_remap_plan_to_dirs(&[&staged_etc], &remap_plan)
        .context("failed to apply cross-family remap to staged /etc")?;
    let var_remap_deferred = !remap_plan.steps.is_empty();
    if rechowned_now > 0 {
        println!(
            "[imageswap] cross-family remap: {rechowned_now} file(s) renumbered in staged /etc"
        );
    }
    if var_remap_deferred {
        println!(
            "[imageswap] /var remains live; its cross-family remap runs from the first-boot unit"
        );
    }

    let host_selinux = selinux::read_host_selinux_config();
    let target_selinux = selinux::read_deployment_selinux_config(deploy);
    let relabel_scheduled =
        cross_family::relabel_needed(host_selinux.as_ref(), target_selinux.as_ref());
    if relabel_scheduled {
        println!("[selinux] pulling {target_image} with podman for its setfiles and policy...");
        let pulled = std::process::Command::new("podman")
            .args(["pull", target_image])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !pulled {
            bail!(
                "could not pull {target_image} with podman to label the merged /etc for the \
                 target's SELinux policy. The deployment is finalized but would boot \
                 unlabelled: run `bootc rollback` to keep the current deployment, or fix \
                 podman and run again."
            );
        }
        relabel_staged_composefs_etc(target_image, deploy, target_selinux.as_ref()).context(
            "labelling the cross-family merged /etc failed after finalization; run \
             `bootc rollback` to keep the current deployment",
        )?;
    }

    let firstboot_unit_installed = if let Some(unit) =
        cross_family::render_image_swap_cross_family_firstboot_unit(
            &remap_plan.steps,
            relabel_scheduled,
        ) {
        cross_family::install_firstboot_unit(&staged_etc, &unit)?;
        crate::firstboot_verify::install_verify_probe(&staged_etc)?;
        crate::firstboot_verify::install_cleanup_prompt(&staged_etc)?;
        label_firstboot_files(&staged_etc);
        println!(
            "[imageswap] installed {} (relabel: {relabel_scheduled}, deferred /var remap steps: {})",
            cross_family::FIRSTBOOT_UNIT,
            remap_plan.steps.len()
        );
        true
    } else {
        false
    };

    write_cross_family_report(
        deploy,
        plan,
        etc_plan,
        remap_plan,
        rechowned_now,
        var_remap_deferred,
        relabel_scheduled,
        firstboot_unit_installed,
    )
}

/// Apply the cross-family policy after OSTree finalization has completed the
/// native merge. Re-running the policy before finalization is unsafe: the
/// finalization service can recreate `/etc` from the live tree afterwards.
fn apply_cross_family_ostree_policy(
    staged_root: &Path,
    booted_root: &Path,
    plan: &cross_family::CrossFamilyPlan,
) -> Result<()> {
    let source_defaults = source_vendor_etc_dir(booted_root);
    let target_defaults = target_vendor_etc_dir(staged_root);
    let etc_plan = cross_family::apply_staged_etc_policy(
        &source_defaults,
        Path::new("/etc"),
        &target_defaults,
        &staged_root.join("etc"),
        plan,
    )?;
    let remap_plan = cross_family_remap_plan(staged_root, &target_defaults);
    print!("{}", remap::render_report(&remap_plan));
    let rechowned_now = remap::apply_remap_plan(staged_root, &remap_plan)
        .context("failed to apply cross-family remap to the staged deployment")?;
    if rechowned_now > 0 {
        println!(
            "Cross-family remap applied: {rechowned_now} file(s)/dir(s) rechowned under {}",
            staged_root.display()
        );
    }

    let relabel_scheduled = match selinux::check_and_schedule_autorelabel(staged_root) {
        Ok(true) => {
            println!(
                "SELinux policy type changed for the cross-family target; \
                 scheduled /.autorelabel in the staged deployment."
            );
            true
        }
        Ok(false) => false,
        Err(e) => {
            eprintln!("Warning: failed to check SELinux policy compatibility: {e:#}");
            false
        }
    };
    write_cross_family_report(
        staged_root,
        plan,
        etc_plan,
        remap_plan,
        rechowned_now,
        false,
        relabel_scheduled,
        false,
    )
}

/// The unit whose `ExecStop=` finalizes a composefs staged deployment.
const COMPOSEFS_FINALIZE_UNIT: &str = "bootc-finalize-staged.service";

/// Run bootc's composefs finalization (the `/etc` merge and the boot entry
/// swap) now instead of at shutdown.
///
/// Stopping the unit runs its `ExecStop=` in the same sandbox shutdown
/// would use, and leaves nothing for shutdown to repeat: the composefs
/// counterpart of [`finalize_staged_now`]. The cross-family switch route
/// deliberately writes its policy after this native merge completes.
fn finalize_composefs_staged_now() -> Result<()> {
    let unit = COMPOSEFS_FINALIZE_UNIT;
    let active = std::process::Command::new("systemctl")
        .args(["is-active", "--quiet", unit])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !active {
        // Nothing waits for shutdown: this bootc merged /etc when it staged.
        println!("[finalize] {unit} is not active; the staged /etc is already merged");
        return Ok(());
    }
    ensure_boot_mounted();
    println!("[finalize] merging /etc into the staged deployment now ({unit})...");
    let status = std::process::Command::new("systemctl")
        .args(["stop", unit])
        .status()
        .map_err(|e| anyhow::anyhow!("failed to execute systemctl stop {unit}: {e}"))?;
    let failed = std::process::Command::new("systemctl")
        .args(["is-failed", "--quiet", unit])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !status.success() || failed {
        bail!(
            "finalizing the staged composefs deployment failed; the previous deployment \
             would boot next. See `journalctl -u {unit}`."
        );
    }
    println!("[finalize] staged composefs deployment finalized: it boots next");
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
/// Ordinary and same-family cross-base routes call this last, after every
/// native staged-deployment write. A cross-family switch route calls it
/// before its explicit post-finalization `/etc` reconciliation, which is the
/// only route allowed to write the finalized deployment afterward.
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

/// `bootc switch` suppresses its interactive progress display when its output
/// is piped into the TUI. Report elapsed time until it exits so a long pull
/// does not look stalled; this is activity, not a download percentage.
fn wait_with_stage_progress(
    child: &mut std::process::Child,
    interval: std::time::Duration,
    output: &mut impl std::io::Write,
) -> Result<std::process::ExitStatus> {
    let started = std::time::Instant::now();
    let mut next_report = interval;
    let mut progress_output_available = true;
    loop {
        if let Some(status) = child.try_wait().context("waiting for bootc switch")? {
            return Ok(status);
        }
        let elapsed = started.elapsed();
        if progress_output_available && elapsed >= next_report {
            let write_result = writeln!(
                output,
                "bootc switch is still pulling and staging ({}s elapsed)...",
                elapsed.as_secs()
            )
            .and_then(|()| output.flush());
            if let Err(error) = write_result {
                // A closed TUI log pipe must not abandon a switch that bootc
                // is already staging. Other output failures still surface.
                if error.kind() == std::io::ErrorKind::BrokenPipe {
                    progress_output_available = false;
                } else {
                    return Err(error).context("writing bootc switch progress");
                }
            }
            next_report += interval;
        }
        std::thread::sleep(std::time::Duration::from_secs(1).min(interval));
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
    /// Proceed onto a target from another OS family with the cross-family
    /// `/etc` policy (bootc-migrate#256/#259). The policy is applied after
    /// `bootc switch` finalizes its native merge.
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

        // #256/#259: the early registry scan is a courtesy gate. The
        // authoritative decision is repeated from the staged image's own
        // `/usr/lib/os-release` after `bootc switch`.
        cross_family::gate_for_switch(self.target_image, self.accept_cross_base)?;

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
        let _sysroot =
            SysrootWritable::acquire(SYSROOT).context("cannot write into the staged deployment")?;

        if let Some(plan) = &de_plan {
            de.run_post_switch(plan, false)?;
        }

        let deploy = staged_composefs_deployment()?;
        let cross_family_plan = cross_family::decide(
            scan::read_host_base_info(),
            scan::read_base_info_from_root(&deploy),
            self.accept_cross_base,
        )
        .map_err(|e| {
            e.context(
                "the staged image was rejected as cross-family without --accept-cross-base; \
                 it was left staged and unfinalized. Remove the staged deployment before \
                 retrying rather than rebooting into it",
            )
        })?;

        if let Some(plan) = &cross_family_plan {
            // The native composefs merge runs at finalization. Reconcile only
            // after it has completed, otherwise shutdown finalization replaces
            // the policy with its same-lineage result.
            finalize_composefs_staged_now()?;
            apply_cross_family_image_swap_policy(self.target_image, &deploy, plan)?;
        } else {
            schedule_image_swap_firstboot(self.target_image)?;
        }

        crate::status::write_report(
            std::path::Path::new(crate::status::MIGRATE_REPORT),
            "composefs -> composefs via ImageSwap",
            self.target_image,
        )
        .context("failed to write the migration report")?;

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
/// Same-family cross-base re-bases keep the existing `cross_base` remap and
/// conflict policy. A genuinely cross-family target is reconciled after
/// OSTree finalization, because finalization performs the native merge and
/// would otherwise overwrite a pre-finalization rewrite.
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
        // #256/#259: the early family scan is a courtesy gate. The staged
        // image is checked again authoritatively after `bootc switch`.
        cross_family::gate_for_switch(self.target_image, self.accept_cross_base)?;

        // Cross-base gate (#67 part 1): preserve its remap report and
        // unknown-target refusal for same-family cross-base re-bases.
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
        let _sysroot =
            SysrootWritable::acquire(SYSROOT).context("cannot write into the staged deployment")?;
        let staged_root = cross_base::staged_deployment_root()
            .context("failed to locate the staged deployment for post-processing")?;
        let booted_root = cross_base::booted_deployment_root()
            .context("failed to locate the booted deployment for post-processing")?;

        // The registry-side gate above is advisory for the family decision.
        // Read the staged image's own vendor os-release before choosing the
        // policy, so a registry scan cannot disagree with what will boot.
        let cross_family_plan = cross_family::decide(
            scan::read_host_base_info(),
            scan::read_base_info_from_root(&staged_root),
            self.accept_cross_base,
        )
        .map_err(|e| {
            e.context(
                "the staged image was rejected as cross-family without --accept-cross-base; \
                 it was left staged and unfinalized. Remove it with `ostree admin undeploy \
                 <index>` before retrying rather than rebooting into it",
            )
        })?;

        let finalized = if let Some(plan) = &cross_family_plan {
            // libostree performs its native /etc merge while finalizing. The
            // cross-family policy must see that result and run afterwards.
            finalize_staged_now()?;
            apply_cross_family_ostree_policy(&staged_root, &booted_root, plan)?;
            true
        } else {
            if let Some(plan) = &cross_base_plan {
                cross_base::apply_cross_base_remap(&staged_root, plan)?;
                cross_base::apply_cross_base_etc_policy(&staged_root)?;

                // #67: when the base family changes, the SELinux policy type
                // may differ — schedule /.autorelabel on first boot.
                match selinux::check_and_schedule_autorelabel(&staged_root) {
                    Ok(true) => println!(
                        "SELinux policy type changed for the cross-base target; \
                         scheduled /.autorelabel in the staged deployment."
                    ),
                    Ok(false) => {}
                    Err(e) => {
                        eprintln!("Warning: failed to check SELinux policy compatibility: {e:#}")
                    }
                }
            }
            false
        };

        if let Some(plan) = &de_plan {
            de.run_post_switch(plan, false)?;
        }

        // #262: last for the ordinary and same-family cross-base paths. A
        // cross-family path finalized before its post-finalization rewrite.
        if !finalized {
            finalize_staged_now()?;
        }

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
    fn stage_wait_reports_long_running_child_and_preserves_exit_status() {
        for (script, expected_code, expect_report) in
            [("sleep 0.2; exit 0", 0, true), ("exit 7", 7, false)]
        {
            let mut child = std::process::Command::new("sh")
                .args(["-c", script])
                .spawn()
                .unwrap();
            let mut output = Vec::new();
            let status = wait_with_stage_progress(
                &mut child,
                std::time::Duration::from_millis(50),
                &mut output,
            )
            .unwrap();
            assert_eq!(status.code(), Some(expected_code), "{script}");
            assert_eq!(
                String::from_utf8(output)
                    .unwrap()
                    .contains("still pulling and staging"),
                expect_report,
                "{script}"
            );
        }
    }

    #[test]
    fn closed_progress_pipe_does_not_abandon_the_switch() {
        struct ClosedPipe;
        impl std::io::Write for ClosedPipe {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let mut child = std::process::Command::new("sh")
            .args(["-c", "sleep 0.2; exit 0"])
            .spawn()
            .unwrap();
        let status = wait_with_stage_progress(
            &mut child,
            std::time::Duration::from_millis(50),
            &mut ClosedPipe,
        )
        .unwrap();
        assert!(status.success());
    }

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

    #[test]
    fn composefs_relabel_command_shape() {
        let deploy = Path::new("/sysroot/state/deploy/abc123def456");
        let cases = [
            (
                "ghcr.io/tuna-os/utah:testing",
                "targeted",
                "/etc/selinux/targeted/contexts/files/file_contexts",
            ),
            (
                "ghcr.io/projectbluefin/utah:testing",
                "mls",
                "/etc/selinux/mls/contexts/files/file_contexts",
            ),
            (
                "quay.io/fedora/fedora-bootc:42",
                "minimum",
                "/etc/selinux/minimum/contexts/files/file_contexts",
            ),
        ];

        for (img, policy, expected_fc) in cases {
            let cmd = composefs_relabel_command(img, deploy, policy);
            let joined = cmd.join(" ");
            assert!(
                joined.starts_with("podman run --rm --privileged --security-opt label=disable"),
                "unexpected command prefix for {img}: {joined}"
            );
            assert!(
                joined.contains("--mount type=bind,src=/sysroot,dst=/target"),
                "missing bind mount in {joined}"
            );
            assert!(joined.contains(img), "missing image name in {joined}");
            assert!(
                joined.contains("setfiles -F -r /target/state/deploy/abc123def456"),
                "missing setfiles and alt-root in {joined}"
            );
            assert!(
                joined.contains(expected_fc),
                "expected file_contexts {expected_fc} not found in {joined}"
            );
            assert!(
                joined.ends_with("/target/state/deploy/abc123def456/etc"),
                "missing target etc path in {joined}"
            );
        }
    }
}
