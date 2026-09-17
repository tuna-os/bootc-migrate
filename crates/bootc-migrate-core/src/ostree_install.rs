//! composefs → ostree: `Strategy::OstreeInstall` (bootc-migrate#260, part of
//! the any-base-to-any-base goal, #258).
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
//! It initializes `/sysroot/ostree`, deploys the image, installs the
//! bootloader through bootupd, and — because it writes the deployment's
//! origin and BLS entry itself — leaves the target's bootc in charge of
//! day-2 updates, which is the whole point of going back. Three facts about
//! it shape everything below (read from upstream `install.rs`):
//!
//! 1. Without an explicit `/target` it bind-mounts PID 1's `/`, which on a
//!    composefs host is the sealed overlay, not the block device. The
//!    physical root is `/sysroot`, so that is what is bound to `/target`.
//! 2. Alongside mode **empties the ESP** (`clean_boot_directories`), which
//!    destroys the composefs deployment's kernel, initrd, BLS entry and
//!    systemd-boot — the rollback path. So the composefs-owned ESP artifacts
//!    are snapshotted first and restored afterwards, beside the shim/GRUB
//!    that bootupd writes. Both loaders then coexist: GRUB through its NVRAM
//!    entry, systemd-boot through "Linux Boot Manager".
//! 3. It carries nothing else: the new deployment's `/etc` is the factory
//!    copy and its stateroot `/var` is empty. The `/etc` merge (same 3-way
//!    rule as Phase 4, cross-family policy included) and the `/var` copy
//!    are this module's.
//!
//! Planning helpers ([`install_command`], [`composefs_esp_paths`],
//! [`newest_deployment`]) are pure; the I/O lives in [`OstreeInstallConfig::run`].

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::cross_family;
use crate::mergetc::{self, IdentityMergePolicy, MergePolicy};
use crate::migration::rollback;
use crate::migration::{PodmanImageMount, find_esp_or_mount};
use crate::preflight;
use crate::rebase_controller::validate_target_image;
use crate::registry;
use crate::scan;
use crate::xattr;

/// The physical root of a composefs host (the block-device filesystem the
/// composefs store and state live on).
pub const PHYSICAL_ROOT: &str = "/sysroot";
/// Where the target's bootc sees the physical root inside its container —
/// bootc's own `ALONGSIDE_ROOT_MOUNT`.
pub const CONTAINER_TARGET: &str = "/target";
/// Where the new OSTree deployments land (bootc's default stateroot).
pub const OSTREE_DEPLOY_DIR: &str = "/sysroot/ostree/deploy/default/deploy";
/// The new stateroot's `/var`.
pub const OSTREE_STATEROOT_VAR: &str = "/sysroot/ostree/deploy/default/var";
/// Where the ESP snapshot and the report are kept across the reboot.
pub const STATE_DIR: &str = "/var/lib/bootc-rebase";
/// The report's file name under [`STATE_DIR`].
pub const REPORT_FILE: &str = "ostree-install-report.json";

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

// ---- Pure planning -------------------------------------------------------

/// The `podman run … bootc install to-existing-root` invocation, as argv.
///
/// - `--privileged --pid=host`, `/dev` and `/sys`: what `bootc install`
///   needs to see block devices and firmware.
/// - `--security-opt label=disable`: lets bootc write `security.selinux`
///   xattrs onto the target (same as the E2E harness's own installs).
/// - `/var/tmp` bound from the host: ostree writes multi-GB import blobs
///   there, and the container's own `/var/tmp` is far too small.
/// - `/var/lib/containers` bound so the already-pulled image is used, not
///   re-fetched; `--skip-fetch-check` for the same reason.
/// - `/sysroot` bound at `/target` with `rslave` propagation so the host's
///   `/boot` and ESP mounts underneath are visible (see the module docs).
/// - `--acknowledge-destructive`: we know it is the host root; the ESP is
///   snapshotted before this runs.
pub fn install_command(target_image: &str, kargs: &[String]) -> Vec<String> {
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
        format!("type=bind,src={PHYSICAL_ROOT},dst={CONTAINER_TARGET},bind-propagation=rslave"),
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

/// Kernel arguments to carry from the composefs host's cmdline onto the
/// new deployment's BLS entry beyond what bootc inherits itself (`root=`,
/// `rootflags=`, `rd.*`): the console and LVM activation arguments, which
/// the E2E harness and a dedicated-`/var` host both rely on. Backend
/// locators (`composefs=`, `ostree=`) and loader internals are dropped.
pub fn carry_over_kargs(cmdline: &str) -> Vec<String> {
    cmdline
        .split_whitespace()
        .filter(|w| {
            w.starts_with("console=")
                || w.starts_with("rd.lvm.lv=")
                || w.starts_with("rd.luks.")
                || w.starts_with("systemd.log_level=")
        })
        .map(str::to_string)
        .collect()
}

/// What the run did, written as JSON under [`STATE_DIR`] so it survives the
/// reboot.
#[derive(Debug, Clone, Serialize)]
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
}

// ---- I/O -----------------------------------------------------------------

impl OstreeInstallConfig<'_> {
    /// Run the route: preflight, gate, pull, ESP snapshot, alongside
    /// install, `/etc` merge, `/var` copy, ESP restore, NVRAM order, report.
    pub fn run(&self) -> Result<()> {
        validate_target_image(self.target_image)?;
        if self.dry_run {
            println!("*** DRY RUN MODE — no changes will be made ***");
        }
        println!("Checking system state...");

        let cmdline = fs::read_to_string("/proc/cmdline").unwrap_or_default();
        if !cmdline.contains("composefs=") && !self.force {
            bail!(
                "System is not booted from a composefs deployment (/proc/cmdline has no \
                 composefs= parameter); this route converts a composefs host back to the \
                 OSTree backend. Use --force to override."
            );
        }
        if !Path::new("/sys/firmware/efi").exists() {
            bail!("the composefs -> ostree route requires UEFI firmware (NVRAM boot entries).");
        }
        for tool in ["podman", "efibootmgr"] {
            if which(tool).is_none() {
                bail!("`{tool}` is required for the composefs -> ostree route and was not found");
            }
        }
        if Path::new(OSTREE_DEPLOY_DIR).is_dir() && !self.force {
            let existing = list_deployments()?;
            if !existing.is_empty() {
                bail!(
                    "an OSTree deployment already exists under {OSTREE_DEPLOY_DIR} ({}). If this \
                     host was migrated by bootc-migrate and never committed, `bootc-migrate \
                     rollback` returns to it without reinstalling; pass --force to install a \
                     fresh deployment alongside it anyway.",
                    existing.join(", ")
                );
            }
        }

        // #256/#258: lineage gate. No /etc policy can be applied to what
        // `bootc install` writes — the merge below is ours, so the policy
        // applies there exactly as in Phase 4.
        cross_family::gate(self.target_image, self.accept_cross_base)?;

        if !self.skip_preflight {
            match scan::scan_target_image(self.target_image) {
                Ok(caps) => {
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
                Err(e) => eprintln!(
                    "Warning: could not scan target image {} ({e:#}); proceeding without the \
                     capability check.",
                    self.target_image
                ),
            }
        }

        let esp = find_esp_or_mount().context("locating the EFI system partition")?;
        let report = preflight::run_preflight_checks().ok();
        let booted_image = report.as_ref().and_then(|r| r.booted_image.clone());

        if self.dry_run {
            println!("[DRY RUN] Would pull {} with podman.", self.target_image);
            println!(
                "[DRY RUN] Would snapshot composefs ESP artifacts from {esp} to {STATE_DIR}/esp-snapshot-*."
            );
            println!(
                "[DRY RUN] Would run: {}",
                install_command(self.target_image, &carry_over_kargs(&cmdline)).join(" ")
            );
            println!(
                "[DRY RUN] Would 3-way merge /etc into the new deployment (source defaults from {}), copy /var to {OSTREE_STATEROOT_VAR}, restore the composefs ESP artifacts, and put the GRUB entry first in BootOrder.",
                booted_image.as_deref().unwrap_or("<unknown booted image>")
            );
            return Ok(());
        }

        let _sleep_guard =
            crate::migration::SleepGuard::new("bootc composefs -> ostree re-base in progress");

        // ---- Pull ----
        println!("=== Pull: {} ===", self.target_image);
        run_checked(
            Command::new("podman").args(["pull", "--policy", "always", self.target_image]),
            "podman pull",
        )?;

        // ---- ESP snapshot ----
        println!("=== ESP snapshot: {esp} ===");
        fs::create_dir_all(STATE_DIR)?;
        let snapshot_dir = Path::new(STATE_DIR).join(format!(
            "esp-snapshot-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        ));
        let snapshot_paths = snapshot_esp(Path::new(&esp), &snapshot_dir)?;
        println!(
            "[esp] {} composefs artifact(s) snapshotted to {}",
            snapshot_paths.len(),
            snapshot_dir.display()
        );
        if !snapshot_paths
            .iter()
            .any(|p| p.starts_with("EFI/Linux/bootc_composefs-"))
        {
            eprintln!(
                "Warning: no composefs kernel directory found on the ESP; the running \
                 deployment will not be restorable as a rollback entry after the install."
            );
        }

        // ---- Deploy: the target's bootc, alongside ----
        println!(
            "=== Deploy: bootc install to-existing-root ({}) ===",
            self.target_image
        );
        let _ = Command::new("mount")
            .args(["-o", "remount,rw", PHYSICAL_ROOT])
            .status();
        let _boot_binds = bind_boot_into_physical_root(&esp)?;
        let argv = install_command(self.target_image, &carry_over_kargs(&cmdline));
        println!("[deploy] {}", argv.join(" "));
        let status = Command::new(&argv[0])
            .args(&argv[1..])
            .status()
            .context("failed to execute podman")?;
        if !status.success() {
            bail!(
                "bootc install to-existing-root failed (exit {status}); the composefs ESP \
                 snapshot is at {} — restore it with `bootc-rebase` before rebooting if the \
                 ESP was already emptied",
                snapshot_dir.display()
            );
        }
        drop(_boot_binds);

        let deployment = newest_deployment(&deployment_entries()?).ok_or_else(|| {
            anyhow!("no OSTree deployment found under {OSTREE_DEPLOY_DIR} after the install")
        })?;
        let deploy_root = Path::new(OSTREE_DEPLOY_DIR).join(&deployment);
        println!("[deploy] new deployment: {}", deploy_root.display());

        // ---- /etc ----
        println!("=== /etc: 3-way merge into the new deployment ===");
        let (etc_merge, cross) = merge_etc_into(
            &deploy_root,
            booted_image.as_deref(),
            self.accept_cross_base,
        )?;
        println!("[etc] {etc_merge}");

        // ---- /var ----
        println!("=== /var: carrying the live tree into the stateroot ===");
        let var_copied = copy_var_into_stateroot()?;

        if let Some(outcome) = cross.as_ref() {
            let source_passwd = fs::read_to_string("/etc/passwd").unwrap_or_default();
            let source_group = fs::read_to_string("/etc/group").unwrap_or_default();
            cross_family::apply_post_merge(&cross_family::PostMergeInputs {
                etc_outcome: outcome,
                deploy_dir: &deploy_root,
                etc_dir: &deploy_root.join("etc"),
                staged_var: if var_copied {
                    cross_family::StagedVar::Copied(Path::new(OSTREE_STATEROOT_VAR))
                } else {
                    cross_family::StagedVar::InPlace
                },
                host_selinux: crate::selinux::read_host_selinux_config(),
                source_passwd: &source_passwd,
                source_group: &source_group,
            })?;
        }

        // ---- ESP restore + NVRAM ----
        println!("=== Bootloader: restoring the composefs rollback entry ===");
        let restored = restore_esp(&snapshot_dir, Path::new(&esp))?;
        println!("[esp] {} composefs artifact(s) restored", restored.len());
        let grub_entry = put_grub_first()?;

        let report = OstreeInstallReport {
            target_image: self.target_image.to_string(),
            deployment: deployment.clone(),
            esp: esp.clone(),
            esp_snapshot_dir: snapshot_dir.display().to_string(),
            esp_paths_restored: restored,
            etc_merge,
            var_copied,
            cross_family: cross.is_some(),
            grub_boot_entry: grub_entry,
        };
        let report_path = Path::new(STATE_DIR).join(REPORT_FILE);
        fs::write(
            &report_path,
            serde_json::to_string_pretty(&report).expect("OstreeInstallReport serializes"),
        )
        .with_context(|| format!("writing {}", report_path.display()))?;
        println!("Report written to {}", report_path.display());
        println!(
            "OSTree deployment staged. Reboot to enter it; the composefs deployment stays \
             selectable through the \"Linux Boot Manager\" firmware entry as rollback."
        );
        Ok(())
    }
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

fn list_deployments() -> Result<Vec<String>> {
    Ok(deployment_entries()?.into_iter().map(|(n, _)| n).collect())
}

fn deployment_entries() -> Result<Vec<(String, u64)>> {
    let dir = Path::new(OSTREE_DEPLOY_DIR);
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

fn bind_boot_into_physical_root(esp: &str) -> Result<BootBinds> {
    let live_mounts: Vec<&str> = ["/boot", "/boot/efi", "/efi", esp]
        .into_iter()
        .filter(|p| is_mountpoint(Path::new(p)))
        .collect();
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
    _tmp: Option<tempfile::TempDir>,
}

/// Obtain [`SourceEtc`] from the booted image's podman mount when it is
/// cached locally, else streamed from the registry; `None` (with a
/// warning) when neither works, which makes the merge keep every live path
/// (safe, but no vendor update is applied).
fn source_default_etc(booted_image: Option<&str>) -> Result<Option<SourceEtc>> {
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
        let read = |n: &str| fs::read_to_string(new_default.join(n)).unwrap_or_default();
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
    // merge writes the whole result.
    if out.exists() {
        fs::remove_dir_all(&out).with_context(|| format!("clearing {}", out.display()))?;
    }
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

/// Copy the live `/var` into the new stateroot's `/var` unless `/var` is a
/// dedicated filesystem the deployment's fstab will mount itself.
fn copy_var_into_stateroot() -> Result<bool> {
    if let Some(var) = crate::migration::var_layout::detect_separate_var()? {
        println!(
            "[var] /var is a dedicated filesystem (UUID={}); the merged /etc/fstab mounts it, \
             nothing to copy",
            var.uuid
        );
        return Ok(false);
    }
    let dst = Path::new(OSTREE_STATEROOT_VAR);
    fs::create_dir_all(dst)?;
    xattr::copy_dir_all_with_xattrs("/var", dst)
        .context("copying /var into the OSTree stateroot")?;
    println!("[var] live /var copied to {}", dst.display());
    Ok(true)
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
