//! Live ESP/NVRAM mutation for the standalone `migrate-bootloader` command.
//!
//! The standalone path deliberately has three observable phases:
//!
//! 1. Stage systemd-boot, the current deployment's BLS artifacts, and a
//!    `kernel-install` resync hook.  Register the loader without changing the
//!    existing `BootOrder`, then select it with a one-boot `BootNext` trial.
//! 2. `--promote` moves the trial entry to the front of `BootOrder`, but only
//!    after the running system reports that it booted from that entry.
//! 3. `--undo` removes artifacts owned by this migration and restores the
//!    pre-migration firmware ordering.
//!
//! Filesystem and process I/O lives here; BLS assembly remains in
//! [`super::systemd_boot`] so the parsing and formatting logic stays
//! unit-testable.  The QEMU E2E cell is the proof for the firmware and reboot
//! sequence; the tests below cover the state and file primitives.

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::BlsEntry;
use super::systemd_boot::{carry_over_kargs, esp_kernel_paths};
use crate::migration::boot::get_esp_disk_and_part;
use crate::migration::rollback::{build_new_boot_order, parse_boot_current, parse_boot_order};

/// Persistent state for one standalone bootloader migration.
///
/// The state is intentionally explicit: `--undo` must not infer ownership from
/// whatever happens to be on the ESP after a reboot or a kernel update.  The
/// `serde(default)` fields keep state written by the first implementation
/// readable while the migration is being upgraded in place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationState {
    pub entry_token: String,
    pub esp_path: String,
    /// Kept as the primary entry for compatibility with the original state
    /// format.  `artifacts` contains every entry staged by the current code.
    pub bls_filename: String,
    /// The `efibootmgr` Boot#### id, without the `Boot` prefix.
    pub boot_id: String,
    #[serde(default)]
    pub kernel_version: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub strip_composefs: bool,
    #[serde(default)]
    pub artifacts: Vec<MigrationArtifact>,
    #[serde(default)]
    pub original_boot_order: Option<String>,
    #[serde(default)]
    pub original_boot_next: Option<String>,
    #[serde(default = "default_true")]
    pub nvram_entry_created: bool,
    #[serde(default)]
    pub created_loader_files: Vec<String>,
    #[serde(default)]
    pub loader_conf_before: Option<String>,
    #[serde(default)]
    pub hook_before: Option<String>,
}

fn default_true() -> bool {
    true
}

/// One kernel/BLS pair owned by the migration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationArtifact {
    pub kernel_version: String,
    pub bls_filename: String,
}

/// Inputs for one staged kernel/BLS pair.  The caller builds the BLS entry
/// using the existing deployment discovery path; this module only performs
/// the live copy and records what it owns.
#[derive(Debug)]
pub struct ArtifactInput<'a> {
    pub kernel_version: &'a str,
    pub vmlinuz: &'a Path,
    pub initrd: Option<&'a Path>,
    pub entry: &'a BlsEntry,
}

/// Default state location used by both CLI frontends and the installed hook.
pub const STATE_PATH: &str = "/var/lib/bootc-migrate/migrate-bootloader-state.json";

/// `kernel-install` drop-in installed by the migration.
pub const RESYNC_HOOK_PATH: &str = "/etc/kernel/install.d/95-bootc-migrate-esp-sync.install";
/// Where the resync hook keeps its own copy of the binary it runs.
///
/// The hook cannot rely on `PATH`: on images that keep `/usr/local` inside
/// the read-only `/usr` (Bluefin LTS), nothing writable on `PATH` survives a
/// reboot, so `bootc-migrate` is wherever the operator ran it from.
pub const HOOK_BINARY_DIR: &str = "/var/lib/bootc-migrate/bin";

const TRIAL_LABEL: &str = "Linux Boot Manager (bootc-migrate trial)";
const LOADER_RELATIVE_PATHS: [&str; 2] =
    ["EFI/systemd/systemd-bootx64.efi", "EFI/BOOT/BOOTX64.EFI"];

/// Validate the token used as the ESP namespace and in the hook command.
pub fn validate_entry_token(entry_token: &str) -> Result<()> {
    if entry_token.is_empty()
        || entry_token == "."
        || entry_token == ".."
        || entry_token.contains('/')
        || entry_token.contains('\\')
        || entry_token.chars().any(char::is_control)
    {
        bail!("invalid systemd-boot entry token {:?}", entry_token);
    }
    Ok(())
}

/// Load state written by a previous migration, or return `None` when no
/// migration is active.
pub fn load_state(state_path: &Path) -> Result<Option<MigrationState>> {
    match fs::read_to_string(state_path) {
        Ok(contents) => Ok(Some(
            serde_json::from_str(&contents).context("parsing migrate-bootloader state file")?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", state_path.display())),
    }
}

fn save_state(state_path: &Path, state: &MigrationState) -> Result<()> {
    let parent = state_path
        .parent()
        .ok_or_else(|| anyhow!("state path {} has no parent", state_path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;

    let serialized = serde_json::to_vec_pretty(state).context("serializing migration state")?;
    let temporary = state_path.with_extension("json.tmp");
    fs::write(&temporary, serialized)
        .with_context(|| format!("writing temporary state {}", temporary.display()))?;
    let mut permissions = fs::metadata(&temporary)?.permissions();
    permissions.set_mode(0o600);
    fs::set_permissions(&temporary, permissions)?;
    fs::rename(&temporary, state_path)
        .with_context(|| format!("promoting state {}", state_path.display()))
}

fn read_optional(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn read_efibootmgr() -> Result<String> {
    let output = Command::new("efibootmgr")
        .arg("-v")
        .output()
        .context("failed to execute efibootmgr -v")?;
    if !output.status.success() {
        bail!(
            "efibootmgr -v failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn parse_boot_next(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        line.trim()
            .strip_prefix("BootNext:")
            .map(str::trim)
            .filter(|id| id.len() == 4 && id.chars().all(|c| c.is_ascii_hexdigit()))
            .map(str::to_owned)
    })
}

fn find_entry_id(output: &str, label: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let trimmed = line.trim();
        let rest = trimmed.strip_prefix("Boot")?;
        let id = rest.get(..4)?;
        if !id.chars().all(|c| c.is_ascii_hexdigit()) || !trimmed.contains(label) {
            return None;
        }
        Some(id.to_owned())
    })
}

fn run_efibootmgr(args: &[&str], operation: &str) -> Result<()> {
    let status = Command::new("efibootmgr")
        .args(args)
        .status()
        .with_context(|| format!("failed to execute efibootmgr {operation}"))?;
    if !status.success() {
        bail!("efibootmgr {operation} exited {status}");
    }
    Ok(())
}

fn set_boot_order(order: &str) -> Result<()> {
    run_efibootmgr(&["--bootorder", order], &format!("--bootorder {order}"))
}

fn set_boot_next(id: &str) -> Result<()> {
    run_efibootmgr(&["--bootnext", id], &format!("--bootnext {id}"))
}

fn delete_boot_next() -> Result<()> {
    run_efibootmgr(&["--delete-bootnext"], "--delete-bootnext")
}

fn create_trial_entry(esp_path: &Path) -> Result<(String, bool, Option<String>, Option<String>)> {
    let before = read_efibootmgr()?;
    let original_boot_order = parse_boot_order(&before)
        .ok_or_else(|| anyhow!("could not parse BootOrder from efibootmgr output"))?;
    let original_boot_next = parse_boot_next(&before);

    let (boot_id, created) = if let Some(existing) = find_entry_id(&before, TRIAL_LABEL) {
        (existing, false)
    } else {
        let (disk, part) = get_esp_disk_and_part(
            esp_path
                .to_str()
                .ok_or_else(|| anyhow!("ESP path is not valid UTF-8"))?,
        )
        .ok_or_else(|| {
            anyhow!(
                "could not determine the ESP backing disk and partition for efibootmgr (findmnt {})",
                esp_path.display()
            )
        })?;
        run_efibootmgr(
            &[
                "--create",
                "--disk",
                &disk,
                "--part",
                &part,
                "--loader",
                "\\EFI\\systemd\\systemd-bootx64.efi",
                "--label",
                TRIAL_LABEL,
            ],
            "--create",
        )?;
        let after = read_efibootmgr()?;
        let id = find_entry_id(&after, TRIAL_LABEL)
            .ok_or_else(|| anyhow!("could not find the new '{TRIAL_LABEL}' NVRAM entry"))?;
        // efibootmgr commonly prepends a newly-created entry to BootOrder.
        // Restore the exact pre-migration order before selecting BootNext.
        set_boot_order(&original_boot_order)?;
        (id, true)
    };

    set_boot_next(&boot_id)?;
    Ok((
        boot_id,
        created,
        Some(original_boot_order),
        original_boot_next,
    ))
}

/// Locate a host kernel for a resync when `kernel-install` did not provide a
/// path.  The module layout is the bootc/OSTree convention; `/boot` is the
/// fallback used by regular Fedora installations.
pub fn find_host_vmlinuz(kver: &str) -> Result<PathBuf> {
    let candidates = [
        PathBuf::from(format!("/usr/lib/modules/{kver}/vmlinuz")),
        PathBuf::from(format!("/boot/vmlinuz-{kver}")),
    ];
    candidates
        .into_iter()
        .find(|path| path.exists())
        .ok_or_else(|| {
            anyhow!("could not find vmlinuz for kernel {kver} under /usr/lib/modules or /boot")
        })
}

/// Build an initramfs for a resync when the kernel-install hook did not pass
/// one.  The normal hook path uses the initramfs that kernel-install already
/// built; this fallback keeps a manual `--resync` useful.
pub fn build_initrd(kver: &str, destination: &Path) -> Result<()> {
    let dracut = ["/usr/bin/dracut", "/usr/sbin/dracut", "dracut"]
        .into_iter()
        .find(|candidate| *candidate == "dracut" || Path::new(candidate).exists())
        .ok_or_else(|| anyhow!("dracut not found on host"))?;
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let status = Command::new(dracut)
        .args(["--force", "--kver", kver])
        .arg(destination)
        .status()
        .context("failed to execute dracut")?;
    if !status.success() {
        bail!("dracut exited {status} building initramfs for {kver}");
    }
    Ok(())
}

/// Copy a kernel/initramfs pair into the systemd-boot `$BOOT/<token>/<kver>`
/// directory.
pub fn populate_esp_kernel(
    esp_path: &Path,
    entry_token: &str,
    kver: &str,
    vmlinuz_source: &Path,
    initrd_source: &Path,
) -> Result<()> {
    validate_entry_token(entry_token)?;
    let destination = esp_path.join(entry_token).join(kver);
    fs::create_dir_all(&destination)
        .with_context(|| format!("creating {}", destination.display()))?;
    fs::copy(vmlinuz_source, destination.join("linux"))
        .with_context(|| format!("copying {} to ESP", vmlinuz_source.display()))?;
    fs::copy(initrd_source, destination.join("initrd"))
        .with_context(|| format!("copying {} to ESP", initrd_source.display()))?;
    Ok(())
}

/// Write one BLS entry under the ESP's loader directory.
pub fn write_bls_entry(esp_path: &Path, entry: &BlsEntry) -> Result<()> {
    let entries_dir = esp_path.join("loader/entries");
    fs::create_dir_all(&entries_dir)
        .with_context(|| format!("creating {}", entries_dir.display()))?;
    fs::write(entries_dir.join(&entry.filename), entry.render())
        .with_context(|| format!("writing BLS entry {}", entry.filename))
}

fn install_systemd_boot_binary_with_state(
    esp_path: &Path,
    from_image: Option<&str>,
) -> Result<Vec<String>> {
    let host_source = Path::new("/usr/lib/systemd/boot/efi/systemd-bootx64.efi");
    let systemd_dir = esp_path.join("EFI/systemd");
    let removable_dir = esp_path.join("EFI/BOOT");
    let systemd_destination = systemd_dir.join("systemd-bootx64.efi");
    let removable_destination = removable_dir.join("BOOTX64.EFI");
    let mut created = Vec::new();

    if !systemd_destination.exists() {
        fs::create_dir_all(&systemd_dir)?;
        if host_source.exists() {
            fs::copy(host_source, &systemd_destination)
                .context("installing systemd-bootx64.efi")?;
        } else if let Some(image) = from_image {
            crate::registry::extract_files_via_registry(
                image,
                &[(
                    Path::new("/usr/lib/systemd/boot/efi/systemd-bootx64.efi"),
                    systemd_destination.as_path(),
                )],
            )
            .with_context(|| format!("extracting systemd-bootx64.efi from {image}"))?;
        } else {
            bail!(
                "host does not ship systemd-bootx64.efi at {} and no --from-image was given; \
                 pass --from-image <image> or install a systemd-boot package",
                host_source.display()
            );
        }
        created.push(LOADER_RELATIVE_PATHS[0].to_owned());
    }

    if !removable_destination.exists() {
        fs::create_dir_all(&removable_dir)?;
        fs::copy(&systemd_destination, &removable_destination)
            .context("installing BOOTX64.EFI removable-media loader")?;
        created.push(LOADER_RELATIVE_PATHS[1].to_owned());
    }
    Ok(created)
}

fn build_entry(
    title: &str,
    version: &str,
    entry_token: &str,
    cmdline: &str,
    filename: &str,
    sort_key: &str,
    strip_composefs: bool,
) -> BlsEntry {
    let (linux, initrd) = esp_kernel_paths(entry_token, version);
    let strip = if strip_composefs {
        &["composefs="][..]
    } else {
        &[]
    };
    BlsEntry {
        title: title.to_owned(),
        version: version.to_owned(),
        linux,
        initrds: vec![initrd],
        options: carry_over_kargs(cmdline, strip),
        filename: filename.to_owned(),
        sort_key: sort_key.to_owned(),
    }
}

fn ensure_initrd(
    kver: &str,
    requested: Option<&Path>,
) -> Result<(PathBuf, Option<tempfile::TempDir>)> {
    if let Some(path) = requested.filter(|path| path.exists()) {
        return Ok((path.to_owned(), None));
    }
    let temporary = tempfile::Builder::new()
        .prefix("bootc-migrate-initrd-")
        .tempdir_in("/var/tmp")
        .context("creating scratch directory for initramfs build")?;
    let destination = temporary.path().join("initrd");
    build_initrd(kver, &destination)?;
    Ok((destination, Some(temporary)))
}

/// Stage the live files and create the one-boot trial.  A second invocation
/// is idempotent: it refreshes the current kernel's files instead of adding a
/// second firmware entry.
#[allow(clippy::too_many_arguments)]
pub fn stage_artifacts(
    state_path: &Path,
    esp_path: &Path,
    entry_token: &str,
    title: &str,
    strip_composefs: bool,
    artifacts: &[ArtifactInput<'_>],
    loader_conf: &str,
    from_image: Option<&str>,
) -> Result<MigrationState> {
    let first = artifacts
        .first()
        .ok_or_else(|| anyhow!("cannot stage a bootloader migration without a kernel"))?;

    if let Some(state) = load_state(state_path)? {
        println!(
            "migrate-bootloader already has state at {}; refreshing the ESP without creating a second NVRAM entry",
            state_path.display()
        );
        let (initrd, _temporary) = ensure_initrd(first.kernel_version, first.initrd)?;
        resync_with_sources(
            Path::new(&state.esp_path),
            &state.entry_token,
            first.kernel_version,
            first.vmlinuz,
            &initrd,
            first.entry,
        )?;
        return Ok(state);
    }

    let loader_conf_path = esp_path.join("loader/loader.conf");
    let loader_conf_before = read_optional(&loader_conf_path)?;
    let hook_before = read_optional(Path::new(RESYNC_HOOK_PATH))?;

    let mut temporary_initrds = Vec::new();
    for artifact in artifacts {
        let (initrd, temporary) = ensure_initrd(artifact.kernel_version, artifact.initrd)?;
        populate_esp_kernel(
            esp_path,
            entry_token,
            artifact.kernel_version,
            artifact.vmlinuz,
            &initrd,
        )?;
        write_bls_entry(esp_path, artifact.entry)?;
        if let Some(temporary) = temporary {
            temporary_initrds.push(temporary);
        }
    }

    let created_loader_files = install_systemd_boot_binary_with_state(esp_path, from_image)?;
    fs::create_dir_all(loader_conf_path.parent().expect("loader.conf has a parent"))?;
    fs::write(&loader_conf_path, loader_conf).context("writing systemd-boot loader.conf")?;
    install_resync_hook(entry_token, title)?;

    let (boot_id, nvram_entry_created, original_boot_order, original_boot_next) =
        create_trial_entry(esp_path)?;
    let state = MigrationState {
        entry_token: entry_token.to_owned(),
        esp_path: esp_path.to_string_lossy().into_owned(),
        bls_filename: first.entry.filename.clone(),
        boot_id,
        kernel_version: first.kernel_version.to_owned(),
        title: title.to_owned(),
        strip_composefs,
        artifacts: artifacts
            .iter()
            .map(|artifact| MigrationArtifact {
                kernel_version: artifact.kernel_version.to_owned(),
                bls_filename: artifact.entry.filename.clone(),
            })
            .collect(),
        original_boot_order,
        original_boot_next,
        nvram_entry_created,
        created_loader_files,
        loader_conf_before,
        hook_before,
    };
    save_state(state_path, &state)?;
    drop(temporary_initrds);

    println!(
        "Staged systemd-boot as a one-boot trial (Boot{} via BootNext). BootOrder was left unchanged; reboot to test it, then run `migrate-bootloader --promote` or `--undo`.",
        state.boot_id
    );
    Ok(state)
}

/// Re-copy a supplied kernel/initramfs pair and rewrite its BLS entry.
pub fn resync_with_sources(
    esp_path: &Path,
    entry_token: &str,
    kver: &str,
    vmlinuz: &Path,
    initrd: &Path,
    entry: &BlsEntry,
) -> Result<()> {
    populate_esp_kernel(esp_path, entry_token, kver, vmlinuz, initrd)?;
    write_bls_entry(esp_path, entry)
}

/// Manual resync entry point retained for callers that only have a kernel
/// version.  The installed hook uses [`resync_from_kernel_install`] so it can
/// reuse the initramfs that `kernel-install` already built.
pub fn resync_for_kernel_update(
    esp_path: &Path,
    entry_token: &str,
    kver: &str,
    cmdline: &str,
    title: &str,
) -> Result<()> {
    let vmlinuz = find_host_vmlinuz(kver)?;
    let (initrd, _temporary) = ensure_initrd(kver, None)?;
    let filename = format!("bootc-migrate-{entry_token}-{kver}.conf");
    let entry = build_entry(
        title,
        kver,
        entry_token,
        cmdline,
        &filename,
        "bootc-migrate-0",
        true,
    );
    resync_with_sources(esp_path, entry_token, kver, &vmlinuz, &initrd, &entry)
}

/// Resync the ESP from the arguments supplied by a `kernel-install` hook.
pub fn resync_from_kernel_install(
    state_path: &Path,
    entry_token_override: Option<&str>,
    title_override: Option<&str>,
    kver: &str,
    vmlinuz_override: Option<&Path>,
    initrd_override: Option<&Path>,
) -> Result<()> {
    let mut state = load_state(state_path)?.ok_or_else(|| {
        anyhow!(
            "no migrate-bootloader state found at {}; run the migration before resyncing",
            state_path.display()
        )
    })?;
    let entry_token = entry_token_override.unwrap_or(&state.entry_token);
    let title = title_override
        .filter(|title| !title.is_empty())
        .unwrap_or(&state.title);
    if title.is_empty() {
        bail!("migrate-bootloader resync has no BLS title in state or arguments");
    }
    let cmdline = fs::read_to_string("/proc/cmdline").unwrap_or_default();
    let vmlinuz = match vmlinuz_override.filter(|path| path.exists()) {
        Some(path) => path.to_owned(),
        None => find_host_vmlinuz(kver)?,
    };
    let (initrd, _temporary) = ensure_initrd(kver, initrd_override)?;
    let filename = state
        .artifacts
        .iter()
        .find(|artifact| artifact.kernel_version == kver)
        .map(|artifact| artifact.bls_filename.clone())
        .unwrap_or_else(|| format!("bootc-migrate-{entry_token}-{kver}.conf"));
    let entry = build_entry(
        title,
        kver,
        entry_token,
        cmdline.trim(),
        &filename,
        "bootc-migrate-0",
        state.strip_composefs,
    );
    let esp_path = Path::new(&state.esp_path);
    resync_with_sources(esp_path, entry_token, kver, &vmlinuz, &initrd, &entry)?;
    update_loader_default(esp_path, &filename)?;

    state.entry_token = entry_token.to_owned();
    state.title = title.to_owned();
    state.kernel_version = kver.to_owned();
    state.bls_filename = filename.clone();
    if let Some(artifact) = state
        .artifacts
        .iter_mut()
        .find(|artifact| artifact.kernel_version == kver)
    {
        artifact.bls_filename = filename;
    } else {
        state.artifacts.push(MigrationArtifact {
            kernel_version: kver.to_owned(),
            bls_filename: filename,
        });
    }
    save_state(state_path, &state)
}

/// Promote the trial entry after confirming that this boot actually came from
/// it.  A missing `BootCurrent` is tolerated for older firmware tools, but a
/// conflicting value is a hard refusal.
pub fn run_promote(state_path: &Path) -> Result<()> {
    let state = load_state(state_path)?.ok_or_else(|| {
        anyhow!(
            "no migrate-bootloader state found at {}; run the migration first",
            state_path.display()
        )
    })?;
    let output = read_efibootmgr()?;
    if let Some(current) = parse_boot_current(&output)
        && !current.eq_ignore_ascii_case(&state.boot_id)
    {
        bail!(
            "refusing to promote Boot{}: the running system booted from Boot{}, not the trial entry",
            state.boot_id,
            current
        );
    }
    let current_order = parse_boot_order(&output)
        .ok_or_else(|| anyhow!("could not parse BootOrder from efibootmgr output"))?;
    let new_order = build_new_boot_order(&current_order, &state.boot_id);
    set_boot_order(&new_order)?;
    if parse_boot_next(&output).as_deref() == Some(state.boot_id.as_str()) {
        delete_boot_next()?;
    }
    println!("Promoted Boot{} to the front of BootOrder.", state.boot_id);
    Ok(())
}

fn order_without(order: &str, removed: &str) -> String {
    order
        .split(',')
        .filter(|id| !id.eq_ignore_ascii_case(removed) && !id.is_empty())
        .collect::<Vec<_>>()
        .join(",")
}

fn restore_order(original: Option<&str>, current: &str, removed: &str) -> Option<String> {
    let current_without = order_without(current, removed);
    let Some(original) = original else {
        return (!current_without.is_empty()).then_some(current_without);
    };
    let mut ids = original
        .split(',')
        .filter(|id| !id.is_empty() && !id.eq_ignore_ascii_case(removed))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    for id in current_without.split(',').filter(|id| !id.is_empty()) {
        if !ids.iter().any(|existing| existing.eq_ignore_ascii_case(id)) {
            ids.push(id.to_owned());
        }
    }
    (!ids.is_empty()).then_some(ids.join(","))
}

fn artifact_versions(state: &MigrationState) -> Vec<MigrationArtifact> {
    if !state.artifacts.is_empty() {
        return state.artifacts.clone();
    }
    if state.kernel_version.is_empty() {
        return Vec::new();
    }
    vec![MigrationArtifact {
        kernel_version: state.kernel_version.clone(),
        bls_filename: state.bls_filename.clone(),
    }]
}

fn remove_artifacts(state: &MigrationState) -> Result<()> {
    let esp = Path::new(&state.esp_path);
    for artifact in artifact_versions(state) {
        let entry = esp.join("loader/entries").join(&artifact.bls_filename);
        match fs::remove_file(&entry) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", entry.display())),
        }
        let kernel_dir = esp.join(&state.entry_token).join(&artifact.kernel_version);
        match fs::remove_dir_all(&kernel_dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| format!("removing {}", kernel_dir.display()));
            }
        }
    }
    Ok(())
}

fn remove_created_loader_files(state: &MigrationState) -> Result<()> {
    let esp = Path::new(&state.esp_path);
    for relative in &state.created_loader_files {
        let path = esp.join(relative);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", path.display())),
        }
    }
    Ok(())
}

fn restore_optional(path: &Path, contents: Option<&str>) -> Result<()> {
    match contents {
        Some(contents) => fs::write(path, contents),
        None => match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        },
    }
    .with_context(|| format!("restoring {}", path.display()))
}

fn update_loader_default(esp_path: &Path, bls_filename: &str) -> Result<()> {
    let path = esp_path.join("loader/loader.conf");
    let default_id = bls_filename.trim_end_matches(".conf");
    let contents = read_optional(&path)?.unwrap_or_default();
    let mut found = false;
    let mut updated = String::new();
    for line in contents.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("default ") {
            updated.push_str(&line[..line.len() - trimmed.len()]);
            updated.push_str("default ");
            updated.push_str(default_id);
            found = true;
        } else {
            updated.push_str(line);
        }
        updated.push('\n');
    }
    if !found {
        updated = format!("default {default_id}\n{contents}");
    }
    fs::write(&path, updated).with_context(|| format!("updating {}", path.display()))
}

/// Undo the staged migration.  New NVRAM entries are deleted; a pre-existing
/// systemd-boot entry is retained.  Any entries created by a later kernel
/// update are included in `state.artifacts` and are removed as well.
pub fn run_undo(state_path: &Path) -> Result<()> {
    let Some(state) = load_state(state_path)? else {
        println!("No migrate-bootloader state found — nothing to undo.");
        return Ok(());
    };
    let output = read_efibootmgr()?;

    let current_next = parse_boot_next(&output);
    if current_next
        .as_deref()
        .is_some_and(|id| id.eq_ignore_ascii_case(&state.boot_id))
    {
        delete_boot_next()?;
        if let Some(original_next) = &state.original_boot_next {
            set_boot_next(original_next)?;
        }
    } else if let Some(original_next) = &state.original_boot_next
        && current_next.as_deref() != Some(original_next.as_str())
    {
        set_boot_next(original_next)?;
    }

    if let Some(current_order) = parse_boot_order(&output)
        && let Some(restored) = restore_order(
            state.original_boot_order.as_deref(),
            &current_order,
            &state.boot_id,
        )
        && restored != current_order
    {
        set_boot_order(&restored)?;
    }

    if state.nvram_entry_created {
        run_efibootmgr(
            &["-b", &state.boot_id, "-B"],
            &format!("-b {} -B", state.boot_id),
        )?;
    }

    remove_artifacts(&state)?;
    remove_created_loader_files(&state)?;
    let esp = Path::new(&state.esp_path);
    restore_optional(
        &esp.join("loader/loader.conf"),
        state.loader_conf_before.as_deref(),
    )?;
    restore_optional(Path::new(RESYNC_HOOK_PATH), state.hook_before.as_deref())?;
    // The hook's binary copy goes with the hook, unless the hook restored
    // above still runs it.
    if !state
        .hook_before
        .as_deref()
        .is_some_and(|h| h.contains(HOOK_BINARY_DIR))
        && let Err(e) = fs::remove_dir_all(HOOK_BINARY_DIR)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        return Err(e).with_context(|| format!("removing {HOOK_BINARY_DIR}"));
    }
    fs::remove_file(state_path).with_context(|| format!("removing {}", state_path.display()))?;

    println!("migrate-bootloader undone: ESP entries, resync hook, and trial NVRAM state removed.");
    Ok(())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn resync_hook_script(binary: &str, entry_token: &str, title: &str) -> String {
    format!(
        "#!/bin/sh\n\
# Installed by `bootc-migrate migrate-bootloader`; do not edit by hand.\n\
set -eu\n\
COMMAND=\"${{1:-}}\"\n\
KERNEL_VERSION=\"${{2:-}}\"\n\
KERNEL_IMAGE=\"${{4:-}}\"\n\
INITRD=\"${{5:-}}\"\n\
[ \"$COMMAND\" = add ] || exit 0\n\
[ -n \"$KERNEL_VERSION\" ] || exit 0\n\
exec {} migrate-bootloader --resync --entry-token {} --title {} \\\n    --kernel-version \"$KERNEL_VERSION\" \\\n    --kernel-image \"$KERNEL_IMAGE\" \\\n    --initrd \"$INITRD\"\n",
        shell_quote(binary),
        shell_quote(entry_token),
        shell_quote(title),
    )
}

/// Copy the running binary into `dir` for the resync hook to run, and return
/// its path. The copy is written beside its final name and renamed into
/// place, so a hook firing during the copy never runs a partial binary.
fn persist_hook_binary(dir: &Path) -> Result<std::path::PathBuf> {
    let exe = std::env::current_exe().context("locating the running binary for the resync hook")?;
    let name = exe
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("the running binary {} has no file name", exe.display()))?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let dest = dir.join(name);
    if exe == dest {
        return Ok(dest);
    }
    let tmp = dir.join(format!(".{}.tmp", name.to_string_lossy()));
    fs::copy(&exe, &tmp)
        .with_context(|| format!("copying {} to {}", exe.display(), tmp.display()))?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))
        .with_context(|| format!("chmod +x {}", tmp.display()))?;
    fs::rename(&tmp, &dest)
        .with_context(|| format!("renaming {} to {}", tmp.display(), dest.display()))?;
    Ok(dest)
}

/// Install the executable kernel-install drop-in, with its own copy of the
/// running binary under [`HOOK_BINARY_DIR`].
pub fn install_resync_hook(entry_token: &str, title: &str) -> Result<()> {
    let binary = persist_hook_binary(Path::new(HOOK_BINARY_DIR))?;
    let path = Path::new(RESYNC_HOOK_PATH);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::write(
        path,
        resync_hook_script(&binary.to_string_lossy(), entry_token, title),
    )
    .with_context(|| format!("writing {}", path.display()))?;
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).with_context(|| format!("chmod +x {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn state_round_trips_through_json() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("state.json");
        let state = MigrationState {
            entry_token: "abc123".into(),
            esp_path: "/boot/efi".into(),
            bls_filename: "bootc-migrate-abc123-6.8.0.conf".into(),
            boot_id: "0007".into(),
            kernel_version: "6.8.0".into(),
            title: "Bluefin".into(),
            strip_composefs: true,
            artifacts: vec![MigrationArtifact {
                kernel_version: "6.8.0".into(),
                bls_filename: "bootc-migrate-abc123-6.8.0.conf".into(),
            }],
            original_boot_order: Some("0000,0001".into()),
            original_boot_next: None,
            nvram_entry_created: true,
            created_loader_files: vec!["EFI/systemd/systemd-bootx64.efi".into()],
            loader_conf_before: Some("timeout 5\n".into()),
            hook_before: None,
        };
        save_state(&path, &state).unwrap();
        assert_eq!(load_state(&path).unwrap(), Some(state));
    }

    #[test]
    fn missing_state_is_none() {
        let tmp = tempdir().unwrap();
        assert_eq!(load_state(&tmp.path().join("missing.json")).unwrap(), None);
    }

    #[test]
    fn populate_esp_kernel_copies_both_files() {
        let tmp = tempdir().unwrap();
        let esp = tmp.path().join("esp");
        let kernel = tmp.path().join("vmlinuz");
        let initrd = tmp.path().join("initrd");
        fs::write(&kernel, b"kernel").unwrap();
        fs::write(&initrd, b"initrd").unwrap();

        populate_esp_kernel(&esp, "machine", "6.8.0", &kernel, &initrd).unwrap();
        assert_eq!(
            fs::read(esp.join("machine/6.8.0/linux")).unwrap(),
            b"kernel"
        );
        assert_eq!(
            fs::read(esp.join("machine/6.8.0/initrd")).unwrap(),
            b"initrd"
        );
    }

    #[test]
    fn resync_updates_loader_default_and_keeps_settings() {
        let tmp = tempdir().unwrap();
        let loader = tmp.path().join("loader/loader.conf");
        fs::create_dir_all(loader.parent().unwrap()).unwrap();
        fs::write(&loader, "timeout 3\ndefault old-entry\nconsole-mode keep\n").unwrap();

        update_loader_default(tmp.path(), "bootc-migrate-machine-6.9.0.conf").unwrap();

        assert_eq!(
            fs::read_to_string(loader).unwrap(),
            "timeout 3\ndefault bootc-migrate-machine-6.9.0\nconsole-mode keep\n"
        );
    }

    #[test]
    fn order_restore_removes_trial_and_preserves_new_entries() {
        assert_eq!(
            restore_order(Some("0001,0002"), "0007,0002,0003", "0007"),
            Some("0001,0002,0003".into())
        );
        assert_eq!(order_without("0007,0002", "0007"), "0002");
    }

    #[test]
    fn resync_hook_is_add_only_and_uses_kernel_install_paths() {
        let script = resync_hook_script(
            "/var/lib/bootc-migrate/bin/bootc-rebase",
            "machine-id",
            "Bluefin Linux",
        );
        assert!(script.starts_with("#!/bin/sh"));
        assert!(script.contains("[ \"$COMMAND\" = add ] || exit 0"));
        assert!(script.contains("KERNEL_IMAGE=\"${4:-}\""));
        assert!(script.contains("INITRD=\"${5:-}\""));
        assert!(script.contains("--kernel-version \"$KERNEL_VERSION\""));
        assert!(script.contains("--kernel-image \"$KERNEL_IMAGE\""));
        // An absolute path: the hook must not depend on PATH.
        assert!(script.contains(
            "exec '/var/lib/bootc-migrate/bin/bootc-rebase' migrate-bootloader --resync"
        ));
        assert!(script.contains("'Bluefin Linux'"));
    }

    #[test]
    fn hook_binary_is_an_executable_copy_of_the_running_binary() {
        let dir = tempdir().unwrap();
        let bin = dir.path().join("bin");
        let dest = persist_hook_binary(&bin).unwrap();
        let exe = std::env::current_exe().unwrap();
        assert_eq!(dest, bin.join(exe.file_name().unwrap()));
        assert_eq!(fs::read(&dest).unwrap(), fs::read(&exe).unwrap());
        assert_eq!(
            fs::metadata(&dest).unwrap().permissions().mode() & 0o777,
            0o755
        );
        // No temporary file is left behind, and a second install replaces it.
        persist_hook_binary(&bin).unwrap();
        assert_eq!(fs::read_dir(&bin).unwrap().count(), 1);
    }

    #[test]
    fn parse_boot_next_rejects_malformed_values() {
        assert_eq!(parse_boot_next("BootNext: 0007\n"), Some("0007".into()));
        assert_eq!(parse_boot_next("BootNext: nope\n"), None);
        assert_eq!(parse_boot_next("BootOrder: 0007\n"), None);
    }

    #[test]
    fn build_entry_respects_composefs_stripping_policy() {
        let stripped = build_entry(
            "Bluefin",
            "6.8.0",
            "machine",
            "root=UUID=x composefs=deadbeef rw",
            "entry.conf",
            "bootc-migrate-0",
            true,
        );
        assert_eq!(stripped.options, "root=UUID=x rw");
        let kept = build_entry(
            "Dakota",
            "6.8.0",
            "machine",
            "root=UUID=x composefs=deadbeef rw",
            "entry.conf",
            "bootc-migrate-0",
            false,
        );
        assert_eq!(kept.options, "root=UUID=x composefs=deadbeef rw");
    }

    #[test]
    fn entry_token_rejects_path_traversal_and_control_characters() {
        assert!(validate_entry_token("machine-id").is_ok());
        assert!(validate_entry_token("../outside").is_err());
        assert!(validate_entry_token("machine\nid").is_err());
    }

    #[test]
    fn shell_quote_handles_single_quotes() {
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }
}
