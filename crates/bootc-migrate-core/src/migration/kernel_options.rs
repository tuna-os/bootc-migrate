use anyhow::{Context, Result};
use std::fs;
use std::process::Command;

/// Build kernel command-line options for the ComposeFS boot entry.
///
/// Reads the current /proc/cmdline, strips OSTree-specific and incompatible
/// arguments, and appends the composefs= digest.
///
/// # Filtered arguments
/// - `ostree=*` — OSTree deployment locator
/// - `BOOT_IMAGE=*` — GRUB internal
/// - `initrd=*` — GRUB internal
/// - `rd.systemd.unit=*` — stay out of initrd overrides
///
/// `root=` and `rootflags=` are deliberately retained. ComposeFS replaces the
/// deployed root, but its object store and deployment state still live on the
/// physical sysroot selected by those arguments. In particular, Anaconda-style
/// Btrfs installs such as this Bazzite layout can put the sysroot in a named
/// `root` subvolume while leaving the filesystem default at ID 5. Dropping
/// `rootflags=subvol=root` makes the target initramfs mount ID 5 and then fail
/// to find `/sysroot/composefs/images/<digest>`.
pub fn get_kernel_options(composefs_digest: &str) -> Result<String> {
    let cmdline = fs::read_to_string("/proc/cmdline").context("failed to read /proc/cmdline")?;
    let mut options: Vec<String> = Vec::new();
    for word in cmdline.split_whitespace() {
        if should_filter(word) {
            continue;
        }
        // Strip quiet/rhgb so emergency-mode console output is visible.
        if word == "quiet" || word == "rhgb" {
            continue;
        }
        options.push(word.to_string());
    }

    // Activate every LVM logical volume that backs a mounted filesystem.
    //
    // The source OSTree cmdline typically lists only the root LV
    // (`rd.lvm.lv=<vg>/root`). Non-root LVs — most importantly a dedicated
    // `/var` volume — auto-activate post-switchroot on the source distro (udev
    // event activation / lvm2-monitor), so they never appear on the cmdline. The
    // composefs target image may lack that auto-activation path, so its
    // generated `var.mount` (which waits on `blockdev@…<uuid>.target`) never gets
    // its device and `/var` silently falls back to the empty per-deployment var —
    // losing the user's home, flatpaks, etc. Emitting `rd.lvm.lv` for each such
    // LV makes the initrd activate it, so the device exists before the mount runs.
    for arg in discover_lvm_kernel_args() {
        if !options.contains(&arg) {
            options.push(arg);
        }
    }

    // bootc assembles composefs /var from /sysroot/state/os/default/var. If
    // the source has a dedicated filesystem or direct Btrfs subvolume there,
    // mount it at that path in the initrd before bootc-root-setup runs.
    //
    // This must live in the BLS command line, not only in a rebuilt initrd:
    // composefs upgrades copy the current command line while replacing the
    // image-provided initrd, so the mount remains effective on future kernels.
    if let Some(var) = super::detect_separate_var()? {
        let mount_arg = super::stateroot_var_mount_kernel_arg(&var);
        let already_present = options.iter().any(|option| {
            option.starts_with("rd.systemd.mount-extra=")
                && option.contains(":/sysroot/state/os/default/var:")
        });
        if !already_present {
            println!(
                "[phase5] preserving dedicated /var through composefs upgrades ({}, UUID={}, options={})",
                var.fstype, var.uuid, var.options
            );
            options.push(mount_arg);
        }
    }

    // composefs= gets the bare hex digest (no sha512: prefix).
    // SPECIFICATION.md §3.4 and §4.2 examples use the bare hex form.
    let bare_hex = crate::VerityDigest::from_prefixed_or_hex(composefs_digest);
    options.push(format!("composefs={}", bare_hex.as_hex()));
    // In CI (GitHub Actions sets CI=true) forward the journal to the serial
    // console so that QEMU serial logs capture systemd boot diagnostics for
    // E2E failure analysis. Never emitted on user machines.
    for arg in ci_debug_kernel_args(std::env::var_os("CI").is_some()) {
        options.push(arg);
    }
    Ok(options.join(" "))
}

/// Returns systemd console-journal args when running in CI; empty otherwise.
///
/// Factored out so the logic is testable without mutating the process
/// environment (which would require `unsafe` in Rust 2024).
fn ci_debug_kernel_args(ci: bool) -> Vec<String> {
    if ci {
        vec![
            "systemd.log_level=debug".into(),
            "systemd.log_target=console".into(),
            "systemd.journald.forward_to_console=1".into(),
        ]
    } else {
        vec![]
    }
}

fn should_filter(word: &str) -> bool {
    if word.starts_with("ostree=")
        || word.starts_with("BOOT_IMAGE=")
        || word.starts_with("initrd=")
        || word.starts_with("rd.systemd.unit=")
    {
        return true;
    }
    // Also filter anything starting with "ostree." — belt and suspenders.
    if word.starts_with("ostree.") {
        return true;
    }
    false
}

/// Return `rd.lvm.lv=<vg>/<lv>` for every LVM logical volume currently backing a
/// mounted filesystem. Best-effort: returns an empty vec on non-LVM systems or
/// if `findmnt`/`lvs` are unavailable.
fn discover_lvm_kernel_args() -> Vec<String> {
    let sources = match Command::new("findmnt")
        .args(["-rn", "-o", "SOURCE"])
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => return Vec::new(),
    };

    let mut seen: Vec<String> = Vec::new();
    let mut args: Vec<String> = Vec::new();
    for line in sources.lines() {
        let dev = strip_mount_source(line);
        if dev.is_empty() || seen.iter().any(|s| s == dev) {
            continue;
        }
        seen.push(dev.to_string());
        // `lvs <device>` succeeds only for real LVM logical volumes; for anything
        // else (plain partitions, tmpfs, overlay…) it exits non-zero — free filter.
        let out = Command::new("lvs")
            .args(["--noheadings", "-o", "vg_name,lv_name", dev])
            .output();
        if let Ok(o) = out
            && o.status.success()
            && let Some(arg) = parse_lvs_vg_lv(&String::from_utf8_lossy(&o.stdout))
            && !args.contains(&arg)
        {
            args.push(arg);
        }
    }
    args
}

/// Strip a `findmnt` SOURCE value down to the backing device path, dropping any
/// `[/subvol]`/bind suffix (e.g. `/dev/mapper/vg-var[/lib/containers]` →
/// `/dev/mapper/vg-var`).
fn strip_mount_source(source: &str) -> &str {
    let s = source.trim();
    match s.find('[') {
        Some(i) => &s[..i],
        None => s,
    }
}

/// Parse `lvs --noheadings -o vg_name,lv_name` output (e.g. `  vg0 var`) into a
/// `rd.lvm.lv=vg0/var` argument. Returns None if the line isn't a vg/lv pair.
fn parse_lvs_vg_lv(output: &str) -> Option<String> {
    let line = output.lines().next()?.trim();
    let mut it = line.split_whitespace();
    let vg = it.next()?;
    let lv = it.next()?;
    if vg.is_empty() || lv.is_empty() || it.next().is_some() {
        return None;
    }
    Some(format!("rd.lvm.lv={vg}/{lv}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // TDD tests for kernel option filtering.

    /// Helper that simulates get_kernel_options without reading /proc/cmdline.
    fn build_options(cmdline: &str, hex_digest: &str) -> String {
        let mut options: Vec<String> = Vec::new();
        for word in cmdline.split_whitespace() {
            if should_filter(word) {
                continue;
            }
            options.push(word.to_string());
        }
        // digest must be bare hex (no sha512: prefix) for composefs=
        options.push(format!("composefs={hex_digest}"));
        options.join(" ")
    }

    #[test]
    fn composefs_arg_is_bare_hex_not_prefixed() {
        let result = build_options("root=UUID=xxx quiet", "abc123");
        assert!(result.contains("composefs=abc123"));
        assert!(!result.contains("sha512:"));
    }

    #[test]
    fn filters_ostree_arg() {
        let result = build_options(
            "root=UUID=xxx quiet ostree=/ostree/boot.1/fedora/abc/0",
            "ab01cd23ef45",
        );
        assert!(!result.contains("ostree="));
    }

    #[test]
    fn filters_boot_image_arg() {
        let result = build_options(
            "BOOT_IMAGE=/vmlinuz-x.y root=UUID=xxx quiet",
            "ab01cd23ef45",
        );
        assert!(!result.contains("BOOT_IMAGE"));
    }

    #[test]
    fn filters_initrd_arg() {
        let result = build_options(
            "initrd=/initramfs-x.y.img root=UUID=xxx quiet",
            "ab01cd23ef45",
        );
        assert!(!result.contains("initrd="));
    }

    #[test]
    fn preserves_root_arg() {
        // root= is needed alongside composefs= per SPECIFICATION.md §4.2.
        let result = build_options("root=UUID=aaaa-bbbb quiet rw", "ab01cd23ef45");
        assert!(result.contains("root=UUID=aaaa-bbbb"));
        assert!(result.contains("quiet"));
    }

    #[test]
    fn preserves_sysroot_mount_flags() {
        // ComposeFS metadata remains on the physical sysroot, so every form of
        // rootflags must reach the target initramfs. The named-subvolume cases
        // reproduce Anaconda/Bazzite layouts whose Btrfs default remains ID 5.
        let cases = [
            "rootflags=subvol=root",
            "rootflags=subvol=/root",
            "rootflags=subvolid=256",
            "rootflags=subvol=root,compress=zstd:1",
            "rootflags=defaults",
        ];
        for rootflags in cases {
            let result = build_options(&format!("{rootflags} quiet rw"), "ab01cd23ef45");
            assert!(
                result.contains(rootflags),
                "{rootflags} must be retained in: {result}"
            );
        }
    }

    #[test]
    fn preserves_rd_luks_args() {
        // Migrating an encrypted system (e.g. Bluefin installed with LUKS) must
        // carry the rd.luks.* unlock args from the source GRUB cmdline onto the
        // new systemd-boot/composefs BLS entry, or the post-migration boot loses
        // the ability to unlock root. These must NOT be filtered.
        let cmdline = "root=UUID=aaaa rd.luks.name=1234-5678=root \
                       rd.luks.uuid=1234-5678 rd.luks.options=discard \
                       rd.luks.key=/keys/luks.key quiet";
        let result = build_options(cmdline, "ab01cd23ef45");
        assert!(result.contains("rd.luks.name=1234-5678=root"));
        assert!(result.contains("rd.luks.uuid=1234-5678"));
        assert!(result.contains("rd.luks.options=discard"));
        assert!(result.contains("rd.luks.key=/keys/luks.key"));
    }

    #[test]
    fn filters_rd_systemd_unit_override() {
        let result = build_options(
            "rd.systemd.unit=ostree-prepare-root.service quiet",
            "ab01cd23ef45",
        );
        assert!(!result.contains("rd.systemd.unit="));
        assert!(result.contains("quiet"));
    }

    #[test]
    fn filters_ostree_dot_prefix_args() {
        let result = build_options("ostree.booted=1 ostree.composefs=0 quiet", "ab01cd23ef45");
        assert!(!result.contains("ostree."));
        assert!(result.contains("quiet"));
    }

    #[test]
    fn preserves_console_args() {
        let result = build_options("console=ttyS0,115200n8 console=tty0 quiet", "ab01cd23ef45");
        assert!(result.contains("console=ttyS0,115200n8"));
        assert!(result.contains("console=tty0"));
    }

    #[test]
    fn preserves_rw_and_quiet() {
        let result = build_options("rw quiet", "ab01cd23ef45");
        assert!(result.contains("rw"));
        assert!(result.contains("quiet"));
    }

    #[test]
    fn strip_mount_source_drops_subvol_suffix() {
        assert_eq!(
            strip_mount_source("/dev/mapper/bluefin_bluefin--lts-var[/lib/containers/storage]"),
            "/dev/mapper/bluefin_bluefin--lts-var"
        );
        assert_eq!(
            strip_mount_source("  /dev/mapper/vg-root  "),
            "/dev/mapper/vg-root"
        );
        assert_eq!(strip_mount_source("tmpfs"), "tmpfs");
    }

    #[test]
    fn parse_lvs_vg_lv_extracts_pair() {
        assert_eq!(
            parse_lvs_vg_lv("  bluefin_bluefin-lts var\n").as_deref(),
            Some("rd.lvm.lv=bluefin_bluefin-lts/var")
        );
        assert_eq!(
            parse_lvs_vg_lv("vg0 root").as_deref(),
            Some("rd.lvm.lv=vg0/root")
        );
    }

    #[test]
    fn parse_lvs_vg_lv_rejects_non_pairs() {
        // Non-LVM `lvs` output is empty; a single token or extra columns are invalid.
        assert_eq!(parse_lvs_vg_lv(""), None);
        assert_eq!(parse_lvs_vg_lv("   "), None);
        assert_eq!(parse_lvs_vg_lv("onlyone"), None);
        assert_eq!(parse_lvs_vg_lv("vg lv extra"), None);
    }

    /// Representative Bluefin cmdline (simulated from a real bootc OSTree system).
    #[test]
    fn representative_bluefin_cmdline() {
        let cmdline = concat!(
            "BOOT_IMAGE=/boot/vmlinuz-6.11.4-301.fc41.x86_64 ",
            "root=UUID=abcd-1234 ",
            "rootflags=subvol=root ",
            "rw quiet ",
            "console=ttyS0,115200n8 console=tty0 ",
            "ostree=/ostree/boot.1/fedora/abc123def456/0 ",
            "rd.systemd.unit=ostree-prepare-root.service ",
            "ostree.booted=1 ",
            "ostree.composefs=0"
        );
        let result = build_options(cmdline, "ab01cd23ef45");
        // Must not contain:
        assert!(!result.contains("ostree="));
        assert!(!result.contains("ostree."));
        assert!(!result.contains("BOOT_IMAGE"));
        assert!(!result.contains("rd.systemd.unit="));
        // Must contain (root= is preserved per spec §4.2):
        assert!(result.contains("root=UUID=abcd-1234"));
        assert!(result.contains("rootflags=subvol=root"));
        assert!(result.contains("rw"));
        assert!(result.contains("quiet"));
        assert!(result.contains("console=ttyS0"));
        assert!(result.contains("composefs=ab01cd23ef45"));
        // composefs arg must NOT have sha512: prefix
        assert!(!result.contains("sha512:"));
    }

    /// Debug kernel args are emitted when ci=true and suppressed otherwise.
    /// Calls the pure helper directly — no env manipulation needed.
    #[test]
    fn ci_debug_args_gated_by_flag() {
        // (ci_flag, expect_debug)
        let cases: &[(bool, bool)] = &[(true, true), (false, false)];
        for &(ci, expect_debug) in cases {
            let args = super::ci_debug_kernel_args(ci);
            if expect_debug {
                assert!(
                    args.iter().any(|a| a == "systemd.log_level=debug"),
                    "ci={ci}: expected debug args, got {args:?}"
                );
                assert!(args.iter().any(|a| a == "systemd.log_target=console"));
                assert!(
                    args.iter()
                        .any(|a| a == "systemd.journald.forward_to_console=1")
                );
            } else {
                assert!(
                    args.is_empty(),
                    "ci={ci}: expected no debug args, got {args:?}"
                );
            }
        }
    }
}
