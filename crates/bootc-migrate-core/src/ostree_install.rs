//! Shared alongside-install engine for composefs → ostree
//! (`Strategy::OstreeInstall`, #260) and package → ostree
//! (`Strategy::PackageInstall`, #373).
//!
//! # The problem
//!
//! A composefs-backed host cannot become an ostree-backed one with `bootc
//! switch`: the running bootc only stages composefs deployments, and a
//! target image whose own bootc lacks composefs support could never manage
//! the result. `undo` covers only the narrow case where this tool did the
//! forward migration and the old OSTree deployment is still on disk. The
//! general case — a host installed as composefs from day one — has nothing
//! to restore and needs a fresh OSTree deployment built beside the running
//! system.
//!
//! # The engine
//!
//! The target image's *own* bootc, run in a privileged container against
//! the physical root: `bootc install to-existing-root` ("alongside" mode).
//! It initializes the physical root's `ostree` repository, deploys the image,
//! installs the bootloader through bootupd, and — because it writes the
//! deployment's origin and BLS entry itself — leaves the target's bootc in
//! charge of day-2 updates. Three facts about it shape everything below (read
//! from upstream `install.rs`):
//!
//! 1. The physical root is explicitly bound at `/target` with `rslave`
//!    propagation. It is `/sysroot` for a composefs source and `/` for a
//!    package source; the propagation makes `/boot` and ESP submounts visible.
//! 2. Alongside mode **empties the boot directories**
//!    (`clean_boot_directories`). Composefs-owned ESP artifacts are
//!    snapshotted and restored. Package-host kernels, initramfs, BLS entries
//!    and GRUB configuration are snapshotted for the "Previous system" entry
//!    implemented by #374.
//! 3. It carries nothing else: the new deployment's `/etc` is the factory
//!    copy and its stateroot `/var` is empty. The `/etc` merge (same 3-way
//!    rule as Phase 4, cross-family policy included) and the `/var` copy
//!    are this module's.
//!
//! Planning helpers ([`install_command`], [`package_install_command`],
//! [`composefs_esp_paths`], [`package_boot_snapshot_paths`],
//! [`newest_deployment`]) are pure; the I/O lives in the two public configs'
//! `run` methods.

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::cross_family;
use crate::mergetc::{self, IdentityMergePolicy, MergePolicy};
use crate::migration::rollback;
use crate::migration::{MountGuard, PodmanImageMount, acquire_lock, find_esp_or_mount};
use crate::preflight;
use crate::rebase_controller::validate_target_image;
use crate::registry;
use crate::scan;
use crate::sysroot::{SYSROOT, SysrootWritable};
use crate::xattr;

pub(crate) mod esp_resident;

/// The physical root of a composefs host (the block-device filesystem the
/// composefs store and state live on).
pub const PHYSICAL_ROOT: &str = "/sysroot";
/// The physical root of a package-managed host.
pub const PACKAGE_PHYSICAL_ROOT: &str = "/";
/// Where the target's bootc sees the physical root inside its container —
/// bootc's own `ALONGSIDE_ROOT_MOUNT`.
pub const CONTAINER_TARGET: &str = "/target";
/// Where composefs-source OSTree deployments land.
pub const OSTREE_DEPLOY_DIR: &str = "/sysroot/ostree/deploy/default/deploy";
/// Where package-source OSTree deployments land.
pub const PACKAGE_OSTREE_DEPLOY_DIR: &str = "/ostree/deploy/default/deploy";
/// The composefs-source stateroot's `/var`.
pub const OSTREE_STATEROOT_VAR: &str = "/sysroot/ostree/deploy/default/var";
/// The package-source stateroot's `/var`.
pub const PACKAGE_OSTREE_STATEROOT_VAR: &str = "/ostree/deploy/default/var";
/// Where snapshots and reports are kept across the reboot.
pub const STATE_DIR: &str = "/var/lib/bootc-rebase";
/// The composefs-source report's file name under [`STATE_DIR`].
pub const REPORT_FILE: &str = "ostree-install-report.json";
/// The package-source report's file name under [`STATE_DIR`].
pub const PACKAGE_REPORT_FILE: &str = "package-install-report.json";
/// Numbered steps in the shared alongside-install engine: scan, pull,
/// snapshot, deploy, /etc merge, /var carry, SELinux relabel, bootloader.
const TOTAL_STEPS: u8 = 8;
/// Where this route re-decides an unknown lineage: the /etc merge reads the
/// installed deployment's own os-release ([`merge_etc_into`] refuses a
/// cross-family target there unless accepted). Named in the Unknown warning
/// so the gate text stays true per route.
const OSTREE_INSTALL_DECIDE_NOTE: &str = "The /etc merge below decides from the installed \
     deployment's own os-release; a cross-family target is refused there unless \
     --accept-cross-base was given.";

/// Source-specific paths and policy for the shared alongside-install engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallSource {
    Composefs,
    Package,
}

impl InstallSource {
    fn physical_root(self) -> &'static str {
        match self {
            Self::Composefs => PHYSICAL_ROOT,
            Self::Package => PACKAGE_PHYSICAL_ROOT,
        }
    }

    fn deploy_dir(self) -> &'static str {
        match self {
            Self::Composefs => OSTREE_DEPLOY_DIR,
            Self::Package => PACKAGE_OSTREE_DEPLOY_DIR,
        }
    }

    fn stateroot_var(self) -> &'static str {
        match self {
            Self::Composefs => OSTREE_STATEROOT_VAR,
            Self::Package => PACKAGE_OSTREE_STATEROOT_VAR,
        }
    }

    fn report_file(self) -> &'static str {
        match self {
            Self::Composefs => REPORT_FILE,
            Self::Package => PACKAGE_REPORT_FILE,
        }
    }

    fn route_name(self) -> &'static str {
        match self {
            Self::Composefs => "composefs -> ostree",
            Self::Package => "package -> ostree",
        }
    }
}

/// A numbered step header: `=== Step 2/8 · Pull ===`.
fn step(n: u8, total: u8, title: &str) -> String {
    format!("=== Step {n}/{total} · {title} ===")
}

/// Capability summary value: `yes`/`no`.
fn yes_no(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

/// Refusal when an OSTree deployment is already on disk. Names both ways
/// out: the never-committed forward migration (rollback returns to it) and
/// the leftover of a failed attempt (undeploy or delete, then re-run) —
/// a failed `bootc install` leaves an unregistered directory that `undeploy`
/// cannot see, so both removals are spelled out.
fn existing_deploy_refusal(source: InstallSource, existing: &[String]) -> String {
    let deploy_dir = source.deploy_dir();
    let sysroot = source.physical_root();
    format!(
        "an OSTree deployment already exists under {deploy_dir} ({}). If this \
         host was migrated by bootc-migrate and never committed, `bootc-migrate \
         rollback` returns to it without reinstalling; if it is a leftover from a \
         failed attempt, remove it (`ostree admin --sysroot={sysroot} undeploy \
         <index>`, or delete the directory when `status` does not list it) and \
         re-run. Pass --force to install a fresh deployment alongside it anyway.",
        existing.join(", ")
    )
}

/// Refuse when an OSTree deployment already exists, unless forced. Checked
/// twice per run: early for a fast failure, and again under the migration
/// lock where a concurrent run can no longer slip between check and install.
fn refuse_existing_deploy(source: InstallSource, force: bool) -> Result<()> {
    if Path::new(source.deploy_dir()).is_dir() && !force {
        let existing = list_deployments(source)?;
        if !existing.is_empty() {
            bail!("{}", existing_deploy_refusal(source, &existing));
        }
    }
    Ok(())
}

/// Where the run report is mirrored inside the new stateroot `/var`, so it
/// survives the reboot onto the carried copy.
fn carried_report_path(source: InstallSource) -> PathBuf {
    Path::new(source.stateroot_var())
        .join("lib/bootc-rebase")
        .join(source.report_file())
}

/// True when `fstab` already mounts `mountpoint` (comments ignored).
fn fstab_has_mount(fstab: &str, mountpoint: &str) -> bool {
    fstab.lines().any(|line| {
        let line = line.trim();
        !line.is_empty()
            && !line.starts_with('#')
            && line.split_whitespace().nth(1) == Some(mountpoint)
    })
}

/// The device `mountpoint` is mounted from, from /proc/mounts text.
fn mount_source_for(mounts: &str, mountpoint: &str) -> Option<String> {
    mounts.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let source = fields.next()?;
        (fields.next() == Some(mountpoint)).then(|| source.to_string())
    })
}

/// A filesystem UUID from a mount source, when it names one directly
/// (`/dev/disk/by-uuid/<uuid>`).
fn uuid_from_mount_source(source: &str) -> Option<String> {
    source
        .strip_prefix("/dev/disk/by-uuid/")
        .map(|uuid| uuid.trim().to_string())
        .filter(|uuid| !uuid.is_empty())
}

/// The ESP's filesystem UUID for fstab: resolved from /proc/mounts, falling
/// back to blkid when the mount source is a plain device node.
fn esp_fs_uuid(esp_mountpoint: &str) -> Option<String> {
    let mounts = fs::read_to_string("/proc/mounts").ok()?;
    let source = mount_source_for(&mounts, esp_mountpoint)?;
    if let Some(uuid) = uuid_from_mount_source(&source) {
        return Some(uuid);
    }
    let out = Command::new("blkid")
        .args(["-o", "value", "-s", "UUID", &source])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let uuid = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if uuid.is_empty() { None } else { Some(uuid) }
}

/// The fstab line that mounts the ESP at `/boot/efi`. `nofail`: a missing
/// or renamed ESP must not stop the boot; only bootloader updates need it.
fn esp_fstab_line(uuid: &str) -> String {
    format!("UUID={uuid} /boot/efi vfat umask=0077,shortname=winnt,nofail 0 2\n")
}

/// Guarantee the deployed /etc/fstab mounts the ESP at /boot/efi. bootc's
/// alongside install does not always write one — this host booted with no
/// fstab at all — and without it the ESP never mounts: bootupd cannot stage
/// updates and the hand-copied ESP kernel (#305) has no tooling path.
/// Append-only, skipped when any /boot/efi entry exists.
fn ensure_esp_fstab_entry(deploy_root: &Path, esp_mountpoint: &str) -> Result<()> {
    let fstab = deploy_root.join("etc/fstab");
    let current = fs::read_to_string(&fstab).unwrap_or_default();
    if fstab_has_mount(&current, "/boot/efi") {
        return Ok(());
    }
    let Some(uuid) = esp_fs_uuid(esp_mountpoint) else {
        bail!("could not determine the ESP filesystem UUID for {esp_mountpoint}");
    };
    let mut out = current;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&esp_fstab_line(&uuid));
    fs::write(&fstab, out).with_context(|| format!("writing {}", fstab.display()))?;
    println!(
        "[etc] added ESP mount to the deployed fstab ({})",
        fstab.display()
    );
    Ok(())
}

/// Everything the composefs → ostree strategy needs, translated from CLI
/// flags exactly once by the caller.
#[derive(Debug, Clone, Copy)]
pub struct OstreeInstallConfig<'a> {
    pub target_image: &'a str,
    pub dry_run: bool,
    pub force: bool,
    pub skip_preflight: bool,
    pub accept_cross_base: bool,
}

/// Everything the package → ostree strategy needs. It uses the same
/// alongside-install engine with `/` as the physical root.
#[derive(Debug, Clone, Copy)]
pub struct PackageInstallConfig<'a> {
    pub target_image: &'a str,
    pub dry_run: bool,
    pub force: bool,
    pub skip_preflight: bool,
    pub accept_cross_base: bool,
}

// ---- Pure planning -------------------------------------------------------

/// Build the `podman run … bootc install to-existing-root` invocation for a
/// physical root. `rslave` keeps `/boot` and ESP submounts visible below
/// `/target`.
fn install_command_for_root(
    target_image: &str,
    kargs: &[String],
    physical_root: &str,
) -> Vec<String> {
    let mut argv: Vec<String> = vec![
        "podman".into(),
        "run".into(),
        "--rm".into(),
        "--privileged".into(),
        "--pid=host".into(),
        "--security-opt".into(),
        "label=disable".into(),
        "-v".into(),
        "/dev:/dev".into(),
        "-v".into(),
        "/sys:/sys".into(),
        "-v".into(),
        "/var/tmp:/var/tmp".into(),
        "-v".into(),
        "/var/lib/containers:/var/lib/containers".into(),
        "--mount".into(),
        format!("type=bind,src={physical_root},dst={CONTAINER_TARGET},bind-propagation=rslave"),
        target_image.into(),
        "bootc".into(),
        "install".into(),
        "to-existing-root".into(),
        "--acknowledge-destructive".into(),
        "--skip-fetch-check".into(),
    ];
    for karg in kargs {
        argv.push("--karg".into());
        argv.push(karg.clone());
    }
    argv.push(CONTAINER_TARGET.into());
    argv
}

/// The composefs-source install invocation.
pub fn install_command(target_image: &str, kargs: &[String]) -> Vec<String> {
    install_command_for_root(target_image, kargs, PHYSICAL_ROOT)
}

/// The package-source install invocation. The live `/` is the physical root.
pub fn package_install_command(target_image: &str, kargs: &[String]) -> Vec<String> {
    install_command_for_root(target_image, kargs, PACKAGE_PHYSICAL_ROOT)
}

/// Where [`seed_var_command`] mounts the stateroot `/var` inside the
/// target image's container.
const CONTAINER_VAR_TARGET: &str = "/target-var";

/// The `podman run` argv that fills the stateroot `/var` with the target
/// image's own `/var` skeleton, without overwriting anything the live copy
/// put there (`cp -a --no-clobber`). The stateroot `/var` of an alongside
/// install starts empty, so without this the target boots with the source's
/// `/var` only, and a unit that expects a directory its package created at
/// build time fails (fedora-bootc's chronyd: `/var/lib/chrony: No such file
/// or directory`). Pure so it is table-tested.
fn seed_var_command_for(target_image: &str, stateroot_var: &str) -> Vec<String> {
    vec![
        "podman".into(),
        "run".into(),
        "--rm".into(),
        "--privileged".into(),
        "--security-opt".into(),
        "label=disable".into(),
        "--mount".into(),
        format!("type=bind,src={stateroot_var},dst={CONTAINER_VAR_TARGET}"),
        target_image.into(),
        "cp".into(),
        "-a".into(),
        "--no-clobber".into(),
        "/var/.".into(),
        format!("{CONTAINER_VAR_TARGET}/"),
    ]
}

pub fn seed_var_command(target_image: &str) -> Vec<String> {
    seed_var_command_for(target_image, OSTREE_STATEROOT_VAR)
}

/// Whether an ESP-relative path (forward slashes) belongs to the composefs
/// deployment or its loader — what alongside mode's ESP wipe would destroy
/// and what must come back afterwards. Everything bootupd writes
/// (`EFI/fedora`, `EFI/BOOT`, `EFI/centos`, …) is deliberately excluded:
/// the restore must never overwrite the fresh shim/GRUB with stale copies.
pub fn is_composefs_esp_path(rel: &str) -> bool {
    let rel = rel.trim_start_matches('/');
    rel.starts_with("EFI/Linux/bootc_composefs-")
        || rel.starts_with("EFI/systemd/")
        || rel == "loader/loader.conf"
        || (rel.starts_with("loader/entries/") && rel.ends_with(".conf"))
}

/// Filter an ESP file listing (relative paths) down to the composefs-owned
/// ones — see [`is_composefs_esp_path`]. Sorted, for a stable snapshot
/// manifest.
pub fn composefs_esp_paths<'a>(listing: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut v: Vec<String> = listing
        .into_iter()
        .filter(|p| is_composefs_esp_path(p))
        .map(|p| p.trim_start_matches('/').to_string())
        .collect();
    v.sort();
    v.dedup();
    v
}

/// Filter a `/boot` listing to the package host artifacts needed to construct
/// the "Previous system" entry: kernels, initramfs images, BLS entries and the
/// source GRUB configuration. The snapshot is created before alongside mode
/// empties `/boot`; issue #374 consumes it.
pub fn package_boot_snapshot_paths<'a>(listing: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut paths: Vec<String> = listing
        .into_iter()
        .map(|path| path.trim_start_matches('/'))
        .filter(|path| {
            let top_level = !path.contains('/');
            (top_level
                && (path.starts_with("vmlinuz-")
                    || path.starts_with("initramfs-")
                    || path.starts_with("initrd.img-")))
                || (path.starts_with("loader/entries/") && path.ends_with(".conf"))
                || matches!(*path, "grub/grub.cfg" | "grub2/grub.cfg")
        })
        .map(str::to_string)
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

/// Pick the deployment `bootc install` just created from the names under
/// [`OSTREE_DEPLOY_DIR`]: `<checksum>.<serial>` entries, newest first by
/// modification time is not available to a pure function, so the caller
/// passes `(name, mtime)` pairs and this picks the latest `.0`-suffixed one
/// (a fresh install always deploys serial 0 of its checksum).
pub fn newest_deployment(entries: &[(String, u64)]) -> Option<String> {
    entries
        .iter()
        .filter(|(name, _)| name.ends_with(".0") && name.len() > 2)
        .max_by_key(|(_, mtime)| *mtime)
        .map(|(name, _)| name.clone())
}

/// Kernel arguments to carry from the source host onto the new deployment's
/// BLS entry. Composefs installs inherit the root locator from bootc, while a
/// package install must explicitly preserve it because the new OSTree
/// deployment still boots from the same physical root.
fn filtered_kargs(cmdline: &str, include_root: bool) -> Vec<String> {
    cmdline
        .split_whitespace()
        .filter(|w| {
            (include_root && (w.starts_with("root=") || w.starts_with("rootflags=")))
                || w.starts_with("console=")
                || w.starts_with("rd.lvm.lv=")
                || w.starts_with("rd.luks.")
                || w.starts_with("systemd.log_level=")
        })
        .map(str::to_string)
        .collect()
}

/// Kernel arguments needed beyond those bootc inherits on a composefs host.
pub fn carry_over_kargs(cmdline: &str) -> Vec<String> {
    filtered_kargs(cmdline, false)
}

/// Kernel arguments needed to mount and diagnose a package host's old root.
/// Kernel image paths, initrd paths and backend locators are deliberately
/// excluded.
pub fn package_carry_over_kargs(cmdline: &str) -> Vec<String> {
    filtered_kargs(cmdline, true)
}

/// What the run did, written as JSON under [`STATE_DIR`] so it survives the
/// reboot, and read back by the commit step
/// ([`crate::ostree_install_commit`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OstreeInstallReport {
    pub target_image: String,
    pub deployment: String,
    pub esp: String,
    pub esp_snapshot_dir: String,
    pub esp_paths_restored: Vec<String>,
    pub etc_merge: String,
    pub var_copied: bool,
    pub cross_family: bool,
    pub grub_boot_entry: Option<String>,
    /// `/boot` has no partition of its own and the root is encrypted, so
    /// GRUB cannot read the new deployment's kernels (#305). Defaulted so
    /// reports written before #328 still load in the commit step.
    #[serde(default)]
    pub boot_on_encrypted_root: bool,
    /// The ESP-resident BLS entry written for that layout.
    pub esp_resident_entry: Option<String>,
    /// The systemd-boot firmware entry put first for that layout.
    pub systemd_boot_entry: Option<String>,
}

/// Package-source install report. The boot snapshot is intentionally retained
/// for the rollback entry implemented by issue #374.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageInstallReport {
    pub target_image: String,
    pub deployment: String,
    pub esp: String,
    pub boot_snapshot_dir: String,
    pub boot_snapshot_paths: Vec<String>,
    pub etc_merge: String,
    pub var_copied: bool,
    pub cross_family: bool,
    pub grub_boot_entry: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct InstallConfig<'a> {
    target_image: &'a str,
    dry_run: bool,
    force: bool,
    skip_preflight: bool,
    accept_cross_base: bool,
    source: InstallSource,
}

impl OstreeInstallConfig<'_> {
    pub fn run(&self) -> Result<()> {
        InstallConfig {
            target_image: self.target_image,
            dry_run: self.dry_run,
            force: self.force,
            skip_preflight: self.skip_preflight,
            accept_cross_base: self.accept_cross_base,
            source: InstallSource::Composefs,
        }
        .run()
    }
}

impl PackageInstallConfig<'_> {
    pub fn run(&self) -> Result<()> {
        InstallConfig {
            target_image: self.target_image,
            dry_run: self.dry_run,
            force: self.force,
            skip_preflight: self.skip_preflight,
            accept_cross_base: self.accept_cross_base,
            source: InstallSource::Package,
        }
        .run()
    }
}

// ---- I/O -----------------------------------------------------------------

impl InstallConfig<'_> {
    /// Run the shared alongside-install route.
    fn run(&self) -> Result<()> {
        validate_target_image(self.target_image)?;
        if self.dry_run {
            println!("*** DRY RUN MODE — no changes will be made ***");
        }
        // #303: the deployment-exists check below is check-then-act, so two
        // runs started together would both pass it and race the ESP
        // snapshot, the install and the /var copy. The run lock, held until
        // this returns, makes the second one refuse here instead.
        let _lock = if self.dry_run {
            None
        } else {
            Some(acquire_lock()?)
        };
        println!("Checking system state...");

        let cmdline = fs::read_to_string("/proc/cmdline").unwrap_or_default();
        if self.source == InstallSource::Composefs && !cmdline.contains("composefs=") && !self.force
        {
            bail!(
                "System is not booted from a composefs deployment (/proc/cmdline has no \
                 composefs= parameter); this route converts a composefs host back to the \
                 OSTree backend. Use --force to override."
            );
        }
        if !Path::new("/sys/firmware/efi").exists() {
            bail!(
                "the {} route requires UEFI firmware (NVRAM boot entries).",
                self.source.route_name()
            );
        }
        for tool in ["podman", "efibootmgr"] {
            if which(tool).is_none() {
                bail!(
                    "`{tool}` is required for the {} route and was not found",
                    self.source.route_name()
                );
            }
        }
        refuse_existing_deploy(self.source, self.force)?;

        // One registry scan serves the lineage gate and the capability
        // checks below — previously each scanned the target again, which
        // doubled the longest silent stretch of this route.
        println!("{}", step(1, TOTAL_STEPS, "Scan target image"));
        println!(
            "[scan] streaming probe files for {} from the registry; \
             the first run downloads layers and can take minutes.",
            self.target_image
        );
        let caps = crate::cross_base::scan_target_capabilities_with_retries(
            self.target_image,
            "cross-family identity + capability check",
        );
        if let Some(caps) = &caps {
            let base = caps
                .base
                .as_ref()
                .map(cross_family::describe)
                .unwrap_or_else(|| "no readable os-release identity".to_string());
            println!(
                "[scan] bootc: {}, bootupd: {}, ostree-capable: {}; base: {base}",
                yes_no(caps.bootc_present),
                yes_no(caps.bootupd_present),
                yes_no(caps.ostree_capable),
            );
        }

        if !self.skip_preflight {
            let secure_boot =
                preflight::secure_boot::read_from_efivars(Path::new("/sys/firmware/efi/efivars"));
            let loaders = caps
                .as_ref()
                .map(|caps| caps.target_bootloaders.as_slice())
                .unwrap_or(&[]);
            preflight::secure_boot::enforce_target_bootloaders(
                &secure_boot,
                loaders,
                preflight::secure_boot::TargetBootloaderKind::Shim,
                self.force,
            )?;
        }

        // #256/#258: lineage gate. No /etc policy can be applied to what
        // `bootc install` writes — the merge below is ours, so the policy
        // applies there exactly as in Phase 4.
        cross_family::gate_with_caps(
            self.accept_cross_base,
            caps.as_ref(),
            OSTREE_INSTALL_DECIDE_NOTE,
        )?;

        if !self.skip_preflight {
            match &caps {
                Some(caps) => {
                    if !caps.bootc_present && !self.force {
                        bail!(
                            "target image {} ships no bootc; `bootc install to-existing-root` \
                             runs the target's own bootc. Use --force to try anyway.",
                            self.target_image
                        );
                    }
                    // bootc's ostree backend installs its bootloader through
                    // bootupd and refuses without it ("bootupd is required for
                    // ostree-based installs"); Dakota, GNOME OS-based, ships
                    // none. Refusing here costs a scan; refusing there costs
                    // the pull, the import and an emptied ESP.
                    if !caps.bootupd_present && !self.force {
                        bail!(
                            "target image {} ships no bootupd (usr/bin/bootupctl + \
                             usr/lib/bootupd/updates); bootc's ostree backend cannot install \
                             a bootloader from it, so this route needs a target that ships \
                             bootupd (Bluefin, Fedora or CentOS bootc images do). Use --force \
                             to try anyway.",
                            self.target_image
                        );
                    }
                    if !caps.ostree_capable {
                        eprintln!(
                            "Warning: target image {} has no prepare-root.conf; bootc's ostree \
                             backend will still deploy it, but the image may not be built for \
                             ostree booting.",
                            self.target_image
                        );
                    }
                }
                None => eprintln!(
                    "Warning: could not scan target image {}; proceeding without the \
                     capability check.",
                    self.target_image
                ),
            }
        }

        let esp = find_esp_or_mount().context("locating the EFI system partition")?;
        let booted_image = if self.source == InstallSource::Composefs {
            preflight::run_preflight_checks()
                .ok()
                .and_then(|report| report.booted_image)
        } else {
            None
        };

        // #305 applies only to composefs sources. Package-host preflight
        // refuses encrypted roots until their own E2E coverage lands.
        let boot_on_luks = self.source == InstallSource::Composefs
            && esp_resident::detect(
                &boot_bind_plan(&esp, &live_boot_mounts(&esp)),
                PHYSICAL_ROOT,
                &cmdline,
            );
        if boot_on_luks {
            println!(
                "/boot is on the encrypted root with no boot partition of its own; GRUB cannot \
                 unlock it, so the new deployment will boot through systemd-boot from a copy \
                 of its kernel on the ESP."
            );
            if !Path::new(&esp)
                .join(esp_resident::SYSTEMD_BOOT_EFI)
                .is_file()
                && !self.force
            {
                bail!(
                    "/boot is on the encrypted root and the ESP has no systemd-boot ({}) to \
                     boot the new deployment from; the GRUB stub bootupd writes cannot unlock \
                     the root, so the first boot would stop at a GRUB shell. Install \
                     systemd-boot on the ESP, or pass --force and then {}.",
                    esp_resident::SYSTEMD_BOOT_EFI,
                    esp_resident::manual_workaround()
                );
            }
        }

        if self.dry_run {
            println!("[DRY RUN] Would pull {} with podman.", self.target_image);
            match self.source {
                InstallSource::Composefs => println!(
                    "[DRY RUN] Would snapshot composefs ESP artifacts from {esp} to \
                     {STATE_DIR}/esp-snapshot-*."
                ),
                InstallSource::Package => println!(
                    "[DRY RUN] Would snapshot the running package system's kernels, initramfs, \
                     BLS entries and GRUB configuration from /boot to \
                     {STATE_DIR}/boot-snapshot-*."
                ),
            }
            let kargs = if self.source == InstallSource::Package {
                package_carry_over_kargs(&cmdline)
            } else {
                carry_over_kargs(&cmdline)
            };
            println!(
                "[DRY RUN] Would run: {}",
                install_command_for_root(self.target_image, &kargs, self.source.physical_root())
                    .join(" ")
            );
            println!(
                "[DRY RUN] Would merge /etc into the new deployment, carry /var to {}, and {}.",
                self.source.stateroot_var(),
                if self.source == InstallSource::Package {
                    "put the target GRUB entry first while retaining the old /boot snapshot for the Previous system entry"
                } else if boot_on_luks {
                    "restore the composefs ESP artifacts, copy the new kernel and initrd to the ESP with a BLS entry, make it systemd-boot's default, and put \"Linux Boot Manager\" first in BootOrder"
                } else {
                    "restore the composefs ESP artifacts and put the GRUB entry first in BootOrder"
                }
            );
            return Ok(());
        }

        let sleep_reason = format!("bootc {} re-base in progress", self.source.route_name());
        let _sleep_guard = crate::migration::SleepGuard::new(&sleep_reason);

        // ---- Pull ----
        println!(
            "{}",
            step(2, TOTAL_STEPS, &format!("Pull: {}", self.target_image))
        );
        println!("[pull] streaming `podman pull` progress below…");
        run_checked(
            Command::new("podman").args(["pull", "--policy", "always", self.target_image]),
            "podman pull",
        )?;

        // ---- Source boot snapshot ----
        let snapshot_kind = if self.source == InstallSource::Package {
            "boot"
        } else {
            "esp"
        };
        let snapshot_source = if self.source == InstallSource::Package {
            "/boot"
        } else {
            &esp
        };
        println!(
            "{}",
            step(3, TOTAL_STEPS, &format!("Boot snapshot: {snapshot_source}"))
        );
        fs::create_dir_all(STATE_DIR)?;
        let snapshot_dir = Path::new(STATE_DIR).join(format!(
            "{snapshot_kind}-snapshot-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        ));
        let snapshot_paths = if self.source == InstallSource::Package {
            snapshot_package_boot(Path::new("/boot"), &snapshot_dir)?
        } else {
            snapshot_esp(Path::new(&esp), &snapshot_dir)?
        };
        println!(
            "[{snapshot_kind}] {} artifact(s) snapshotted to {}",
            snapshot_paths.len(),
            snapshot_dir.display()
        );
        if self.source == InstallSource::Composefs
            && !snapshot_paths
                .iter()
                .any(|p| p.starts_with("EFI/Linux/bootc_composefs-"))
        {
            eprintln!(
                "Warning: no composefs kernel directory found on the ESP; the running \
                 deployment will not be restorable as a rollback entry after the install."
            );
        }
        if self.source == InstallSource::Package
            && !snapshot_paths.iter().any(|p| p.starts_with("vmlinuz-"))
        {
            bail!(
                "no package-host kernel was found under /boot; refusing before \
                 `bootc install` empties the boot directory"
            );
        }

        // Everything from here writes under /sysroot (the stateroot /var, the
        // install, the /etc merge), which is read-only by its mount flag on a
        // composefs host. Hold it writable until the run ends; preflight no
        // longer leaves it writable by accident (#419).
        let _sysroot = SysrootWritable::acquire(SYSROOT)
            .context("cannot write the OSTree deployment under /sysroot")?;

        // A previous failed attempt may have removed the stateroot /var while
        // the repo layout survives; `bootc install` reuses extant layouts
        // verbatim and fails opening it. The install expects it empty — the
        // /var copy below fills it — so (re)create it, never wipe it.
        fs::create_dir_all(self.source.stateroot_var()).with_context(|| {
            format!(
                "creating the stateroot /var at {}",
                self.source.stateroot_var()
            )
        })?;

        // ---- Deploy: the target's bootc, alongside ----
        println!(
            "{}",
            step(
                4,
                TOTAL_STEPS,
                &format!(
                    "Deploy: bootc install to-existing-root ({})",
                    self.target_image
                )
            )
        );
        println!("[deploy] streaming `bootc install` output below (OSTree import takes minutes)…");
        let _boot_binds = if self.source == InstallSource::Composefs {
            Some(bind_boot_into_physical_root(&esp)?)
        } else {
            None
        };
        let kargs = if self.source == InstallSource::Package {
            package_carry_over_kargs(&cmdline)
        } else {
            carry_over_kargs(&cmdline)
        };
        let argv = install_command_for_root(self.target_image, &kargs, self.source.physical_root());
        println!("[deploy] {}", argv.join(" "));
        let status = Command::new(&argv[0])
            .args(&argv[1..])
            .status()
            .context("failed to execute podman")?;
        if !status.success() {
            bail!(
                "bootc install to-existing-root failed (exit {status}); the pre-install boot \
                 snapshot is at {}",
                snapshot_dir.display()
            );
        }
        drop(_boot_binds);

        let deployment = newest_deployment(&deployment_entries(self.source)?).ok_or_else(|| {
            anyhow!(
                "no OSTree deployment found under {} after the install",
                self.source.deploy_dir()
            )
        })?;
        let deploy_root = Path::new(self.source.deploy_dir()).join(&deployment);
        println!("[deploy] new deployment: {}", deploy_root.display());

        // ---- /etc ----
        println!(
            "{}",
            step(5, TOTAL_STEPS, "/etc: 3-way merge into the new deployment")
        );
        let (etc_merge, cross) = merge_etc_into(
            &deploy_root,
            booted_image.as_deref(),
            self.accept_cross_base,
        )?;
        println!("[etc] {etc_merge}");
        // Day-2 hygiene, not staging: never fail the run over fstab.
        if let Err(e) = ensure_esp_fstab_entry(&deploy_root, &esp) {
            eprintln!("Warning: leaving deployed fstab without an ESP entry: {e:#}");
        }

        // ---- /var ----
        println!(
            "{}",
            step(
                6,
                TOTAL_STEPS,
                "/var: carrying the live tree into the stateroot"
            )
        );
        let var_copied = copy_var_into_stateroot(self.source.stateroot_var())?;
        if var_copied {
            seed_var_from_target(self.target_image, self.source.stateroot_var());
        }

        if let Some(outcome) = cross.as_ref() {
            let source_passwd = fs::read_to_string("/etc/passwd").unwrap_or_default();
            let source_group = fs::read_to_string("/etc/group").unwrap_or_default();
            cross_family::apply_post_merge(&cross_family::PostMergeInputs {
                etc_outcome: outcome,
                deploy_dir: &deploy_root,
                etc_dir: &deploy_root.join("etc"),
                staged_var: if var_copied {
                    cross_family::StagedVar::Copied(Path::new(self.source.stateroot_var()))
                } else {
                    cross_family::StagedVar::InPlace
                },
                host_selinux: crate::selinux::read_host_selinux_config(),
                source_passwd: &source_passwd,
                source_group: &source_group,
            })?;
        }

        // ---- First-boot verify probe (L1) ----
        // Installed before the relabel step so the target policy labels the
        // probe's own files along with the merged /etc.
        crate::firstboot_verify::install_verify_probe(&deploy_root.join("etc"))
            .context("failed to stage the first-boot verify probe")?;
        println!(
            "[firstboot] verify probe staged; it reports to {} on first boot",
            crate::firstboot_verify::VERIFY_REPORT
        );
        crate::firstboot_verify::install_cleanup_prompt(&deploy_root.join("etc"))
            .context("failed to stage the desktop cleanup prompt")?;
        println!("[firstboot] desktop cleanup prompt staged");
        // ---- First-boot repair (L3, #309) ----
        // Staged before the relabel below, so its files get target labels.
        // The mergetc /etc merge keeps the machine-id verbatim here, as on
        // CoreMigration, so the machine-id class is not armed.
        crate::firstboot_repair::stage_repair(
            &deploy_root.join("etc"),
            var_copied.then(|| Path::new(self.source.stateroot_var())),
            false,
        )?;

        // ---- SELinux labels ----
        // The merge and the /var copy wrote files with the labels the host
        // had, and a composefs host may run without an SELinux policy at
        // all (Dakota does): the fifth E2E run booted the deployment with
        // /etc labelled default_t, and journald, resolved and every user
        // lookup were denied. bootc labelled what it wrote with the
        // target's policy; the same policy, run from the target image,
        // labels what we wrote.
        println!(
            "{}",
            step(
                7,
                TOTAL_STEPS,
                "SELinux: labelling the merged /etc and the carried /var"
            )
        );
        match relabel_with_target_policy(
            self.target_image,
            &deploy_root,
            self.source.physical_root(),
            self.source.stateroot_var(),
            var_copied,
        ) {
            Ok(Some(n)) => println!("[selinux] labelled {n} tree(s) with the target's policy"),
            Ok(None) => println!("[selinux] target does not enable SELinux; nothing to label"),
            Err(e) if self.force => eprintln!(
                "Warning: labelling failed ({e:#}); --force given, the deployment may not boot \
                 enforcing."
            ),
            Err(e) => return Err(e),
        }

        // ---- Boot snapshot restore + NVRAM ----
        println!(
            "{}",
            step(
                8,
                TOTAL_STEPS,
                if self.source == InstallSource::Package {
                    "Bootloader: selecting the target entry"
                } else {
                    "Bootloader: restoring the composefs rollback entry"
                }
            )
        );
        let restored = if self.source == InstallSource::Composefs {
            let restored = restore_esp(&snapshot_dir, Path::new(&esp))?;
            println!("[esp] {} composefs artifact(s) restored", restored.len());
            restored
        } else {
            Vec::new()
        };
        // A composefs host installed to disk boots systemd-boot through the
        // removable-media path and may never have had a firmware entry of
        // its own; bootupd just replaced that path with shim. The restored
        // loader needs an entry to be reachable at all.
        let rollback_entry = if self.source == InstallSource::Composefs
            && Path::new(&esp)
                .join("EFI/systemd/systemd-bootx64.efi")
                .is_file()
        {
            ensure_systemd_boot_entry(self.target_image, &esp)
        } else {
            false
        };
        let mut esp_resident_entry = None;
        let mut systemd_boot_entry = None;
        let grub_entry = if boot_on_luks && rollback_entry {
            println!("=== Bootloader: ESP-resident entry (/boot on the encrypted root) ===");
            match esp_resident::stage(Path::new(PHYSICAL_ROOT), Path::new(&esp), &deployment) {
                Ok(entry) => esp_resident_entry = Some(entry),
                Err(e) => eprintln!(
                    "Warning: could not write the ESP-resident entry ({e:#}). systemd-boot \
                     stays first, so the host boots the composefs deployment again. To boot \
                     the new deployment, {}.",
                    esp_resident::manual_workaround()
                ),
            }
            // GRUB first would stop at a GRUB shell on this layout; the
            // worst case with systemd-boot first is the composefs default.
            // Everything is staged by now; a failed NVRAM write must not
            // abort the run before the report, so it is best-effort like
            // the entry registration above.
            systemd_boot_entry = match put_systemd_boot_first() {
                Ok(id) => id,
                Err(e) => {
                    eprintln!(
                        "Warning: could not put \"Linux Boot Manager\" first in BootOrder \
                         ({e:#}). BootOrder is unchanged; if a GRUB entry comes first, the \
                         first boot may stop at a GRUB shell. Before rebooting, put the \
                         systemd-boot entry first with `efibootmgr --bootorder`, or pick \
                         \"Linux Boot Manager\" in the firmware boot menu."
                    );
                    None
                }
            };
            None
        } else {
            if boot_on_luks {
                eprintln!(
                    "Warning: /boot is on the encrypted root and no firmware entry reaches \
                     systemd-boot, so GRUB stays first and the first boot will likely stop at \
                     a GRUB shell. Before rebooting, {}.",
                    esp_resident::manual_workaround()
                );
            }
            put_grub_first()?
        };

        let rendered = if self.source == InstallSource::Package {
            serde_json::to_string_pretty(&PackageInstallReport {
                target_image: self.target_image.to_string(),
                deployment: deployment.clone(),
                esp: esp.clone(),
                boot_snapshot_dir: snapshot_dir.display().to_string(),
                boot_snapshot_paths: snapshot_paths,
                etc_merge: etc_merge.clone(),
                var_copied,
                cross_family: cross.is_some(),
                grub_boot_entry: grub_entry.clone(),
            })
            .expect("PackageInstallReport serializes")
        } else {
            serde_json::to_string_pretty(&OstreeInstallReport {
                target_image: self.target_image.to_string(),
                deployment: deployment.clone(),
                esp: esp.clone(),
                esp_snapshot_dir: snapshot_dir.display().to_string(),
                esp_paths_restored: restored,
                etc_merge,
                var_copied,
                cross_family: cross.is_some(),
                grub_boot_entry: grub_entry.clone(),
                boot_on_encrypted_root: boot_on_luks,
                esp_resident_entry: esp_resident_entry.clone(),
                systemd_boot_entry,
            })
            .expect("OstreeInstallReport serializes")
        };
        let report_path = Path::new(STATE_DIR).join(self.source.report_file());
        fs::write(&report_path, &rendered)
            .with_context(|| format!("writing {}", report_path.display()))?;
        println!("Report written to {}", report_path.display());
        // The report above lands on THIS /var; post-reboot /var is the
        // carried copy made before it existed. Mirror it into the new
        // stateroot var so post-boot tooling finds it after the reboot.
        let carried = carried_report_path(self.source);
        if let Some(parent) = carried.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&carried, &rendered).with_context(|| format!("writing {}", carried.display()))?;
        println!(
            "Report also staged at {} for post-boot inspection",
            carried.display()
        );
        // A report.json from an earlier composefs run (ImageSwap or
        // CoreMigration) rode along in the /var copy. `bootc-rebase status`
        // prefers it over ours, so it would name the old route after the
        // reboot. Keep it for reference, under a name status does not read.
        let stale = Path::new(self.source.stateroot_var()).join("lib/bootc-migrate/report.json");
        if stale.exists() {
            let kept = stale.with_extension(format!(
                "json.before-{}-install",
                if self.source == InstallSource::Package {
                    "package"
                } else {
                    "ostree"
                }
            ));
            fs::rename(&stale, &kept)
                .with_context(|| format!("moving {} to {}", stale.display(), kept.display()))?;
            println!(
                "Earlier migration report in the carried /var moved to {}",
                kept.display()
            );
        }
        if self.source == InstallSource::Package {
            println!(
                "OSTree deployment staged. Reboot to enter it. The old package system remains \
                 on disk, and its boot artifacts are retained in {} for the Previous system \
                 rollback entry (#374).",
                snapshot_dir.display()
            );
        } else if esp_resident_entry.is_some() {
            println!(
                "OSTree deployment staged. Reboot to enter it through systemd-boot; the \
                 composefs deployment stays selectable in the systemd-boot menu as rollback. \
                 /boot is on the encrypted root, so `bootc upgrade` writes later kernels \
                 where neither GRUB nor this ESP entry reads them: refresh the ESP copy after \
                 each upgrade (#305)."
            );
        } else if rollback_entry {
            println!(
                "OSTree deployment staged. Reboot to enter it; the composefs deployment stays \
                 selectable through the \"Linux Boot Manager\" firmware entry as rollback."
            );
        } else {
            println!(
                "OSTree deployment staged. Reboot to enter it. No firmware entry reaches the \
                 restored composefs loader; its ESP artifacts are in place, so an entry for \
                 \\EFI\\systemd\\systemd-bootx64.efi can be added later with efibootmgr."
            );
        }
        Ok(())
    }
}

/// Whether `efibootmgr -v` lists a "Linux Boot Manager" entry.
fn systemd_boot_entry_present() -> bool {
    Command::new("efibootmgr")
        .arg("-v")
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .any(|l| l.contains("Linux Boot Manager"))
        })
        .unwrap_or(false)
}

/// The `podman run` argv that registers "Linux Boot Manager" with the
/// target image's efibootmgr, the way bootupd registered its own entry
/// moments earlier from the same container. Pure so it is table-tested.
pub fn register_entry_command(target_image: &str, disk: &str, part: &str) -> Vec<String> {
    vec![
        "podman".into(),
        "run".into(),
        "--rm".into(),
        "--privileged".into(),
        "--security-opt".into(),
        "label=disable".into(),
        "-v".into(),
        "/dev:/dev".into(),
        "-v".into(),
        "/sys:/sys".into(),
        target_image.into(),
        "efibootmgr".into(),
        "--create".into(),
        "--disk".into(),
        disk.into(),
        "--part".into(),
        part.into(),
        "--loader".into(),
        "\\EFI\\systemd\\systemd-bootx64.efi".into(),
        "--label".into(),
        "Linux Boot Manager".into(),
    ]
}

/// Make sure a firmware entry reaches the restored systemd-boot loader.
/// The host's own efibootmgr is tried first (Phase 5's helper); on the
/// eighth E2E run Dakota's failed with exit 5 while bootupd's, run from
/// the target container with /dev and /sys, had just succeeded, so that
/// is the fallback. Returns whether the entry exists afterwards.
fn ensure_systemd_boot_entry(target_image: &str, esp: &str) -> bool {
    crate::migration::boot::register_systemd_boot_nvram(esp);
    if systemd_boot_entry_present() {
        return true;
    }
    let Some((disk, part)) = crate::migration::boot::get_esp_disk_and_part(esp) else {
        return false;
    };
    let argv = register_entry_command(target_image, &disk, &part);
    println!("[nvram] {}", argv.join(" "));
    match Command::new(&argv[0]).args(&argv[1..]).status() {
        Ok(s) if s.success() => {}
        Ok(s) => eprintln!(
            "Warning: efibootmgr --create in the target image failed (exit {:?}).",
            s.code()
        ),
        Err(e) => eprintln!("Warning: could not run efibootmgr in the target image: {e}"),
    }
    let present = systemd_boot_entry_present();
    if present {
        println!("[nvram] registered \"Linux Boot Manager\" through the target image's efibootmgr");
    }
    present
}

/// Whether the deployment boots with SELinux enabled, from its own
/// `usr/etc/selinux/config` (the vendor copy; `etc` is the merge's).
fn deployment_enables_selinux(deploy_root: &Path) -> bool {
    let config = fs::read_to_string(deploy_root.join("usr/etc/selinux/config"))
        .or_else(|_| fs::read_to_string(deploy_root.join("etc/selinux/config")));
    match config {
        Ok(c) => {
            let parsed = crate::selinux::parse_selinux_config(&c);
            matches!(
                parsed
                    .selinux
                    .as_deref()
                    .map(str::to_ascii_lowercase)
                    .as_deref(),
                Some("enforcing") | Some("permissive")
            )
        }
        Err(_) => false,
    }
}

/// The `podman run` argv that labels `paths` with the target image's own
/// `setfiles` and policy, each matched against its `alt_root`.
fn relabel_command_for_root(
    target_image: &str,
    physical_root: &str,
    jobs: &[(&Path, &Path)],
) -> Vec<Vec<String>> {
    let to_container = |p: &Path| -> String {
        let rel = p.strip_prefix(physical_root).unwrap_or(p);
        Path::new(CONTAINER_TARGET).join(rel).display().to_string()
    };
    jobs.iter()
        .map(|(alt_root, path)| {
            vec![
                "podman".to_string(),
                "run".into(),
                "--rm".into(),
                "--privileged".into(),
                "--security-opt".into(),
                "label=disable".into(),
                "--mount".into(),
                format!("type=bind,src={physical_root},dst={CONTAINER_TARGET}"),
                target_image.to_string(),
                "setfiles".into(),
                "-F".into(),
                "-r".into(),
                to_container(alt_root),
                "/etc/selinux/targeted/contexts/files/file_contexts".into(),
                to_container(path),
            ]
        })
        .collect()
}

/// Composefs-source relabel command, retained as the public planning helper.
pub fn relabel_command(target_image: &str, jobs: &[(&Path, &Path)]) -> Vec<Vec<String>> {
    relabel_command_for_root(target_image, PHYSICAL_ROOT, jobs)
}

/// Label the merged `/etc` and, when it was copied, the stateroot `/var`
/// with the target's policy. `Ok(None)` when the target does not enable
/// SELinux.
fn relabel_with_target_policy(
    target_image: &str,
    deploy_root: &Path,
    physical_root: &str,
    stateroot_var: &str,
    var_copied: bool,
) -> Result<Option<usize>> {
    if !deployment_enables_selinux(deploy_root) {
        return Ok(None);
    }
    let policy_type = fs::read_to_string(deploy_root.join("usr/etc/selinux/config"))
        .ok()
        .and_then(|c| crate::selinux::parse_selinux_config(&c).selinux_type)
        .unwrap_or_else(|| "targeted".to_string());
    let stateroot = Path::new(stateroot_var)
        .parent()
        .unwrap_or(Path::new(physical_root))
        .to_path_buf();
    let etc = deploy_root.join("etc");
    let var = Path::new(stateroot_var);
    let mut jobs: Vec<(&Path, &Path)> = vec![(deploy_root, etc.as_path())];
    if var_copied {
        jobs.push((stateroot.as_path(), var));
    }
    let mut done = 0;
    for mut argv in relabel_command_for_root(target_image, physical_root, &jobs) {
        if let Some(fc) = argv
            .iter_mut()
            .find(|a| a.as_str() == "/etc/selinux/targeted/contexts/files/file_contexts")
        {
            *fc = format!("/etc/selinux/{policy_type}/contexts/files/file_contexts");
        }
        println!("[selinux] {}", argv.join(" "));
        run_checked(
            Command::new(&argv[0]).args(&argv[1..]),
            "setfiles in the target image",
        )?;
        done += 1;
    }
    Ok(Some(done))
}

fn which(tool: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|p| {
        std::env::split_paths(&p)
            .map(|d| d.join(tool))
            .find(|c| c.is_file())
    })
}

fn run_checked(cmd: &mut Command, what: &str) -> Result<()> {
    let status = cmd
        .status()
        .with_context(|| format!("failed to execute {what}"))?;
    if !status.success() {
        bail!("{what} failed (exit {status})");
    }
    Ok(())
}

fn list_deployments(source: InstallSource) -> Result<Vec<String>> {
    Ok(deployment_entries(source)?
        .into_iter()
        .map(|(name, _)| name)
        .collect())
}

fn deployment_entries(source: InstallSource) -> Result<Vec<(String, u64)>> {
    let dir = Path::new(source.deploy_dir());
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        if !entry.path().is_dir() {
            continue;
        }
        let mtime = entry
            .metadata()?
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        out.push((entry.file_name().to_string_lossy().into_owned(), mtime));
    }
    Ok(out)
}

/// Walk the ESP and copy every composefs-owned path into `snapshot_dir`,
/// returning the relative paths copied.
fn snapshot_esp(esp: &Path, snapshot_dir: &Path) -> Result<Vec<String>> {
    let mut listing = Vec::new();
    walk_files(esp, esp, &mut listing)?;
    let paths = composefs_esp_paths(listing.iter().map(String::as_str));
    for rel in &paths {
        let src = esp.join(rel);
        let dst = snapshot_dir.join(rel);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&src, &dst).with_context(|| format!("snapshotting {}", src.display()))?;
    }
    Ok(paths)
}

/// Snapshot the package host's boot artifacts for the rollback-entry stage.
fn snapshot_package_boot(boot: &Path, snapshot_dir: &Path) -> Result<Vec<String>> {
    let mut listing = Vec::new();
    walk_files(boot, boot, &mut listing)?;
    let paths = package_boot_snapshot_paths(listing.iter().map(String::as_str));
    for rel in &paths {
        let src = boot.join(rel);
        let dst = snapshot_dir.join(rel);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&src, &dst).with_context(|| format!("snapshotting {}", src.display()))?;
    }
    Ok(paths)
}

/// Copy the snapshot back onto the ESP without overwriting anything that
/// exists there now (bootupd's fresh shim/GRUB, or an entry the install
/// wrote). Returns the relative paths written.
fn restore_esp(snapshot_dir: &Path, esp: &Path) -> Result<Vec<String>> {
    let mut listing = Vec::new();
    if snapshot_dir.is_dir() {
        walk_files(snapshot_dir, snapshot_dir, &mut listing)?;
    }
    listing.sort();
    let mut restored = Vec::new();
    for rel in listing {
        let src = snapshot_dir.join(&rel);
        let dst = esp.join(&rel);
        if dst.exists() {
            continue;
        }
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&src, &dst).with_context(|| format!("restoring {}", dst.display()))?;
        restored.push(rel);
    }
    Ok(restored)
}

fn walk_files(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            walk_files(root, &path, out)?;
        } else if path.is_file() {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            out.push(rel);
        }
    }
    Ok(())
}

/// Bind the host's live boot mounts into the physical root so the
/// container, which sees `/sysroot` as `/target`, finds them where
/// alongside mode expects them: a boot directory (or partition) at
/// `/target/boot` and the ESP at `/target/boot/efi`. Unbound on drop.
///
/// A composefs host installed by `bootc install to-disk
/// --composefs-backend` has no boot partition and mounts the ESP at
/// `/boot` itself. Binding that `/boot` to `/target/boot` made bootc mount
/// the same ESP a second time at `/target/boot/efi` (vfat resolves `efi`
/// to the existing `EFI` directory) and fail with EBUSY while emptying
/// it, because the nested `EFI` entry was the mountpoint (#263's first
/// E2E run). For that layout the ESP is bound at `/target/boot/efi` and
/// `/target/boot` stays the root filesystem's own directory, where bootc
/// then puts the OSTree kernels and BLS entries.
struct BootBinds(Vec<PathBuf>);

impl Drop for BootBinds {
    fn drop(&mut self) {
        for p in self.0.iter().rev() {
            let _ = Command::new("umount").arg(p).status();
        }
    }
}

/// The bind mounts alongside mode needs, as `(live source, path under
/// the physical root)`, for a host whose ESP is mounted at `esp`. Pure so
/// the layouts are table-tested.
pub fn boot_bind_plan(esp: &str, live_mounts: &[&str]) -> Vec<(String, String)> {
    let is_mount = |p: &str| live_mounts.contains(&p);
    if esp == "/boot" {
        // ESP-at-/boot, no boot partition: the ESP belongs at boot/efi.
        return vec![("/boot".to_string(), "boot/efi".to_string())];
    }
    let mut plan = Vec::new();
    for (live, rel) in [("/boot", "boot"), ("/boot/efi", "boot/efi")] {
        if is_mount(live) {
            plan.push((live.to_string(), rel.to_string()));
        }
    }
    if esp != "/boot/efi" && is_mount(esp) {
        // ESP mounted somewhere unusual (/efi): still needs to be boot/efi.
        plan.push((esp.to_string(), "boot/efi".to_string()));
    }
    plan
}

/// The host's live boot mountpoints, as [`boot_bind_plan`] takes them.
fn live_boot_mounts(esp: &str) -> Vec<&str> {
    ["/boot", "/boot/efi", "/efi", esp]
        .into_iter()
        .filter(|p| is_mountpoint(Path::new(p)))
        .collect()
}

fn bind_boot_into_physical_root(esp: &str) -> Result<BootBinds> {
    let live_mounts = live_boot_mounts(esp);
    let mut bound = Vec::new();
    for (live, rel) in boot_bind_plan(esp, &live_mounts) {
        let target = Path::new(PHYSICAL_ROOT).join(&rel);
        if is_mountpoint(&target) {
            continue;
        }
        fs::create_dir_all(&target).with_context(|| format!("creating {}", target.display()))?;
        run_checked(
            Command::new("mount").args(["--bind", &live, &target.display().to_string()]),
            "bind-mounting the boot partition into the physical root",
        )?;
        println!("[deploy] bound {live} at {}", target.display());
        bound.push(target);
    }
    Ok(BootBinds(bound))
}

fn is_mountpoint(p: &Path) -> bool {
    Command::new("mountpoint")
        .args(["-q", &p.display().to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The composefs host's factory `/etc` — the "old default" of the 3-way
/// merge — and the guard keeping it mounted or extracted.
struct SourceEtc {
    path: PathBuf,
    via: &'static str,
    _mount: Option<PodmanImageMount>,
    // Declared before `_tmp` so the mount is released before its directory.
    _cfs: Option<MountGuard>,
    _tmp: Option<tempfile::TempDir>,
}

/// The fs-verity digest of the booted composefs image, from the kernel
/// command line's `composefs=` argument. Pure so it is table-tested.
pub fn booted_composefs_verity(cmdline: &str) -> Option<String> {
    cmdline
        .split_whitespace()
        .find_map(|t| t.strip_prefix("composefs="))
        .map(|v| v.trim_start_matches("sha512:").to_string())
        .filter(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Mount the booted composefs image read-only from the host's own store
/// (`bootc internals cfs --system mount <verity> <dir>`): its `/etc` is the
/// factory copy behind the live one, with no registry or podman storage
/// involved. `None` when the host is not booted from composefs or the mount
/// fails (an older bootc without that subcommand, say).
fn mount_booted_composefs_image() -> Option<(tempfile::TempDir, MountGuard)> {
    let cmdline = fs::read_to_string("/proc/cmdline").ok()?;
    let verity = booted_composefs_verity(&cmdline)?;
    let tmp = tempfile::Builder::new()
        .prefix("bootc-rebase-booted-image-")
        .tempdir_in("/var/tmp")
        .ok()?;
    let target = tmp.path().display().to_string();
    let status = Command::new("bootc")
        .args(["internals", "cfs", "--system", "mount", &verity, &target])
        .stderr(std::process::Stdio::null())
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    let guard = MountGuard::new(tmp.path());
    Some((tmp, guard))
}

/// Obtain [`SourceEtc`] from the booted image's podman mount when it is
/// cached locally, else streamed from the registry; `None` (with a
/// warning) when neither works, which makes the merge keep every live path
/// (safe, but no vendor update is applied).
fn source_default_etc(booted_image: Option<&str>) -> Result<Option<SourceEtc>> {
    // The booted image's own factory copy, kept at /usr/etc by every
    // ostree-container build and visible on the composefs root: no
    // registry, no podman storage, and by definition the right "old
    // default" for what is live in /etc.
    let live_factory = Path::new("/usr/etc");
    if live_factory.is_dir() && live_factory.join("passwd").is_file() {
        return Ok(Some(SourceEtc {
            path: live_factory.to_path_buf(),
            via: "booted image (/usr/etc)",
            _mount: None,
            _cfs: None,
            _tmp: None,
        }));
    }
    // An image without /usr/etc (Dakota keeps its factory copy at /etc):
    // the booted composefs image itself, mounted from the host's store.
    if let Some((tmp, guard)) = mount_booted_composefs_image() {
        for rel in ["usr/etc", "etc"] {
            let etc = tmp.path().join(rel);
            if etc.is_dir() && etc.join("passwd").is_file() {
                return Ok(Some(SourceEtc {
                    path: etc,
                    via: "booted image (composefs mount)",
                    _mount: None,
                    _cfs: Some(guard),
                    _tmp: Some(tmp),
                }));
            }
        }
    }
    let Some(image) = booted_image else {
        eprintln!("Warning: booted image unknown; /etc merge keeps every live path.");
        return Ok(None);
    };
    let image = image
        .strip_prefix("docker://")
        .or_else(|| image.strip_prefix("ostree-unverified-image:docker://"))
        .or_else(|| image.strip_prefix("ostree-unverified-registry:"))
        .unwrap_or(image);
    if let Ok(mount) = PodmanImageMount::new(image) {
        let etc = mount.path.join("etc");
        if etc.is_dir() {
            return Ok(Some(SourceEtc {
                path: etc,
                via: "booted image (podman mount)",
                _mount: Some(mount),
                _cfs: None,
                _tmp: None,
            }));
        }
    }
    let tmp = tempfile::Builder::new()
        .prefix("bootc-rebase-source-etc-")
        .tempdir_in("/var/tmp")?;
    match registry::extract_subtree_via_registry(image, "etc/", tmp.path()) {
        Ok(()) => Ok(Some(SourceEtc {
            path: tmp.path().to_path_buf(),
            via: "booted image (registry stream)",
            _mount: None,
            _cfs: None,
            _tmp: Some(tmp),
        })),
        Err(e) => {
            eprintln!(
                "Warning: could not obtain the booted image's factory /etc ({e:#}); the merge \
                 keeps every live path."
            );
            Ok(None)
        }
    }
}

/// Remove everything under `dir`, keeping `dir` itself.
fn clear_dir_contents(dir: &Path) -> Result<()> {
    if !dir.exists() {
        fs::create_dir_all(dir)?;
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.symlink_metadata()?.file_type().is_dir() {
            fs::remove_dir_all(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/// Merge `/etc` into the new deployment: old default = the booted
/// composefs image's `/etc`, current = live `/etc`, new default = the
/// deployment's `/usr/etc`. Returns a summary line and the cross-family
/// outcome when the policy applied.
fn merge_etc_into(
    deploy_root: &Path,
    booted_image: Option<&str>,
    accept_cross_base: bool,
) -> Result<(String, Option<cross_family::CrossFamilyEtcOutcome>)> {
    let new_default = deploy_root.join("usr/etc");
    let out = deploy_root.join("etc");
    let current = Path::new("/etc");
    if !new_default.is_dir() {
        bail!("new deployment has no usr/etc at {}", new_default.display());
    }

    let plan = cross_family::decide(
        scan::read_host_base_info(),
        scan::read_base_info_from_root(deploy_root),
        accept_cross_base,
    )?;

    let source = source_default_etc(booted_image)?;
    let empty = tempfile::tempdir()?;
    let old_default: &Path = source
        .as_ref()
        .map(|s| s.path.as_path())
        .unwrap_or(empty.path());

    let mut outcome = None;
    let mut manifest = None;
    if let Some(plan) = &plan {
        let states = mergetc::etc_path_states(old_default, current, &new_default)?;
        let etc_plan = cross_family::plan_etc(&states);
        manifest = Some(etc_plan.overrides());
        let read = |n: &str| {
            let etc = fs::read_to_string(new_default.join(n)).unwrap_or_default();
            let usr = fs::read_to_string(deploy_root.join("usr/lib").join(n)).unwrap_or_default();
            if usr.is_empty() {
                etc
            } else if etc.is_empty() {
                usr
            } else {
                mergetc::union_by_first_field(&etc, &usr)
            }
        };
        outcome = Some(cross_family::CrossFamilyEtcOutcome {
            plan: plan.clone(),
            etc_plan,
            target_passwd: read("passwd"),
            target_group: read("group"),
            target_selinux: fs::read_to_string(new_default.join("selinux/config"))
                .ok()
                .map(|c| crate::selinux::parse_selinux_config(&c)),
        });
    }

    // Start from a clean tree: bootc left the factory copy here, and the
    // merge writes the whole result. The directory itself stays: ostree
    // marks the deployment root immutable, so unlinking `etc` from it is
    // EPERM (the fourth E2E run); only its contents are ours.
    clear_dir_contents(&out).with_context(|| format!("clearing {}", out.display()))?;
    mergetc::merge_etc_files_with_policy(
        old_default,
        current,
        &new_default,
        &out,
        &MergePolicy {
            overrides: manifest.as_ref(),
            identity: if plan.is_some() {
                IdentityMergePolicy::TargetFirst
            } else {
                IdentityMergePolicy::SourceFirst
            },
        },
    )
    .context("3-way /etc merge into the new deployment failed")?;

    let supplemented = supplement_identity_from_usr_lib(deploy_root, &out)?;
    if supplemented > 0 {
        println!(
            "[etc] supplemented {supplemented} identity DB(s) with target's /usr/lib accounts"
        );
    }

    match mergetc::prune_dangling_symlinks(&out, deploy_root) {
        Ok(n) if n > 0 => println!("[etc] pruned {n} dangling symlink(s)"),
        Ok(_) => {}
        Err(e) => eprintln!("[etc] warning: dangling-symlink prune failed: {e:#}"),
    }
    // A composefs-era artifact that lies about state on an ostree deployment.
    let _ = fs::remove_file(out.join("bootc-migrate/cross-family-firstboot"));

    let summary = format!(
        "merged (old default: {}, policy: {})",
        source
            .as_ref()
            .map(|s| s.via)
            .unwrap_or("none — live /etc kept"),
        if plan.is_some() {
            "cross-family"
        } else {
            "same-lineage"
        }
    );
    Ok((summary, outcome))
}

/// Supplement the merged `/etc` identity databases (passwd, group, shadow, gshadow, subuid, subgid)
/// with the target image's vendor accounts from `<deploy_root>/usr/lib/`.
///
/// Images using `nss-altfiles` or vendor-split user databases (such as Fedora bootc, CentOS,
/// AlmaLinux, RHEL) store system accounts (`chrony`, `rpc`, `rpcuser`, `sssd`, `mail`, etc.) in
/// `/usr/lib/passwd` and `/usr/lib/group` rather than `/usr/etc/passwd` and `/usr/etc/group`.
/// When migrating from a host that does not define those accounts (e.g. Dakota on composefs),
/// the 3-way /etc merge keeps the source's accounts and takes only `/usr/etc` from the target,
/// leaving the target's system accounts missing from `/etc/passwd` and `/etc/group`.
///
/// Early in boot, `systemd-tmpfiles-setup.service` runs during `sysinit.target` before `systemd-userdbd`
/// is active. If these accounts are not in `/etc/passwd` / `/etc/group`, `systemd-tmpfiles-setup` fails
/// with "Unknown user 'chrony'" (exit 65), causing dependent services like `chronyd` and `gssproxy`
/// to fail.
///
/// Appending the target's vendor accounts to the merged `/etc/passwd` and `/etc/group` (preserving the
/// source's accounts first) ensures they resolve via standard NSS `files` from the earliest boot stage.
pub fn supplement_identity_from_usr_lib(deploy_root: &Path, out_etc: &Path) -> Result<usize> {
    let usr_lib = deploy_root.join("usr/lib");
    if !usr_lib.is_dir() {
        return Ok(0);
    }
    let names = [
        "passwd", "passwd-", "group", "group-", "shadow", "shadow-", "gshadow", "gshadow-",
        "subuid", "subuid-", "subgid", "subgid-",
    ];
    let mut supplemented = 0;
    for name in &names {
        let usr_file = usr_lib.join(name);
        if !usr_file.is_file() {
            continue;
        }
        let target_content = fs::read_to_string(&usr_file).unwrap_or_default();
        if target_content.trim().is_empty() {
            continue;
        }
        let etc_file = out_etc.join(name);
        let cur_content = fs::read_to_string(&etc_file).unwrap_or_default();
        let merged = mergetc::union_by_first_field(&cur_content, &target_content);
        if merged != cur_content {
            let perms = fs::metadata(&etc_file).ok().map(|m| m.permissions());
            fs::write(&etc_file, merged.as_bytes())
                .with_context(|| format!("failed to rewrite {}", etc_file.display()))?;
            if let Some(p) = perms {
                let _ = fs::set_permissions(&etc_file, p);
            }
            supplemented += 1;
        }
    }
    Ok(supplemented)
}

/// Copy the live `/var` into `stateroot_var` unless `/var` is a dedicated
/// filesystem the deployment's fstab will mount itself.
fn copy_var_into_stateroot(stateroot_var: &str) -> Result<bool> {
    if let Some(var) = crate::migration::var_layout::detect_separate_var()? {
        println!(
            "[var] /var is a dedicated filesystem (UUID={}); the merged /etc/fstab mounts it, \
             nothing to copy",
            var.uuid
        );
        return Ok(false);
    }
    let dst = Path::new(stateroot_var);
    fs::create_dir_all(dst)?;
    // A live /var is gigabytes; heartbeat every few seconds so the copy
    // reads as progress, podman-pull style, instead of a hang.
    let started = std::time::Instant::now();
    let mut last_tick = started;
    let mut tick = |counts: xattr::CopyCounts| {
        if last_tick.elapsed() >= std::time::Duration::from_secs(5) {
            last_tick = std::time::Instant::now();
            println!(
                "[var] …{} files ({} MB) in {}s, still copying…",
                counts.files,
                counts.bytes / (1024 * 1024),
                started.elapsed().as_secs(),
            );
        }
    };
    let counts = xattr::copy_dir_all_with_xattrs_progress("/var", dst, &mut tick)
        .context("copying /var into the OSTree stateroot")?;
    let skipped = if counts.specials_skipped > 0 {
        format!(", {} special files skipped", counts.specials_skipped)
    } else {
        String::new()
    };
    println!(
        "[var] live /var copied to {} ({} files, {} MB, {}s{skipped})",
        dst.display(),
        counts.files,
        counts.bytes / (1024 * 1024),
        started.elapsed().as_secs(),
    );
    Ok(true)
}

/// Fill the stateroot `/var` with the target's own `/var` skeleton (see
/// [`seed_var_command`]). Best-effort: a path the source has as a different
/// file type (a directory where the target ships a symlink) makes `cp`
/// report an error for that path while still copying the rest, and the
/// carried data must win there anyway.
fn seed_var_from_target(target_image: &str, stateroot_var: &str) {
    let argv = seed_var_command_for(target_image, stateroot_var);
    println!("[var] {}", argv.join(" "));
    match Command::new(&argv[0]).args(&argv[1..]).status() {
        Ok(s) if s.success() => println!("[var] target /var skeleton added"),
        Ok(s) => eprintln!(
            "Warning: filling /var from the target image reported errors (exit {:?}); \
             paths the source already had are kept as carried.",
            s.code()
        ),
        Err(e) => eprintln!("Warning: could not run the target image to fill /var: {e}"),
    }
}

/// Put the shim/GRUB entry bootupd registered first in `BootOrder`, keeping
/// every other entry (the composefs "Linux Boot Manager" included) behind
/// it. Returns the entry id, or `None` with a warning when no such entry
/// exists — the firmware's removable-media fallback (`EFI/BOOT/BOOTX64.EFI`,
/// which bootupd also writes) then boots GRUB.
fn put_grub_first() -> Result<Option<String>> {
    let out = Command::new("efibootmgr")
        .arg("-v")
        .output()
        .context("running efibootmgr")?;
    if !out.status.success() {
        bail!(
            "efibootmgr failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let txt = String::from_utf8_lossy(&out.stdout);
    let Some(id) = rollback::parse_ostree_boot_entry_id(&txt) else {
        eprintln!(
            "Warning: no shim/GRUB firmware entry found; the new deployment boots through the \
             removable-media fallback unless the firmware prefers systemd-boot's entry."
        );
        return Ok(None);
    };
    let order = rollback::parse_boot_order(&txt).unwrap_or_default();
    let new_order = rollback::build_new_boot_order(&order, &id);
    run_checked(
        Command::new("efibootmgr").args(["--bootorder", &new_order]),
        "efibootmgr --bootorder",
    )?;
    println!("[nvram] BootOrder: {new_order} (GRUB entry Boot{id} first)");
    Ok(Some(id))
}

/// Put systemd-boot's firmware entry ("Linux Boot Manager") first in
/// `BootOrder`, for the layout where GRUB cannot boot the deployment
/// (#305). Returns the entry id, or `None` with a warning when there is
/// no such entry.
fn put_systemd_boot_first() -> Result<Option<String>> {
    let out = Command::new("efibootmgr")
        .arg("-v")
        .output()
        .context("running efibootmgr")?;
    if !out.status.success() {
        bail!(
            "efibootmgr failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let txt = String::from_utf8_lossy(&out.stdout);
    let Some(id) = esp_resident::parse_systemd_boot_entry_id(&txt) else {
        eprintln!("Warning: no \"Linux Boot Manager\" firmware entry found; BootOrder unchanged.");
        return Ok(None);
    };
    let order = rollback::parse_boot_order(&txt).unwrap_or_default();
    let new_order = rollback::build_new_boot_order(&order, &id);
    run_checked(
        Command::new("efibootmgr").args(["--bootorder", &new_order]),
        "efibootmgr --bootorder",
    )?;
    println!("[nvram] BootOrder: {new_order} (systemd-boot entry Boot{id} first)");
    Ok(Some(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_var_command_never_overwrites_carried_data() {
        let argv = seed_var_command("quay.io/fedora/fedora-bootc:44");
        assert_eq!(argv[0], "podman");
        assert!(argv.contains(&format!(
            "type=bind,src={OSTREE_STATEROOT_VAR},dst={CONTAINER_VAR_TARGET}"
        )));
        let image_pos = argv
            .iter()
            .position(|a| a == "quay.io/fedora/fedora-bootc:44")
            .unwrap();
        // The copy runs in the target image, from its /var into the
        // stateroot, and must not replace what the live copy carried.
        assert_eq!(
            &argv[image_pos + 1..],
            &["cp", "-a", "--no-clobber", "/var/.", "/target-var/"]
        );
    }

    #[test]
    fn install_command_shape() {
        let argv = install_command(
            "ghcr.io/projectbluefin/bluefin:stable",
            &["console=ttyS0,115200n8".to_string()],
        );
        assert_eq!(argv[0], "podman");
        assert!(argv.contains(&"--privileged".to_string()));
        assert!(argv.contains(&"--pid=host".to_string()));
        assert!(argv.contains(&"/var/lib/containers:/var/lib/containers".to_string()));
        // The physical root, not PID 1's overlay, is what the target's bootc
        // must see at /target — and its submounts must propagate.
        assert!(argv.contains(&format!(
            "type=bind,src={PHYSICAL_ROOT},dst={CONTAINER_TARGET},bind-propagation=rslave"
        )));
        let image_pos = argv
            .iter()
            .position(|a| a == "ghcr.io/projectbluefin/bluefin:stable")
            .unwrap();
        assert_eq!(
            &argv[image_pos + 1..],
            &[
                "bootc",
                "install",
                "to-existing-root",
                "--acknowledge-destructive",
                "--skip-fetch-check",
                "--karg",
                "console=ttyS0,115200n8",
                CONTAINER_TARGET
            ]
        );
        // No kargs: no --karg at all, and the root path stays last.
        let bare = install_command("img", &[]);
        assert!(!bare.contains(&"--karg".to_string()));
        assert_eq!(bare.last().map(String::as_str), Some(CONTAINER_TARGET));
        // Never the composefs backend: the whole point is the ostree one.
        assert!(!argv.iter().any(|a| a.contains("composefs-backend")));
    }

    #[test]
    fn package_install_command_binds_the_live_root_with_submounts() {
        let argv = package_install_command(
            "quay.io/fedora/fedora-bootc:44",
            &[
                "root=UUID=abc".to_string(),
                "rootflags=subvol=root".to_string(),
            ],
        );
        assert!(argv.contains(&format!(
            "type=bind,src=/,dst={CONTAINER_TARGET},bind-propagation=rslave"
        )));
        assert!(!argv.iter().any(|arg| arg.contains("src=/sysroot")));
        let image = argv
            .iter()
            .position(|arg| arg == "quay.io/fedora/fedora-bootc:44")
            .unwrap();
        assert_eq!(
            &argv[image + 1..],
            &[
                "bootc",
                "install",
                "to-existing-root",
                "--acknowledge-destructive",
                "--skip-fetch-check",
                "--karg",
                "root=UUID=abc",
                "--karg",
                "rootflags=subvol=root",
                CONTAINER_TARGET,
            ]
        );
    }

    /// The snapshot is exactly what alongside mode's ESP wipe would take
    /// from the composefs deployment, and never what bootupd writes.
    /// The bind layout alongside mode needs, per host boot layout.
    #[test]
    fn boot_bind_plan_table() {
        let pairs = |v: Vec<(String, String)>| -> Vec<(String, String)> { v };
        // composefs-native host: ESP at /boot, no boot partition. The ESP
        // goes to boot/efi and /target/boot stays the root fs's directory.
        assert_eq!(
            pairs(boot_bind_plan("/boot", &["/boot"])),
            vec![("/boot".to_string(), "boot/efi".to_string())]
        );
        // Classic layout: boot partition at /boot, ESP at /boot/efi.
        assert_eq!(
            pairs(boot_bind_plan("/boot/efi", &["/boot", "/boot/efi"])),
            vec![
                ("/boot".to_string(), "boot".to_string()),
                ("/boot/efi".to_string(), "boot/efi".to_string())
            ]
        );
        // Boot partition, ESP mounted at /efi.
        assert_eq!(
            pairs(boot_bind_plan("/efi", &["/boot", "/efi"])),
            vec![
                ("/boot".to_string(), "boot".to_string()),
                ("/efi".to_string(), "boot/efi".to_string())
            ]
        );
        // /boot on the root fs, ESP mounted by bootc-migrate at /boot/efi.
        assert_eq!(
            pairs(boot_bind_plan("/boot/efi", &["/boot/efi"])),
            vec![("/boot/efi".to_string(), "boot/efi".to_string())]
        );
    }

    #[test]
    fn register_entry_command_shape() {
        let argv = register_entry_command("quay.io/fedora/fedora-bootc:44", "/dev/vda", "2");
        let joined = argv.join(" ");
        assert!(joined.starts_with("podman run --rm --privileged"));
        assert!(joined.contains("-v /dev:/dev -v /sys:/sys quay.io/fedora/fedora-bootc:44 efibootmgr --create --disk /dev/vda --part 2 --loader"));
        assert_eq!(argv[argv.len() - 1], "Linux Boot Manager");
        assert_eq!(argv[argv.len() - 3], "\\EFI\\systemd\\systemd-bootx64.efi");
    }

    #[test]
    fn relabel_command_shape() {
        let deploy = Path::new("/sysroot/ostree/deploy/default/deploy/abc.0");
        let etc = deploy.join("etc");
        let stateroot = Path::new("/sysroot/ostree/deploy/default");
        let var = Path::new(OSTREE_STATEROOT_VAR);
        let cmds = relabel_command(
            "quay.io/fedora/fedora-bootc:44",
            &[(deploy, etc.as_path()), (stateroot, var)],
        );
        assert_eq!(cmds.len(), 2);
        let etc_cmd = cmds[0].join(" ");
        assert!(etc_cmd.contains("setfiles -F -r /target/ostree/deploy/default/deploy/abc.0 "));
        assert!(etc_cmd.ends_with("/target/ostree/deploy/default/deploy/abc.0/etc"));
        assert!(etc_cmd.contains("--privileged"));
        let var_cmd = cmds[1].join(" ");
        assert!(var_cmd.contains("-r /target/ostree/deploy/default "));
        assert!(var_cmd.ends_with("/target/ostree/deploy/default/var"));
    }

    #[test]
    fn booted_composefs_verity_table() {
        assert_eq!(
            booted_composefs_verity("initrd=x rw composefs=abc123 console=ttyS0"),
            Some("abc123".to_string())
        );
        assert_eq!(
            booted_composefs_verity("composefs=sha512:0f1e"),
            Some("0f1e".to_string())
        );
        assert_eq!(
            booted_composefs_verity("root=UUID=x ostree=/ostree/boot.1/x/0"),
            None
        );
        assert_eq!(booted_composefs_verity("composefs="), None);
        assert_eq!(booted_composefs_verity("composefs=not-hex!"), None);
    }

    #[test]
    fn clear_dir_contents_keeps_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let etc = dir.path().join("etc");
        fs::create_dir_all(etc.join("sub/deeper")).unwrap();
        fs::write(etc.join("a"), "x").unwrap();
        fs::write(etc.join("sub/deeper/b"), "y").unwrap();
        std::os::unix::fs::symlink("a", etc.join("link")).unwrap();
        clear_dir_contents(&etc).unwrap();
        assert!(etc.is_dir());
        assert_eq!(fs::read_dir(&etc).unwrap().count(), 0);
        // Absent: created.
        let fresh = dir.path().join("fresh");
        clear_dir_contents(&fresh).unwrap();
        assert!(fresh.is_dir());
    }

    #[test]
    fn esp_path_classification_table() {
        let cases = [
            ("EFI/Linux/bootc_composefs-abc123/vmlinuz", true),
            ("EFI/Linux/bootc_composefs-abc123/initrd", true),
            ("/EFI/Linux/bootc_composefs-abc123/initrd", true),
            ("EFI/systemd/systemd-bootx64.efi", true),
            ("loader/loader.conf", true),
            ("loader/entries/bootc_composefs-abc123.conf", true),
            ("loader/entries/ostree-1.conf", true),
            ("loader/random-seed", false),
            ("EFI/BOOT/BOOTX64.EFI", false),
            ("EFI/fedora/shimx64.efi", false),
            ("EFI/fedora/grub.cfg", false),
            ("EFI/centos/grubx64.efi", false),
            ("EFI/Linux/other.efi", false),
        ];
        for (p, want) in cases {
            assert_eq!(is_composefs_esp_path(p), want, "{p}");
        }
        let got = composefs_esp_paths(cases.iter().map(|(p, _)| *p));
        assert_eq!(
            got,
            vec![
                "EFI/Linux/bootc_composefs-abc123/initrd",
                "EFI/Linux/bootc_composefs-abc123/vmlinuz",
                "EFI/systemd/systemd-bootx64.efi",
                "loader/entries/bootc_composefs-abc123.conf",
                "loader/entries/ostree-1.conf",
                "loader/loader.conf",
            ]
        );
    }

    #[test]
    fn package_boot_snapshot_path_table() {
        let listing = [
            "vmlinuz-6.12.0",
            "/initramfs-6.12.0.img",
            "initrd.img-6.11.0",
            "loader/entries/fedora-6.12.0.conf",
            "grub2/grub.cfg",
            "grub/grub.cfg",
            "config-6.12.0",
            "System.map-6.12.0",
            "efi/EFI/fedora/shimx64.efi",
            "loader/random-seed",
        ];
        assert_eq!(
            package_boot_snapshot_paths(listing),
            vec![
                "grub/grub.cfg",
                "grub2/grub.cfg",
                "initramfs-6.12.0.img",
                "initrd.img-6.11.0",
                "loader/entries/fedora-6.12.0.conf",
                "vmlinuz-6.12.0",
            ]
        );
    }

    #[test]
    fn newest_deployment_picks_latest_serial_zero() {
        let entries = vec![
            ("aaaa.0".to_string(), 10),
            ("bbbb.0".to_string(), 30),
            ("bbbb.1".to_string(), 40), // a serial-1 rollback clone is never a fresh install
            ("cccc.0".to_string(), 20),
        ];
        assert_eq!(newest_deployment(&entries).as_deref(), Some("bbbb.0"));
        assert_eq!(newest_deployment(&[]), None);
        assert_eq!(newest_deployment(&[("x.1".to_string(), 5)]), None);
    }

    #[test]
    fn carry_over_kargs_table() {
        let cmdline = "BOOT_IMAGE=/EFI/Linux/x/vmlinuz root=UUID=abc rootflags=subvol=root rw \
                       composefs=deadbeef console=ttyS0,115200n8 console=tty0 rd.lvm.lv=vg/root \
                       rd.luks.name=1234=root quiet systemd.log_level=info initrd=/x/initrd";
        assert_eq!(
            carry_over_kargs(cmdline),
            vec![
                "console=ttyS0,115200n8",
                "console=tty0",
                "rd.lvm.lv=vg/root",
                "rd.luks.name=1234=root",
                "systemd.log_level=info",
            ]
        );
        assert_eq!(
            package_carry_over_kargs(cmdline),
            vec![
                "root=UUID=abc",
                "rootflags=subvol=root",
                "console=ttyS0,115200n8",
                "console=tty0",
                "rd.lvm.lv=vg/root",
                "rd.luks.name=1234=root",
                "systemd.log_level=info",
            ]
        );
        for dropped in ["BOOT_IMAGE=", "composefs=", "ostree=", "initrd="] {
            assert!(
                package_carry_over_kargs(cmdline)
                    .iter()
                    .all(|arg| !arg.starts_with(dropped)),
                "{dropped}"
            );
        }
    }

    /// Snapshot then restore over a "wiped" ESP that bootupd has partially
    /// repopulated: composefs artifacts come back, bootupd's files are never
    /// overwritten, and an unrelated file is not touched.
    #[test]
    fn esp_snapshot_restore_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let esp = tmp.path().join("esp");
        let w = |p: &str, c: &str| {
            let full = esp.join(p);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(full, c).unwrap();
        };
        w("EFI/Linux/bootc_composefs-abc/vmlinuz", "kernel");
        w("EFI/Linux/bootc_composefs-abc/initrd", "initrd");
        w("EFI/systemd/systemd-bootx64.efi", "sd-boot");
        w(
            "loader/entries/bootc_composefs-abc.conf",
            "options composefs=abc",
        );
        w("loader/loader.conf", "timeout 5");
        w("EFI/BOOT/BOOTX64.EFI", "old-sd-boot-copy");

        let snap = tmp.path().join("snap");
        let taken = snapshot_esp(&esp, &snap).unwrap();
        assert_eq!(taken.len(), 5);
        assert!(!taken.iter().any(|p| p.starts_with("EFI/BOOT")));

        // bootupd empties the ESP and writes shim/GRUB + its own fallback.
        fs::remove_dir_all(&esp).unwrap();
        w("EFI/fedora/shimx64.efi", "shim");
        w("EFI/fedora/grub.cfg", "grub");
        w("EFI/BOOT/BOOTX64.EFI", "shim-fallback");
        w("loader/loader.conf", "written-by-something-else");

        let restored = restore_esp(&snap, &esp).unwrap();
        assert_eq!(
            restored,
            vec![
                "EFI/Linux/bootc_composefs-abc/initrd",
                "EFI/Linux/bootc_composefs-abc/vmlinuz",
                "EFI/systemd/systemd-bootx64.efi",
                "loader/entries/bootc_composefs-abc.conf",
            ]
        );
        let r = |p: &str| fs::read_to_string(esp.join(p)).unwrap();
        assert_eq!(r("EFI/Linux/bootc_composefs-abc/vmlinuz"), "kernel");
        assert_eq!(
            r("loader/entries/bootc_composefs-abc.conf"),
            "options composefs=abc"
        );
        // Existing files are never overwritten.
        assert_eq!(r("EFI/BOOT/BOOTX64.EFI"), "shim-fallback");
        assert_eq!(r("loader/loader.conf"), "written-by-something-else");
        assert_eq!(r("EFI/fedora/shimx64.efi"), "shim");
    }

    #[test]
    fn step_header_numbers_the_run() {
        assert_eq!(
            step(1, 8, "Scan target image"),
            "=== Step 1/8 · Scan target image ==="
        );
        assert_eq!(
            step(2, 8, "Pull: ghcr.io/x/y:z"),
            "=== Step 2/8 · Pull: ghcr.io/x/y:z ==="
        );
        assert_eq!(
            step(8, 8, "Bootloader: restoring the composefs rollback entry"),
            "=== Step 8/8 · Bootloader: restoring the composefs rollback entry ==="
        );
    }

    #[test]
    fn yes_no_renders_capabilities() {
        assert_eq!(yes_no(true), "yes");
        assert_eq!(yes_no(false), "no");
    }

    #[test]
    fn existing_deploy_refusal_names_source_paths_and_both_ways_out() {
        for source in [InstallSource::Composefs, InstallSource::Package] {
            let msg = existing_deploy_refusal(source, &["abc123.0".to_string()]);
            assert!(msg.contains("abc123.0"), "{msg}");
            assert!(msg.contains(source.deploy_dir()), "{msg}");
            assert!(msg.contains("bootc-migrate rollback"), "{msg}");
            assert!(msg.contains("undeploy"), "{msg}");
            assert!(msg.contains("delete the directory"), "{msg}");
        }
    }

    #[test]
    fn ostree_decide_note_names_the_etc_merge_not_phase_4() {
        // This route has no Phase 4; the Unknown warning must point at the
        // /etc merge that actually re-decides the lineage.
        assert!(OSTREE_INSTALL_DECIDE_NOTE.contains("installed deployment"));
        assert!(!OSTREE_INSTALL_DECIDE_NOTE.contains("Phase 4"));
    }

    #[test]
    fn carried_report_paths_land_in_each_new_var() {
        for (source, var, report) in [
            (InstallSource::Composefs, OSTREE_STATEROOT_VAR, REPORT_FILE),
            (
                InstallSource::Package,
                PACKAGE_OSTREE_STATEROOT_VAR,
                PACKAGE_REPORT_FILE,
            ),
        ] {
            assert_eq!(
                carried_report_path(source),
                Path::new(var).join("lib/bootc-rebase").join(report)
            );
        }
    }

    #[test]
    fn fstab_mount_matching_ignores_comments_and_blanks() {
        let fstab =
            "# a comment\n\nUUID=x / ext4 defaults 0 1\nUUID=y /boot/efi vfat umask=0077 0 2\n";
        assert!(fstab_has_mount(fstab, "/boot/efi"));
        assert!(fstab_has_mount(fstab, "/"));
        assert!(!fstab_has_mount(fstab, "/boot"));
        assert!(!fstab_has_mount(
            "# UUID=z /boot/efi vfat defaults 0 2\n",
            "/boot/efi"
        ));
        assert!(!fstab_has_mount("", "/boot/efi"));
    }

    /// The ESP line must not block boot when the ESP is missing, and must
    /// still be found by `fstab_has_mount` so a re-run does not append twice.
    #[test]
    fn esp_fstab_line_is_nofail() {
        let line = esp_fstab_line("ABCD-1234");
        assert_eq!(
            line,
            "UUID=ABCD-1234 /boot/efi vfat umask=0077,shortname=winnt,nofail 0 2\n"
        );
        let options = line.split_whitespace().nth(3).unwrap();
        assert!(options.split(',').any(|o| o == "nofail"), "{line}");
        assert!(fstab_has_mount(&line, "/boot/efi"));
    }

    #[test]
    fn mount_source_and_uuid_parsing() {
        let mounts = "/dev/mapper/root / btrfs rw 0 0\n/dev/nvme0n1p1 /boot/efi vfat rw 0 0\n";
        assert_eq!(
            mount_source_for(mounts, "/boot/efi").as_deref(),
            Some("/dev/nvme0n1p1")
        );
        assert_eq!(mount_source_for(mounts, "/boot"), None);
        assert_eq!(
            uuid_from_mount_source("/dev/disk/by-uuid/3E7D-D5C0").as_deref(),
            Some("3E7D-D5C0")
        );
        assert_eq!(uuid_from_mount_source("/dev/nvme0n1p1"), None);
    }

    #[test]
    fn supplement_identity_from_usr_lib_table() {
        let tmp = tempfile::tempdir().unwrap();
        let deploy = tmp.path().join("deploy");
        let usr_lib = deploy.join("usr/lib");
        let etc = deploy.join("etc");
        fs::create_dir_all(&usr_lib).unwrap();
        fs::create_dir_all(&etc).unwrap();

        // 1. Initial state: host has root and realuser; target has root, chrony, rpc, sssd
        fs::write(
            etc.join("passwd"),
            "root:x:0:0:root:/root:/bin/bash\nrealuser:x:1000:1000:Real User:/var/home/realuser:/bin/bash\n",
        )
        .unwrap();
        fs::write(etc.join("group"), "root:x:0:\nrealuser:x:1000:\n").unwrap();
        fs::write(
            usr_lib.join("passwd"),
            "root:x:0:0:root:/root:/bin/bash\nchrony:x:994:992::/var/lib/chrony:/usr/sbin/nologin\nrpc:x:32:32:Rpcbind Daemon:/var/lib/rpcbind:/usr/sbin/nologin\nsssd:x:993:991:User for sssd:/var/lib/sss:/usr/sbin/nologin\n",
        )
        .unwrap();
        fs::write(
            usr_lib.join("group"),
            "root:x:0:\nmail:x:12:\nchrony:x:992:\nrpc:x:32:\nsssd:x:991:\n",
        )
        .unwrap();
        fs::write(usr_lib.join("subuid"), "realuser:100000:65536\n").unwrap();

        let count = supplement_identity_from_usr_lib(&deploy, &etc).unwrap();
        assert_eq!(count, 3); // passwd, group, subuid

        let passwd = fs::read_to_string(etc.join("passwd")).unwrap();
        let group = fs::read_to_string(etc.join("group")).unwrap();
        let subuid = fs::read_to_string(etc.join("subuid")).unwrap();

        // Verify host accounts preserved first
        assert!(passwd.starts_with("root:x:0:0:root:/root:/bin/bash\nrealuser:x:1000:1000:Real User:/var/home/realuser:/bin/bash\n"));
        // Verify target accounts appended
        assert!(passwd.contains("chrony:x:994:992::/var/lib/chrony:/usr/sbin/nologin\n"));
        assert!(passwd.contains("rpc:x:32:32:Rpcbind Daemon:/var/lib/rpcbind:/usr/sbin/nologin\n"));
        assert!(passwd.contains("sssd:x:993:991:User for sssd:/var/lib/sss:/usr/sbin/nologin\n"));

        assert!(group.starts_with("root:x:0:\nrealuser:x:1000:\n"));
        assert!(group.contains("mail:x:12:\n"));
        assert!(group.contains("chrony:x:992:\n"));
        assert!(group.contains("rpc:x:32:\n"));
        assert!(group.contains("sssd:x:991:\n"));

        assert_eq!(subuid, "realuser:100000:65536\n");

        // Idempotent second run modifies nothing
        let count2 = supplement_identity_from_usr_lib(&deploy, &etc).unwrap();
        assert_eq!(count2, 0);

        // Missing usr/lib returns 0 without error
        let empty_deploy = tmp.path().join("empty_deploy");
        let count_empty = supplement_identity_from_usr_lib(&empty_deploy, &etc).unwrap();
        assert_eq!(count_empty, 0);
    }
}
