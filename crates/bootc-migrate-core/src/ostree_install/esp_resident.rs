//! `/boot` on an encrypted root: the ESP-resident boot path (#305).
//!
//! # The problem
//!
//! A composefs host installed without a separate `/boot` partition keeps
//! its boot files on the ESP and its root on LUKS. Alongside mode then puts
//! the new OSTree kernels and BLS entries under `/boot` on that encrypted
//! root, and bootupd writes its usual GRUB stub: `EFI/fedora/grub.cfg`
//! searches for the root filesystem's UUID. GRUB cannot unlock a LUKS2
//! volume (argon2id key derivation), the search finds nothing and the
//! first boot stops at a bare GRUB shell, with GRUB first in `BootOrder`.
//!
//! # The fix
//!
//! The composefs host already boots through systemd-boot from the ESP,
//! and that loader is restored beside GRUB (see the parent module). So on
//! this layout the new deployment's kernel and initrd are copied to the
//! ESP, a BLS Type-1 entry with the `ostree=` argument from the generated
//! entry is written next to them, the entry becomes systemd-boot's
//! default, and "Linux Boot Manager" goes first in `BootOrder`. The initrd
//! unlocks the root as it does for the composefs deployment. If a step
//! fails, systemd-boot still boots the composefs deployment: a safe
//! failure, not a GRUB shell.
//!
//! The ESP copy is a snapshot of one deployment. `bootc upgrade` writes
//! later kernels under the encrypted `/boot`, which this entry does not
//! follow. Nothing on the host refreshes it, so the entry's title names
//! the deployment it boots and says it is not updated, and the run warns,
//! so an upgraded host is never left booting the old kernel unnoticed.
//!
//! The copy itself never leaves the ESP without a bootable entry: the new
//! kernel and initrd go to a staging directory first and are renamed into
//! a directory no entry points at yet; the entry and `loader.conf` are
//! replaced by rename; older copies are removed only after that.

use anyhow::{Context, Result, anyhow, bail};
use std::fs;
use std::path::{Path, PathBuf};

use crate::migration::bootloader::BlsEntry;

/// ESP directory that holds the copied kernels, one subdirectory per
/// deployment.
pub const ESP_KERNEL_DIR: &str = "EFI/Linux/bootc-migrate-ostree";
/// Staging directory under [`ESP_KERNEL_DIR`] for a copy in progress.
/// systemd-boot never reads it; a leftover from an interrupted run is
/// removed by the next one.
pub const ESP_STAGING_DIR: &str = ".staging";
/// File name of the BLS entry under `loader/entries` on the ESP.
pub const ESP_ENTRY_FILE: &str = "bootc-migrate-ostree.conf";
/// The systemd-boot binary the composefs host boots through.
pub const SYSTEMD_BOOT_EFI: &str = "EFI/systemd/systemd-bootx64.efi";

// ---- Pure planning -------------------------------------------------------

/// Whether a device-mapper UUID (`/sys/class/block/dm-N/dm/uuid`) belongs
/// to a dm-crypt mapping. cryptsetup names them `CRYPT-LUKS2-…`,
/// `CRYPT-LUKS1-…` or `CRYPT-PLAIN-…`.
pub fn is_crypt_dm_uuid(uuid: &str) -> bool {
    uuid.trim().starts_with("CRYPT-")
}

/// The source device of the last mount of `target` in a `/proc/mounts`
/// listing (the last one is the one in effect).
pub fn mount_source(mounts: &str, target: &str) -> Option<String> {
    mounts.lines().rev().find_map(|l| {
        let mut f = l.split_whitespace();
        let src = f.next()?;
        (f.next()? == target).then(|| src.to_string())
    })
}

/// Whether the new deployment's kernels land on an encrypted root that the
/// GRUB stub cannot read: the bind plan gives `/boot` no partition of its
/// own, and the root is dm-crypt (from sysfs) or the cmdline unlocks LUKS.
pub fn boot_on_encrypted_root(
    bind_plan: &[(String, String)],
    root_is_crypt: bool,
    cmdline: &str,
) -> bool {
    let boot_separate = bind_plan.iter().any(|(_, rel)| rel == "boot");
    let cmdline_luks = cmdline
        .split_whitespace()
        .any(|w| w.starts_with("rd.luks.") || w.starts_with("luks."));
    !boot_separate && (root_is_crypt || cmdline_luks)
}

/// The fields of a BLS Type-1 entry that the ESP copy needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlsSource {
    pub title: String,
    pub version: String,
    pub linux: String,
    pub initrds: Vec<String>,
    pub options: String,
}

/// Parse a BLS entry as bootc/ostree writes it. `linux` and `options` are
/// required; an entry without `ostree=` is not an OSTree deployment's.
pub fn parse_bls(text: &str) -> Result<BlsSource> {
    let mut src = BlsSource {
        title: String::new(),
        version: String::new(),
        linux: String::new(),
        initrds: Vec::new(),
        options: String::new(),
    };
    for line in text.lines() {
        let line = line.trim();
        let Some((key, value)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let value = value.trim().to_string();
        match key {
            "title" => src.title = value,
            "version" => src.version = value,
            "linux" => src.linux = value,
            "initrd" => src.initrds.push(value),
            "options" => src.options = value,
            _ => {}
        }
    }
    if src.linux.is_empty() {
        bail!("BLS entry has no `linux` line");
    }
    if !src
        .options
        .split_whitespace()
        .any(|w| w.starts_with("ostree="))
    {
        bail!("BLS entry has no `ostree=` argument");
    }
    Ok(src)
}

/// Where a path from a BLS entry under the physical root's `/boot` is on
/// disk. Without a boot partition, ostree writes it with a `/boot` prefix;
/// with one, relative to that partition. Both resolve under `/boot`.
pub fn resolve_boot_path(physical_root: &Path, bls_path: &str) -> PathBuf {
    let rel = bls_path.trim_start_matches('/');
    let rel = rel.strip_prefix("boot/").unwrap_or(rel);
    physical_root.join("boot").join(rel)
}

/// The ESP entry and the files to copy for it.
#[derive(Debug)]
pub struct EspPlan {
    pub entry: BlsEntry,
    /// `(path as in the source entry, ESP-relative destination)`.
    pub copies: Vec<(String, String)>,
}

/// The deployment id as the entry title shows it: the first 12 hex digits
/// of the checksum and the serial (`e3b0c44298fc.0`).
pub fn short_deployment(deployment: &str) -> String {
    match deployment.split_once('.') {
        Some((csum, serial)) if csum.len() > 12 => {
            format!("{}.{serial}", csum.get(..12).unwrap_or(csum))
        }
        _ => deployment.to_string(),
    }
}

/// The ESP entry's title. It names the deployment the copied kernel
/// belongs to and says the copy is not updated, so a host that ran
/// `bootc upgrade` since does not boot the old kernel unnoticed.
pub fn esp_entry_title(src_title: &str, deployment: &str) -> String {
    let title = if src_title.is_empty() {
        "OSTree"
    } else {
        src_title
    };
    format!(
        "{title} (ESP copy of {}, not updated by bootc upgrade)",
        short_deployment(deployment)
    )
}

/// The name under [`ESP_KERNEL_DIR`] for a new copy of `deployment`: the
/// deployment id, or the first free `<id>-N` while that name is in use (a
/// re-run for the same deployment), so the copy never overwrites files
/// the current entry points at.
pub fn pick_copy_dir(deployment: &str, in_use: impl Fn(&str) -> bool) -> String {
    std::iter::once(deployment.to_string())
        .chain((1..).map(|n| format!("{deployment}-{n}")))
        .find(|name| !in_use(name))
        .expect("an unbounded sequence of names has a free one")
}

/// Plan the ESP-resident entry for `deployment` from its generated BLS
/// entry, with the files under `ESP_KERNEL_DIR/<copy_dir>`. The options
/// are kept verbatim: the `ostree=` argument is how the initrd finds the
/// deployment, and the root/LUKS arguments are how it unlocks the root.
pub fn esp_plan(src: &BlsSource, deployment: &str, copy_dir: &str) -> EspPlan {
    let dir = format!("{ESP_KERNEL_DIR}/{copy_dir}");
    let file_name = |p: &str| p.rsplit('/').next().unwrap_or(p).to_string();
    let mut copies = vec![(
        src.linux.clone(),
        format!("{dir}/{}", file_name(&src.linux)),
    )];
    for i in &src.initrds {
        copies.push((i.clone(), format!("{dir}/{}", file_name(i))));
    }
    EspPlan {
        entry: BlsEntry {
            title: esp_entry_title(&src.title, deployment),
            version: src.version.clone(),
            linux: format!("/{}", copies[0].1),
            initrds: copies[1..].iter().map(|(_, d)| format!("/{d}")).collect(),
            options: src.options.clone(),
            filename: ESP_ENTRY_FILE.to_string(),
            sort_key: "bootc-migrate-ostree".to_string(),
        },
        copies,
    }
}

/// `loader.conf` with `default` set to `entry_file`: the existing `default`
/// line is replaced, or one is added first. Every other line is kept.
pub fn with_loader_default(loader_conf: &str, entry_file: &str) -> String {
    let mut out = vec![format!("default {entry_file}")];
    out.extend(
        loader_conf
            .lines()
            .filter(|l| l.split_whitespace().next() != Some("default"))
            .map(str::to_string),
    );
    let mut s = out.join("\n");
    s.push('\n');
    s
}

/// The firmware entry id of systemd-boot ("Linux Boot Manager") in
/// `efibootmgr` output.
pub fn parse_systemd_boot_entry_id(efibootmgr_output: &str) -> Option<String> {
    efibootmgr_output.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("Boot")?;
        let id = rest.get(..4)?;
        if !id.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let tail = rest[4..].to_ascii_lowercase();
        (tail.contains("linux boot manager") || tail.contains("systemd-bootx64.efi"))
            .then(|| id.to_string())
    })
}

/// The manual workaround, for the cases this module cannot do it itself.
pub fn manual_workaround() -> &'static str {
    "copy the new deployment's kernel and initrd (listed in \
     /sysroot/boot/loader/entries/ostree-*.conf) to the ESP, add a BLS entry under \
     loader/entries on the ESP with that entry's `options` line unchanged (it carries \
     ostree=), and boot it through systemd-boot"
}

// ---- I/O -----------------------------------------------------------------

/// Whether the block device behind `dev` (a path under `/dev`) is dm-crypt
/// or sits on dm-crypt (LVM on LUKS), from sysfs.
fn device_on_crypt(dev: &str) -> bool {
    let Ok(real) = fs::canonicalize(dev) else {
        return false;
    };
    let Some(name) = real.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        return false;
    };
    let mut queue = vec![name];
    let mut seen = 0;
    while let Some(name) = queue.pop() {
        seen += 1;
        if seen > 32 {
            break;
        }
        let base = Path::new("/sys/class/block").join(&name);
        if fs::read_to_string(base.join("dm/uuid")).is_ok_and(|u| is_crypt_dm_uuid(&u)) {
            return true;
        }
        if let Ok(slaves) = fs::read_dir(base.join("slaves")) {
            queue.extend(
                slaves
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned()),
            );
        }
    }
    false
}

/// Whether this host needs the ESP-resident path, given the boot bind
/// plan ([`super::boot_bind_plan`]) of its boot mounts.
pub fn detect(bind_plan: &[(String, String)], physical_root: &str, cmdline: &str) -> bool {
    let mounts = fs::read_to_string("/proc/mounts").unwrap_or_default();
    let root_is_crypt = mount_source(&mounts, physical_root)
        .or_else(|| mount_source(&mounts, "/"))
        .is_some_and(|dev| dev.starts_with("/dev/") && device_on_crypt(&dev));
    boot_on_encrypted_root(bind_plan, root_is_crypt, cmdline)
}

/// The newest `ostree-*.conf` entry under the physical root's `/boot`.
fn newest_ostree_entry(physical_root: &Path) -> Result<PathBuf> {
    let dir = physical_root.join("boot/loader/entries");
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for e in fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        if !(name.starts_with("ostree-") && name.ends_with(".conf")) {
            continue;
        }
        let mtime = e.metadata()?.modified()?;
        if best.as_ref().is_none_or(|(t, _)| mtime > *t) {
            best = Some((mtime, e.path()));
        }
    }
    best.map(|(_, p)| p)
        .ok_or_else(|| anyhow!("no ostree-*.conf entry under {}", dir.display()))
}

/// Write `contents` to `path` through a sibling temporary file and a
/// rename, so `path` holds the old or the new contents, never a part.
fn replace_file(path: &Path, contents: &str) -> Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let write = || -> std::io::Result<()> {
        let mut f = fs::File::create(&tmp)?;
        std::io::Write::write_all(&mut f, contents.as_bytes())?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    };
    write().with_context(|| format!("writing {}", path.display()))
}

/// Copy `copies` (source entry path, ESP-relative destination) into
/// `staging`, flattened to their file names.
fn copy_into(physical_root: &Path, staging: &Path, copies: &[(String, String)]) -> Result<()> {
    fs::create_dir_all(staging).with_context(|| format!("creating {}", staging.display()))?;
    for (from, to) in copies {
        let from = resolve_boot_path(physical_root, from);
        let name = Path::new(to)
            .file_name()
            .ok_or_else(|| anyhow!("no file name in {to}"))?;
        let dest = staging.join(name);
        fs::copy(&from, &dest)
            .with_context(|| format!("copying {} to {}", from.display(), dest.display()))?;
        fs::File::open(&dest)
            .and_then(|f| f.sync_all())
            .with_context(|| format!("syncing {}", dest.display()))?;
        println!("[esp] {} -> {}", from.display(), dest.display());
    }
    Ok(())
}

/// Copy the new deployment's kernel and initrd to the ESP, write the BLS
/// entry and make it systemd-boot's default. Returns the entry's ESP path.
///
/// Every step before the entry is replaced leaves the earlier entry and
/// its files untouched, and the copies of earlier runs are removed only
/// once the new entry is the default, so a failure part way never leaves
/// the default entry pointing at missing files.
pub fn stage(physical_root: &Path, esp: &Path, deployment: &str) -> Result<String> {
    if !esp.join(SYSTEMD_BOOT_EFI).is_file() {
        bail!("no systemd-boot on the ESP ({SYSTEMD_BOOT_EFI})");
    }
    let src_path = newest_ostree_entry(physical_root)?;
    let src = parse_bls(&fs::read_to_string(&src_path)?)
        .with_context(|| format!("parsing {}", src_path.display()))?;
    println!("[esp] source entry: {}", src_path.display());

    let kernel_dir = esp.join(ESP_KERNEL_DIR);
    let copy_dir = pick_copy_dir(deployment, |name| kernel_dir.join(name).exists());
    let plan = esp_plan(&src, deployment, &copy_dir);

    // Copy into a staging directory, then rename it into place under a
    // name no entry points at yet.
    let staging = kernel_dir.join(ESP_STAGING_DIR);
    if staging.exists() {
        fs::remove_dir_all(&staging)
            .with_context(|| format!("removing the leftover {}", staging.display()))?;
    }
    if let Err(e) = copy_into(physical_root, &staging, &plan.copies) {
        let _ = fs::remove_dir_all(&staging);
        return Err(e);
    }
    let final_dir = kernel_dir.join(&copy_dir);
    if let Err(e) = fs::rename(&staging, &final_dir) {
        let _ = fs::remove_dir_all(&staging);
        return Err(e)
            .with_context(|| format!("renaming {} to {}", staging.display(), final_dir.display()));
    }

    let entries = esp.join("loader/entries");
    fs::create_dir_all(&entries)?;
    let entry_path = entries.join(&plan.entry.filename);
    replace_file(&entry_path, &plan.entry.render())?;

    let loader_conf = esp.join("loader/loader.conf");
    let current = fs::read_to_string(&loader_conf).unwrap_or_default();
    replace_file(
        &loader_conf,
        &with_loader_default(&current, &plan.entry.filename),
    )?;
    println!(
        "[esp] {} is systemd-boot's default entry",
        entry_path.display()
    );

    // The new entry is in place; earlier copies are no longer referenced.
    if let Ok(dir) = fs::read_dir(&kernel_dir) {
        for e in dir.flatten() {
            if e.file_name().to_string_lossy() == copy_dir {
                continue;
            }
            if let Err(err) = fs::remove_dir_all(e.path()) {
                eprintln!(
                    "Warning: could not remove the earlier ESP copy {} ({err}).",
                    e.path().display()
                );
            }
        }
    }

    eprintln!(
        "Warning: the default systemd-boot entry \"{}\" boots a copy of this \
         deployment's kernel and initrd on the ESP. `bootc upgrade` does not update it: \
         after an upgrade it keeps booting this deployment's kernel, or fails once the \
         deployment is pruned. Refresh the copy after each upgrade (#305).",
        plan.entry.title
    );
    Ok(entry_path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const OSTREE_ENTRY: &str = "\
title Fedora Linux 44 (Utah) (ostree:0)
version 1
options root=UUID=abcd rw rd.luks.uuid=luks-1234 ostree=/ostree/boot.1/default/e3b0/0
linux /boot/ostree/default-e3b0/vmlinuz-6.17.1-300.fc44.x86_64
initrd /boot/ostree/default-e3b0/initramfs-6.17.1-300.fc44.x86_64.img
aboot /ostree/deploy/default/deploy/e3b0.0/usr/lib/ostree-boot/aboot.img
";

    #[test]
    fn crypt_dm_uuid_table() {
        for (uuid, want) in [
            ("CRYPT-LUKS2-0123456789abcdef-luks-0123\n", true),
            ("CRYPT-LUKS1-0123-root", true),
            ("CRYPT-PLAIN-swap", true),
            ("LVM-abcdef", false),
            ("", false),
        ] {
            assert_eq!(is_crypt_dm_uuid(uuid), want, "{uuid:?}");
        }
    }

    #[test]
    fn mount_source_takes_the_mount_in_effect() {
        let mounts = "\
/dev/vda3 /sysroot ext4 rw 0 0
/dev/mapper/luks-1234 /sysroot btrfs rw,subvol=/root 0 0
composefs / overlay ro 0 0
";
        assert_eq!(
            mount_source(mounts, "/sysroot").as_deref(),
            Some("/dev/mapper/luks-1234")
        );
        assert_eq!(mount_source(mounts, "/").as_deref(), Some("composefs"));
        assert_eq!(mount_source(mounts, "/boot"), None);
    }

    #[test]
    fn boot_on_encrypted_root_table() {
        let plan = |v: &[(&str, &str)]| -> Vec<(String, String)> {
            v.iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect()
        };
        let esp_at_boot = plan(&[("/boot", "boot/efi")]);
        let boot_partition = plan(&[("/boot", "boot"), ("/boot/efi", "boot/efi")]);
        let luks = "root=UUID=x rd.luks.uuid=luks-1 rw";
        let plain = "root=UUID=x rw";
        // (bind plan, root on dm-crypt, cmdline, expected)
        for (bind, crypt, cmdline, want) in [
            // The #305 host: ESP at /boot, LUKS root.
            (&esp_at_boot, false, luks, true),
            (&esp_at_boot, true, plain, true),
            // A boot partition outside LUKS: GRUB reads it.
            (&boot_partition, true, luks, false),
            // No encryption.
            (&esp_at_boot, false, plain, false),
        ] {
            assert_eq!(
                boot_on_encrypted_root(bind, crypt, cmdline),
                want,
                "{bind:?} crypt={crypt} {cmdline}"
            );
        }
    }

    #[test]
    fn parse_bls_reads_an_ostree_entry() {
        let src = parse_bls(OSTREE_ENTRY).unwrap();
        assert_eq!(src.title, "Fedora Linux 44 (Utah) (ostree:0)");
        assert_eq!(
            src.linux,
            "/boot/ostree/default-e3b0/vmlinuz-6.17.1-300.fc44.x86_64"
        );
        assert_eq!(src.initrds.len(), 1);
        assert!(src.options.contains("ostree=/ostree/boot.1/default/e3b0/0"));
    }

    #[test]
    fn parse_bls_refuses_non_ostree_entries() {
        assert!(parse_bls("title x\noptions composefs=abc\nlinux /k\n").is_err());
        assert!(parse_bls("title x\noptions ostree=/o\n").is_err());
    }

    #[test]
    fn resolve_boot_path_table() {
        let root = Path::new("/sysroot");
        for (bls, want) in [
            // /boot on the root filesystem: ostree writes the /boot prefix.
            (
                "/boot/ostree/default-1/vmlinuz",
                "/sysroot/boot/ostree/default-1/vmlinuz",
            ),
            // Separate /boot partition: paths are relative to it.
            (
                "/ostree/default-1/vmlinuz",
                "/sysroot/boot/ostree/default-1/vmlinuz",
            ),
        ] {
            assert_eq!(resolve_boot_path(root, bls), PathBuf::from(want), "{bls}");
        }
    }

    #[test]
    fn esp_plan_keeps_options_verbatim() {
        let src = parse_bls(OSTREE_ENTRY).unwrap();
        let plan = esp_plan(&src, "e3b0.0", "e3b0.0");
        assert_eq!(plan.entry.options, src.options);
        assert_eq!(
            plan.entry.linux,
            "/EFI/Linux/bootc-migrate-ostree/e3b0.0/vmlinuz-6.17.1-300.fc44.x86_64"
        );
        assert_eq!(
            plan.entry.initrds,
            vec!["/EFI/Linux/bootc-migrate-ostree/e3b0.0/initramfs-6.17.1-300.fc44.x86_64.img"]
        );
        assert_eq!(plan.copies.len(), 2);
        assert_eq!(plan.copies[0].0, src.linux);
        assert_eq!(plan.entry.filename, ESP_ENTRY_FILE);
        let rendered = plan.entry.render();
        assert!(rendered.contains("ostree=/ostree/boot.1/default/e3b0/0"));
        assert!(rendered.contains(
            "title Fedora Linux 44 (Utah) (ostree:0) (ESP copy of e3b0.0, not updated by bootc upgrade)"
        ));
        // A re-run for the same deployment copies to another directory.
        let rerun = esp_plan(&src, "e3b0.0", "e3b0.0-1");
        assert_eq!(
            rerun.entry.linux,
            "/EFI/Linux/bootc-migrate-ostree/e3b0.0-1/vmlinuz-6.17.1-300.fc44.x86_64"
        );
        assert_eq!(rerun.entry.title, plan.entry.title);
    }

    #[test]
    fn short_deployment_table() {
        for (dep, want) in [
            (
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855.0",
                "e3b0c44298fc.0",
            ),
            ("e3b0.0", "e3b0.0"),
            ("noserial", "noserial"),
        ] {
            assert_eq!(short_deployment(dep), want, "{dep}");
        }
    }

    #[test]
    fn esp_entry_title_names_the_deployment_and_staleness() {
        assert_eq!(
            esp_entry_title("", "e3b0.1"),
            "OSTree (ESP copy of e3b0.1, not updated by bootc upgrade)"
        );
        assert!(esp_entry_title("Fedora", "e3b0.0").starts_with("Fedora (ESP copy of e3b0.0,"));
    }

    #[test]
    fn pick_copy_dir_table() {
        // (names in use, expected)
        for (used, want) in [
            (&[][..], "d.0"),
            (&["d.0"][..], "d.0-1"),
            (&["d.0", "d.0-1"][..], "d.0-2"),
            (&["d.0-1"][..], "d.0"),
        ] {
            assert_eq!(
                pick_copy_dir("d.0", |n| used.contains(&n)),
                want,
                "{used:?}"
            );
        }
    }

    #[test]
    fn with_loader_default_table() {
        for (conf, want) in [
            ("", "default bootc-migrate-ostree.conf\n"),
            (
                "timeout 3\ndefault bootc_composefs-*\n",
                "default bootc-migrate-ostree.conf\ntimeout 3\n",
            ),
            (
                "#comment\nconsole-mode keep\n",
                "default bootc-migrate-ostree.conf\n#comment\nconsole-mode keep\n",
            ),
        ] {
            assert_eq!(with_loader_default(conf, ESP_ENTRY_FILE), want, "{conf:?}");
        }
    }

    #[test]
    fn systemd_boot_entry_id_table() {
        let out = "\
BootCurrent: 0001
BootOrder: 0003,0001,0000
Boot0000* UiApp\tFvVol(...)
Boot0001* Linux Boot Manager\tHD(1,GPT,...)/File(\\EFI\\systemd\\systemd-bootx64.efi)
Boot0003* Fedora\tHD(1,GPT,...)/File(\\EFI\\fedora\\shimx64.efi)
";
        assert_eq!(parse_systemd_boot_entry_id(out).as_deref(), Some("0001"));
        assert_eq!(parse_systemd_boot_entry_id("BootOrder: 0003\n"), None);
    }

    #[test]
    fn stage_writes_entry_kernel_and_default() {
        let root = tempfile::tempdir().unwrap();
        let esp = tempfile::tempdir().unwrap();
        let boot = root.path().join("boot");
        fs::create_dir_all(boot.join("loader/entries")).unwrap();
        fs::create_dir_all(boot.join("ostree/default-e3b0")).unwrap();
        fs::write(boot.join("loader/entries/ostree-1.conf"), OSTREE_ENTRY).unwrap();
        fs::write(
            boot.join("ostree/default-e3b0/vmlinuz-6.17.1-300.fc44.x86_64"),
            "kernel",
        )
        .unwrap();
        fs::write(
            boot.join("ostree/default-e3b0/initramfs-6.17.1-300.fc44.x86_64.img"),
            "initrd",
        )
        .unwrap();

        // No systemd-boot on the ESP: nothing to boot the entry with.
        assert!(stage(root.path(), esp.path(), "e3b0.0").is_err());

        fs::create_dir_all(esp.path().join("EFI/systemd")).unwrap();
        fs::write(esp.path().join(SYSTEMD_BOOT_EFI), "sd-boot").unwrap();
        fs::create_dir_all(esp.path().join("loader")).unwrap();
        fs::write(
            esp.path().join("loader/loader.conf"),
            "default bootc_composefs-*\ntimeout 5\n",
        )
        .unwrap();
        // A stale copy from an earlier run is replaced.
        fs::create_dir_all(esp.path().join(ESP_KERNEL_DIR).join("old.0")).unwrap();

        stage(root.path(), esp.path(), "e3b0.0").unwrap();
        let dir = esp.path().join(ESP_KERNEL_DIR);
        assert!(!dir.join("old.0").exists());
        assert!(!dir.join(ESP_STAGING_DIR).exists());
        assert_eq!(
            fs::read_to_string(dir.join("e3b0.0/vmlinuz-6.17.1-300.fc44.x86_64")).unwrap(),
            "kernel"
        );
        let entry =
            fs::read_to_string(esp.path().join("loader/entries").join(ESP_ENTRY_FILE)).unwrap();
        assert!(entry.contains("ostree=/ostree/boot.1/default/e3b0/0"));
        assert_eq!(
            fs::read_to_string(esp.path().join("loader/loader.conf")).unwrap(),
            "default bootc-migrate-ostree.conf\ntimeout 5\n"
        );
        assert!(!esp.path().join("loader/loader.conf.tmp").exists());

        // A re-run for the same deployment never overwrites the files the
        // current entry points at: it copies beside them, switches the
        // entry, then removes the earlier copy.
        stage(root.path(), esp.path(), "e3b0.0").unwrap();
        let entry =
            fs::read_to_string(esp.path().join("loader/entries").join(ESP_ENTRY_FILE)).unwrap();
        assert!(entry.contains("/EFI/Linux/bootc-migrate-ostree/e3b0.0-1/vmlinuz"));
        assert!(
            dir.join("e3b0.0-1/vmlinuz-6.17.1-300.fc44.x86_64")
                .is_file()
        );
        assert!(!dir.join("e3b0.0").exists());
    }

    #[test]
    fn stage_failure_keeps_the_earlier_entry_bootable() {
        let root = tempfile::tempdir().unwrap();
        let esp = tempfile::tempdir().unwrap();
        let boot = root.path().join("boot");
        fs::create_dir_all(boot.join("loader/entries")).unwrap();
        fs::create_dir_all(boot.join("ostree/default-e3b0")).unwrap();
        fs::write(boot.join("loader/entries/ostree-1.conf"), OSTREE_ENTRY).unwrap();
        // The kernel is there, the initrd is not: the copy fails part way.
        fs::write(
            boot.join("ostree/default-e3b0/vmlinuz-6.17.1-300.fc44.x86_64"),
            "kernel",
        )
        .unwrap();
        fs::create_dir_all(esp.path().join("EFI/systemd")).unwrap();
        fs::write(esp.path().join(SYSTEMD_BOOT_EFI), "sd-boot").unwrap();
        // The entry and copy of an earlier run.
        let dir = esp.path().join(ESP_KERNEL_DIR);
        fs::create_dir_all(dir.join("old.0")).unwrap();
        fs::write(dir.join("old.0/vmlinuz"), "old kernel").unwrap();
        fs::create_dir_all(esp.path().join("loader/entries")).unwrap();
        let old_entry = "title old\nlinux /EFI/Linux/bootc-migrate-ostree/old.0/vmlinuz\n";
        fs::write(
            esp.path().join("loader/entries").join(ESP_ENTRY_FILE),
            old_entry,
        )
        .unwrap();
        fs::write(
            esp.path().join("loader/loader.conf"),
            "default bootc-migrate-ostree.conf\n",
        )
        .unwrap();

        assert!(stage(root.path(), esp.path(), "e3b0.0").is_err());
        assert_eq!(
            fs::read_to_string(dir.join("old.0/vmlinuz")).unwrap(),
            "old kernel"
        );
        assert_eq!(
            fs::read_to_string(esp.path().join("loader/entries").join(ESP_ENTRY_FILE)).unwrap(),
            old_entry
        );
        assert!(!dir.join(ESP_STAGING_DIR).exists());
        assert!(!dir.join("e3b0.0").exists());
    }
}
