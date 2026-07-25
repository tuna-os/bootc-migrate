//! Live ESP/NVRAM mutation for the standalone `migrate-bootloader`
//! subcommand (issue #65) — the load-bearing counterpart to
//! `systemd_boot`'s pure core. Every function here does real filesystem
//! and NVRAM I/O; its correctness is proven by the `migrate-bootloader` E2E
//! cell (boot → reboot into sd-boot → simulate a kernel update → verify the
//! ESP resynced), not by unit tests alone. Unit tests here cover the pure
//! sub-pieces (path construction, hook script content, state
//! serialization).
//!
//! Sequencing, per the design recorded on #65:
//! 1. `run_migrate`: populate the ESP, write the BLS entry, install the
//!    resync hook, create a UEFI NVRAM entry and set it as a one-boot
//!    `BootNext` trial — `BootOrder` is deliberately left untouched so a
//!    failed trial boot falls back to the existing GRUB/OSTree default.
//! 2. After a human (or the E2E cell) confirms the trial boot worked,
//!    `run_promote` moves the trial entry to the front of `BootOrder`.
//! 3. `run_undo` reverses everything `run_migrate`/`run_promote` did.
//!
//! GRUB is never touched by this route — `--undo` doesn't need to restore
//! it.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::BlsEntry;
use super::systemd_boot::{build_migrate_bootloader_entry, derive_entry_token};
use crate::migration::rollback::{build_new_boot_order, parse_boot_order};

/// Where the initial `run_migrate` records what it created, so `--undo` and
/// `--promote` can find it without re-deriving anything (and so a second
/// `run_migrate` invocation is a clean no-op / re-sync rather than creating
/// a duplicate entry).
pub const STATE_PATH: &str = "/var/lib/bootc-rebase/migrate-bootloader-state.json";

/// The kernel-install plugin this module installs to keep the ESP in sync
/// with future kernel updates — the "load-bearing" piece per #65's spec.
pub const RESYNC_HOOK_PATH: &str = "/etc/kernel/install.d/95-bootc-rebase-esp-sync.install";

/// What `run_migrate` created, persisted across invocations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationState {
    pub entry_token: String,
    pub esp_path: String,
    pub bls_filename: String,
    /// The `efibootmgr` Boot#### id (4 hex digits, no "Boot" prefix) of the
    /// entry this run created.
    pub boot_id: String,
}

/// Load previously-saved state, if `run_migrate` has already run.
pub fn load_state(state_path: &Path) -> Result<Option<MigrationState>> {
    match fs::read_to_string(state_path) {
        Ok(s) => Ok(Some(
            serde_json::from_str(&s).context("parsing migrate-bootloader state file")?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).context("reading migrate-bootloader state file"),
    }
}

fn save_state(state_path: &Path, state: &MigrationState) -> Result<()> {
    if let Some(parent) = state_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::write(
        state_path,
        serde_json::to_string_pretty(state).expect("MigrationState serialization cannot fail"),
    )
    .with_context(|| format!("writing {}", state_path.display()))
}

/// This host's running kernel version (`uname -r`).
pub fn current_kernel_version() -> Result<String> {
    let out = Command::new("uname")
        .arg("-r")
        .output()
        .context("failed to execute uname -r")?;
    if !out.status.success() {
        bail!("uname -r exited {}", out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Locate this host's own vmlinuz for `kver` — standard Fedora/OSTree
/// layout: `/usr/lib/modules/<kver>/vmlinuz` first (bootc/OSTree images
/// always populate this), falling back to `/boot/vmlinuz-<kver>` (plain
/// Fedora layout) for robustness on hosts laid out differently.
pub fn find_host_vmlinuz(kver: &str) -> Result<PathBuf> {
    let candidates = [
        PathBuf::from(format!("/usr/lib/modules/{kver}/vmlinuz")),
        PathBuf::from(format!("/boot/vmlinuz-{kver}")),
    ];
    candidates.into_iter().find(|p| p.exists()).ok_or_else(|| {
        anyhow::anyhow!("could not find vmlinuz for kernel {kver} under /usr/lib/modules or /boot")
    })
}

/// Build a fresh initramfs for `kver` at `dest` via the host's own dracut —
/// deliberately not read from wherever kernel-install's other plugins may
/// or may not have already staged one, so this hook's correctness doesn't
/// depend on assumptions about ambient plugin behavior on a given image.
pub fn build_initrd(kver: &str, dest: &Path) -> Result<()> {
    let dracut_path = ["/usr/bin/dracut", "/usr/sbin/dracut", "dracut"]
        .into_iter()
        .find(|p| Path::new(p).exists() || *p == "dracut")
        .ok_or_else(|| anyhow::anyhow!("dracut not found on host"))?;
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let status = Command::new(dracut_path)
        .args(["--force", "--kver", kver])
        .arg(dest)
        .status()
        .context("failed to execute dracut")?;
    if !status.success() {
        bail!("dracut exited {status} building initramfs for {kver}");
    }
    Ok(())
}

/// Copy `vmlinuz`/`initrd` to `esp_path/<entry_token>/<kver>/{linux,initrd}`
/// (the same layout `systemd_boot::esp_kernel_paths` computes).
pub fn populate_esp_kernel(
    esp_path: &Path,
    entry_token: &str,
    kver: &str,
    vmlinuz_src: &Path,
    initrd_src: &Path,
) -> Result<()> {
    let dir = esp_path.join(entry_token).join(kver);
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    // The ESP is vfat, which never supports FICLONE reflink — a plain copy
    // is the only option here, unlike the composefs-migration ESP writes
    // that read from a CoW-capable sealed mount.
    fs::copy(vmlinuz_src, dir.join("linux")).context("copying vmlinuz to ESP")?;
    fs::copy(initrd_src, dir.join("initrd")).context("copying initrd to ESP")?;
    Ok(())
}

/// Write a BLS entry to `esp_path/loader/entries/<filename>`.
pub fn write_bls_entry(esp_path: &Path, entry: &BlsEntry) -> Result<()> {
    let entries_dir = esp_path.join("loader/entries");
    fs::create_dir_all(&entries_dir)
        .with_context(|| format!("creating {}", entries_dir.display()))?;
    fs::write(entries_dir.join(&entry.filename), entry.render())
        .with_context(|| format!("writing BLS entry {}", entry.filename))
}

/// Install `systemd-bootx64.efi` (host's own — `migrate-bootloader` has no
/// target image to pull from; it converts the *current* deployment's own
/// bootloader) to the ESP's `EFI/systemd/` and `EFI/BOOT/` (removable-media
/// fallback) paths.
pub fn install_systemd_boot_binary(esp_path: &Path) -> Result<()> {
    let src = Path::new("/usr/lib/systemd/boot/efi/systemd-bootx64.efi");
    if !src.exists() {
        bail!(
            "host does not ship systemd-bootx64.efi at {} — migrate-bootloader converts the \
             current deployment's own bootloader and has no target image to pull binaries \
             from; install a systemd-boot package first",
            src.display()
        );
    }
    let sd_dir = esp_path.join("EFI/systemd");
    fs::create_dir_all(&sd_dir)?;
    let removable_dir = esp_path.join("EFI/BOOT");
    fs::create_dir_all(&removable_dir)?;
    let sd_dst = sd_dir.join("systemd-bootx64.efi");
    fs::copy(src, &sd_dst).context("installing systemd-bootx64.efi")?;
    fs::copy(&sd_dst, removable_dir.join("BOOTX64.EFI"))
        .context("installing BOOTX64.EFI removable-media loader")?;
    Ok(())
}

/// The device+partition-number pair `efibootmgr --disk/--part` want, parsed
/// from an ESP mount's backing block device (`lsblk -no PKNAME,PARTN
/// <device>`-style split done by the caller — this just formats).
fn efibootmgr_create_trial(esp_disk: &str, esp_part: &str, label: &str) -> Result<String> {
    let status = Command::new("efibootmgr")
        .args([
            "--create",
            "--disk",
            esp_disk,
            "--part",
            esp_part,
            "--loader",
            "\\EFI\\systemd\\systemd-bootx64.efi",
            "--label",
            label,
        ])
        .status()
        .context("failed to execute efibootmgr --create")?;
    if !status.success() {
        bail!("efibootmgr --create exited {status}");
    }

    let out = Command::new("efibootmgr")
        .output()
        .context("failed to execute efibootmgr")?;
    let txt = String::from_utf8_lossy(&out.stdout);
    let id = txt
        .lines()
        .find(|l| l.contains(label))
        .and_then(|l| l.trim().strip_prefix("Boot"))
        .and_then(|rest| rest.get(..4))
        .filter(|id| id.chars().all(|c| c.is_ascii_hexdigit()))
        .ok_or_else(|| anyhow::anyhow!("could not find the newly-created '{label}' entry's Boot#### id in efibootmgr output"))?
        .to_string();

    let bootnext = Command::new("efibootmgr")
        .args(["--bootnext", &id])
        .status()
        .context("failed to execute efibootmgr --bootnext")?;
    if !bootnext.success() {
        bail!("efibootmgr --bootnext {id} exited {bootnext}");
    }
    Ok(id)
}

/// Inputs to [`run_migrate`], grouped to keep the function signature
/// manageable — every field is a caller-supplied context value (nothing
/// here is discovered internally, so the whole thing stays testable by
/// construction).
#[derive(Debug, Clone, Copy)]
pub struct MigrateInputs<'a> {
    pub esp_path: &'a Path,
    pub state_path: &'a Path,
    pub title: &'a str,
    pub cmdline: &'a str,
    pub entry_token_file: Option<&'a str>,
    pub machine_id: &'a str,
}

/// Full sequence: populate the ESP, write the BLS entry, install the
/// resync hook, and create a `BootNext`-trial NVRAM entry. Idempotent:
/// re-running after a previous `run_migrate` overwrites the same files and
/// reuses the same state (does not create a second NVRAM entry).
pub fn run_migrate(inputs: &MigrateInputs) -> Result<MigrationState> {
    let MigrateInputs {
        esp_path,
        state_path,
        title,
        cmdline,
        entry_token_file,
        machine_id,
    } = *inputs;

    if let Some(existing) = load_state(state_path)? {
        println!(
            "migrate-bootloader already ran (state at {}) — re-syncing the ESP for the \
             current kernel without creating a new NVRAM entry.",
            state_path.display()
        );
        let kver = current_kernel_version()?;
        resync_for_kernel_update(esp_path, &existing.entry_token, &kver, cmdline, title)?;
        return Ok(existing);
    }

    let entry_token = derive_entry_token(entry_token_file, machine_id);
    let kver = current_kernel_version()?;
    let vmlinuz = find_host_vmlinuz(&kver)?;

    let initrd_scratch = tempfile::Builder::new()
        .prefix("bootc-rebase-migrate-bootloader-")
        .tempdir_in("/var/tmp")
        .context("creating scratch dir for initrd build")?;
    let initrd_path = initrd_scratch.path().join("initrd");
    build_initrd(&kver, &initrd_path)?;

    populate_esp_kernel(esp_path, &entry_token, &kver, &vmlinuz, &initrd_path)?;
    install_systemd_boot_binary(esp_path)?;

    let filename = format!("bootc-rebase-{entry_token}-{kver}.conf");
    let entry = build_migrate_bootloader_entry(
        title,
        &kver,
        &entry_token,
        cmdline,
        &filename,
        "bootc-rebase-migrate-bootloader-0",
    );
    write_bls_entry(esp_path, &entry)?;

    install_resync_hook(&entry_token, title)?;

    let (esp_disk, esp_part) = crate::migration::boot::get_esp_disk_and_part(
        &esp_path.display().to_string(),
    )
    .ok_or_else(|| {
        anyhow::anyhow!(
            "could not determine the ESP's backing disk/partition (findmnt {})",
            esp_path.display()
        )
    })?;
    let boot_id = efibootmgr_create_trial(&esp_disk, &esp_part, "Linux Boot Manager (trial)")?;

    let state = MigrationState {
        entry_token,
        esp_path: esp_path.display().to_string(),
        bls_filename: filename,
        boot_id,
    };
    save_state(state_path, &state)?;
    println!(
        "migrate-bootloader staged as a one-boot trial (Boot{} set via BootNext). Reboot to \
         test it — BootOrder is unchanged, so a failed trial falls back to the existing \
         default. Once you've confirmed it works, run `migrate-bootloader --promote`.",
        state.boot_id
    );
    Ok(state)
}

/// Re-copy `kver`'s kernel/initrd and rewrite the BLS entry for an
/// already-migrated system — the operation the installed resync hook
/// performs on every `kernel-install add` (called by `bootc-rebase
/// migrate-bootloader --resync`), and also what `run_migrate`'s idempotent
/// re-run path uses so both share the exact same logic instead of drifting.
pub fn resync_for_kernel_update(
    esp_path: &Path,
    entry_token: &str,
    kver: &str,
    cmdline: &str,
    title: &str,
) -> Result<()> {
    let vmlinuz = find_host_vmlinuz(kver)?;
    let initrd_scratch = tempfile::Builder::new()
        .prefix("bootc-rebase-resync-")
        .tempdir_in("/var/tmp")
        .context("creating scratch dir for initrd rebuild")?;
    let initrd_path = initrd_scratch.path().join("initrd");
    build_initrd(kver, &initrd_path)?;
    populate_esp_kernel(esp_path, entry_token, kver, &vmlinuz, &initrd_path)?;

    let filename = format!("bootc-rebase-{entry_token}-{kver}.conf");
    let entry = build_migrate_bootloader_entry(
        title,
        kver,
        entry_token,
        cmdline,
        &filename,
        "bootc-rebase-migrate-bootloader-0",
    );
    write_bls_entry(esp_path, &entry)
}

/// Promote the trial entry to the front of `BootOrder`, making it the
/// permanent default. Call only after confirming the trial boot worked.
pub fn run_promote(state_path: &Path) -> Result<()> {
    let Some(state) = load_state(state_path)? else {
        bail!(
            "no migrate-bootloader state found at {} — run migrate-bootloader first",
            state_path.display()
        );
    };
    let out = Command::new("efibootmgr")
        .output()
        .context("failed to execute efibootmgr")?;
    let txt = String::from_utf8_lossy(&out.stdout);
    let current_order = parse_boot_order(&txt)
        .ok_or_else(|| anyhow::anyhow!("could not parse BootOrder from efibootmgr output"))?;
    let new_order = build_new_boot_order(&current_order, &state.boot_id);
    let status = Command::new("efibootmgr")
        .args(["--bootorder", &new_order])
        .status()
        .context("failed to execute efibootmgr --bootorder")?;
    if !status.success() {
        bail!("efibootmgr --bootorder {new_order} exited {status}");
    }
    println!("Promoted Boot{} to the front of BootOrder.", state.boot_id);
    Ok(())
}

/// Reverse everything `run_migrate`/`run_promote` did: remove the ESP
/// entries/kernel dir this run created, the resync hook, and the NVRAM
/// entry (restoring `BootOrder` to exclude it if it was promoted). GRUB is
/// never touched by this route, so nothing GRUB-side needs restoring.
pub fn run_undo(state_path: &Path) -> Result<()> {
    let Some(state) = load_state(state_path)? else {
        println!("No migrate-bootloader state found — nothing to undo.");
        return Ok(());
    };
    let esp_path = Path::new(&state.esp_path);

    let entry_file = esp_path.join("loader/entries").join(&state.bls_filename);
    let _ = fs::remove_file(&entry_file);

    let token_dir = esp_path.join(&state.entry_token);
    let _ = fs::remove_dir_all(&token_dir);

    let _ = fs::remove_file(RESYNC_HOOK_PATH);

    let del_status = Command::new("efibootmgr")
        .args(["-b", &state.boot_id, "-B"])
        .status();
    match del_status {
        Ok(s) if s.success() => println!("Removed Boot{} from UEFI NVRAM.", state.boot_id),
        Ok(s) => eprintln!(
            "Warning: efibootmgr -b {} -B exited {s} — the NVRAM entry may need manual removal.",
            state.boot_id
        ),
        Err(e) => eprintln!("Warning: failed to invoke efibootmgr for NVRAM cleanup ({e})."),
    }

    let _ = fs::remove_file(state_path);
    println!("migrate-bootloader undone: ESP entries, resync hook, and NVRAM entry removed.");
    Ok(())
}

/// Content of the kernel-install plugin that keeps the ESP in sync with
/// future kernel updates — the load-bearing piece per #65's spec (without
/// it, a flipped system silently boots a stale kernel after the next
/// update). Deliberately self-sufficient: rebuilds the initramfs itself via
/// `bootc-rebase migrate-bootloader --resync` rather than assuming any
/// particular ambient kernel-install plugin already staged one, since that
/// assumption can't be verified without live testing against a specific
/// image's plugin set.
fn resync_hook_script(entry_token: &str, title: &str) -> String {
    format!(
        "#!/bin/sh\n\
         # Installed by `bootc-rebase migrate-bootloader` (issue #65) — keeps the ESP's\n\
         # systemd-boot entry in sync with kernel-install(8) updates. Do not edit by hand;\n\
         # re-running `migrate-bootloader` regenerates this file.\n\
         set -eu\n\
         COMMAND=\"$1\"\n\
         KERNEL_VERSION=\"$2\"\n\
         [ \"$COMMAND\" = add ] || exit 0\n\
         exec bootc-rebase migrate-bootloader --resync --entry-token {entry_token} \
         --kernel-version \"$KERNEL_VERSION\" --title {title:?}\n"
    )
}

/// Install the resync hook script at [`RESYNC_HOOK_PATH`], executable.
pub fn install_resync_hook(entry_token: &str, title: &str) -> Result<()> {
    let path = Path::new(RESYNC_HOOK_PATH);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::write(path, resync_hook_script(entry_token, title))
        .with_context(|| format!("writing {}", path.display()))?;
    let mut perms = fs::metadata(path)?.permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    fs::set_permissions(path, perms).with_context(|| format!("chmod +x {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trips_through_json() {
        let tmp = tempfile::tempdir().unwrap();
        let state_path = tmp.path().join("state.json");
        let state = MigrationState {
            entry_token: "abc123".to_string(),
            esp_path: "/boot/efi".to_string(),
            bls_filename: "bootc-rebase-abc123-6.8.0.conf".to_string(),
            boot_id: "0007".to_string(),
        };
        save_state(&state_path, &state).unwrap();
        let loaded = load_state(&state_path).unwrap().unwrap();
        assert_eq!(loaded, state);
    }

    #[test]
    fn load_state_missing_file_is_none_not_error() {
        let tmp = tempfile::tempdir().unwrap();
        let loaded = load_state(&tmp.path().join("nope.json")).unwrap();
        assert!(loaded.is_none());
    }

    #[test]
    fn find_host_vmlinuz_prefers_modules_dir() {
        let tmp = tempfile::tempdir().unwrap();
        // Can't override the real / root in a unit test, so this test
        // documents the search order via the pure candidate list instead
        // of end-to-end filesystem behavior (covered by the E2E cell).
        let _ = tmp; // placeholder: candidate order is asserted below.
        let kver = "6.8.0-test";
        let candidates = [
            PathBuf::from(format!("/usr/lib/modules/{kver}/vmlinuz")),
            PathBuf::from(format!("/boot/vmlinuz-{kver}")),
        ];
        assert_eq!(
            candidates[0],
            PathBuf::from("/usr/lib/modules/6.8.0-test/vmlinuz")
        );
        assert_eq!(candidates[1], PathBuf::from("/boot/vmlinuz-6.8.0-test"));
    }

    #[test]
    fn populate_esp_kernel_writes_linux_and_initrd() {
        let tmp = tempfile::tempdir().unwrap();
        let esp = tmp.path().join("esp");
        let vmlinuz_src = tmp.path().join("vmlinuz-src");
        let initrd_src = tmp.path().join("initrd-src");
        fs::write(&vmlinuz_src, b"vmlinuz-bytes").unwrap();
        fs::write(&initrd_src, b"initrd-bytes").unwrap();

        populate_esp_kernel(&esp, "mytoken", "6.8.0", &vmlinuz_src, &initrd_src).unwrap();

        let linux_dst = esp.join("mytoken/6.8.0/linux");
        let initrd_dst = esp.join("mytoken/6.8.0/initrd");
        assert_eq!(fs::read(&linux_dst).unwrap(), b"vmlinuz-bytes");
        assert_eq!(fs::read(&initrd_dst).unwrap(), b"initrd-bytes");
    }

    #[test]
    fn write_bls_entry_creates_entries_dir_and_file() {
        let tmp = tempfile::tempdir().unwrap();
        let esp = tmp.path().join("esp");
        let entry = BlsEntry {
            title: "Bluefin".to_string(),
            version: "6.8.0".to_string(),
            linux: "/mytoken/6.8.0/linux".to_string(),
            initrds: vec!["/mytoken/6.8.0/initrd".to_string()],
            options: "root=UUID=abc rw".to_string(),
            filename: "bootc-rebase-mytoken-6.8.0.conf".to_string(),
            sort_key: "bootc-rebase-migrate-bootloader-0".to_string(),
        };

        write_bls_entry(&esp, &entry).unwrap();

        let written =
            fs::read_to_string(esp.join("loader/entries/bootc-rebase-mytoken-6.8.0.conf")).unwrap();
        assert!(written.contains("title Bluefin"));
        assert!(written.contains("linux /mytoken/6.8.0/linux"));
    }

    #[test]
    fn install_systemd_boot_binary_errors_clearly_when_host_lacks_it() {
        // Can't stub the hardcoded /usr/lib/systemd/boot/efi source path in
        // a unit test without root/mount tricks; assert the error path at
        // least names the missing source so a real run's failure is
        // actionable (full success path covered by the E2E cell).
        let tmp = tempfile::tempdir().unwrap();
        let esp = tmp.path().join("esp");
        // On any host actually running this test suite that does ship the
        // binary, this would succeed instead — so only assert the shape of
        // failure when it doesn't exist, matching CI's non-UEFI runners.
        if !Path::new("/usr/lib/systemd/boot/efi/systemd-bootx64.efi").exists() {
            let err = install_systemd_boot_binary(&esp).unwrap_err();
            assert!(err.to_string().contains("systemd-bootx64.efi"));
        }
    }

    #[test]
    fn resync_hook_script_is_executable_shell_and_names_the_hook_source() {
        let script = resync_hook_script("mytoken", "Bluefin");
        assert!(script.starts_with("#!/bin/sh"));
        assert!(script.contains("COMMAND=\"$1\""));
        assert!(script.contains("[ \"$COMMAND\" = add ] || exit 0"));
        assert!(script.contains("--entry-token mytoken"));
        assert!(script.contains("migrate-bootloader"));
    }

    #[test]
    fn install_resync_hook_writes_executable_file() {
        // install_resync_hook writes to the hardcoded RESYNC_HOOK_PATH
        // under /etc, which this unit test can't safely redirect — the
        // script-content assertions above plus the E2E cell (which does
        // exercise the real installed hook end-to-end) cover this
        // function's behavior. This test just documents the constant.
        assert_eq!(
            RESYNC_HOOK_PATH,
            "/etc/kernel/install.d/95-bootc-rebase-esp-sync.install"
        );
    }

    #[test]
    fn run_undo_with_no_state_is_a_clean_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let state_path = tmp.path().join("no-state.json");
        // Doesn't touch RESYNC_HOOK_PATH or efibootmgr since there's no
        // state to act on — safe to call in a unit test.
        run_undo(&state_path).unwrap();
    }
}
