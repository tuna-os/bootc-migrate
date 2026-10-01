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
//! follow; the run says so.

use anyhow::{Context, Result, anyhow, bail};
use std::fs;
use std::path::{Path, PathBuf};

use crate::migration::bootloader::BlsEntry;

/// ESP directory that holds the copied kernels, one subdirectory per
/// deployment.
pub const ESP_KERNEL_DIR: &str = "EFI/Linux/bootc-migrate-ostree";
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

/// Plan the ESP-resident entry for `deployment` from its generated BLS
/// entry. The options are kept verbatim: the `ostree=` argument is how the
/// initrd finds the deployment, and the root/LUKS arguments are how it
/// unlocks the root.
pub fn esp_plan(src: &BlsSource, deployment: &str) -> EspPlan {
    let dir = format!("{ESP_KERNEL_DIR}/{deployment}");
    let file_name = |p: &str| p.rsplit('/').next().unwrap_or(p).to_string();
    let mut copies = vec![(
        src.linux.clone(),
        format!("{dir}/{}", file_name(&src.linux)),
    )];
    for i in &src.initrds {
        copies.push((i.clone(), format!("{dir}/{}", file_name(i))));
    }
    let title = if src.title.is_empty() {
        "OSTree".to_string()
    } else {
        src.title.clone()
    };
    EspPlan {
        entry: BlsEntry {
            title: format!("{title} (ESP)"),
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

/// Copy the new deployment's kernel and initrd to the ESP, write the BLS
/// entry and make it systemd-boot's default. Returns the entry's ESP path.
pub fn stage(physical_root: &Path, esp: &Path, deployment: &str) -> Result<String> {
    if !esp.join(SYSTEMD_BOOT_EFI).is_file() {
        bail!("no systemd-boot on the ESP ({SYSTEMD_BOOT_EFI})");
    }
    let src_path = newest_ostree_entry(physical_root)?;
    let src = parse_bls(&fs::read_to_string(&src_path)?)
        .with_context(|| format!("parsing {}", src_path.display()))?;
    println!("[esp] source entry: {}", src_path.display());
    let plan = esp_plan(&src, deployment);

    // One deployment at a time: a re-run replaces the earlier copy.
    let kernel_dir = esp.join(ESP_KERNEL_DIR);
    if kernel_dir.is_dir() {
        fs::remove_dir_all(&kernel_dir)
            .with_context(|| format!("removing {}", kernel_dir.display()))?;
    }
    for (from, to) in &plan.copies {
        let from = resolve_boot_path(physical_root, from);
        let to = esp.join(to);
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&from, &to)
            .with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
        println!("[esp] {} -> {}", from.display(), to.display());
    }

    let entries = esp.join("loader/entries");
    fs::create_dir_all(&entries)?;
    let entry_path = entries.join(&plan.entry.filename);
    fs::write(&entry_path, plan.entry.render())
        .with_context(|| format!("writing {}", entry_path.display()))?;

    let loader_conf = esp.join("loader/loader.conf");
    let current = fs::read_to_string(&loader_conf).unwrap_or_default();
    fs::write(
        &loader_conf,
        with_loader_default(&current, &plan.entry.filename),
    )
    .with_context(|| format!("writing {}", loader_conf.display()))?;
    println!(
        "[esp] {} is systemd-boot's default entry",
        entry_path.display()
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
        let plan = esp_plan(&src, "e3b0.0");
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
        assert!(rendered.contains("title Fedora Linux 44 (Utah) (ostree:0) (ESP)"));
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
    }
}
