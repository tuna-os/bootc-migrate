//! The Phase 4 `/etc` transition: acquire the target's `/etc`, three-way merge
//! it against the OSTree factory default and the live tree, then apply the
//! cleanup passes that make the result valid on a composefs deployment.
//!
//! [`run`] is the whole pipeline behind one call. `deploy.rs` supplies typed
//! inputs via [`EtcTransition`] and receives an [`EtcTransitionReport`]
//! instead of sequencing the individual cleanup steps itself.
//!
//! Target-specific hardware and NetworkManager policy lives in
//! [`super::target_compat`]; this module only calls into it.

use super::*;

/// Typed inputs for the `/etc` transition.
pub(crate) struct EtcTransition<'a> {
    pub target_image: &'a str,
    pub sealed_config: &'a str,
    pub etc_dir: &'a Path,
    pub overrides: Option<&'a crate::mergetc::EtcDriftManifest>,
}

/// What the transition actually did, for the caller to report on.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EtcTransitionReport {
    /// The three-way merge failed and a flat copy of the live `/etc` was used.
    /// The post-merge cleanup passes are skipped in that case, as they always
    /// have been — they describe the target tree, which a flat copy is not.
    pub fell_back_to_flat_copy: bool,
    /// Host `/` and `/var` mounts dropped from the target's fstab.
    pub removed_host_mounts: usize,
}

impl<'a> EtcTransition<'a> {
    /// Run the full `/etc` transition, returning what it did.
    ///
    /// The merge is best-effort: on failure the live `/etc` is copied flat and
    /// the run continues, matching the long-standing behavior of the inline
    /// pipeline this replaces. fstab sanitization runs either way.
    pub(crate) fn run(&self) -> Result<EtcTransitionReport> {
        let mut report = EtcTransitionReport::default();

        if let Err(e) = perform_etc_merge(
            self.target_image,
            self.sealed_config,
            self.etc_dir,
            self.overrides,
        ) {
            eprintln!("3-way /etc merge failed ({e}), falling back to flat /etc copy.");
            xattr::copy_dir_all_with_xattrs("/etc", self.etc_dir)
                .context("failed to copy /etc (fallback)")?;
            report.fell_back_to_flat_copy = true;
        }

        report.removed_host_mounts = sanitize_composefs_fstab(self.etc_dir)?;
        Ok(report)
    }
}

/// Remove host root mounts that are invalid after composefs assembles `/` and
/// `/var`. Other filesystems such as `/boot`, `/boot/efi`, `/home`, and swap
/// remain untouched.
fn sanitize_composefs_fstab(etc_dir: &Path) -> Result<usize> {
    let path = etc_dir.join("fstab");
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e).with_context(|| format!("failed to read {}", path.display())),
    };

    let mut removed = 0usize;
    let mut filtered = String::with_capacity(contents.len());
    for line in contents.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let mountpoint = if trimmed.is_empty() || trimmed.starts_with('#') {
            None
        } else {
            trimmed.split_whitespace().nth(1)
        };
        if matches!(mountpoint, Some("/" | "/var")) {
            removed += 1;
        } else {
            filtered.push_str(line);
        }
    }

    if removed > 0 {
        fs::write(&path, filtered)
            .with_context(|| format!("failed to sanitize {}", path.display()))?;
    }
    Ok(removed)
}

/// Build a fstab entry for the /var btrfs subvolume by parsing /proc/mounts and
/// resolving the source device to a UUID. Returns None if the data can't be derived.
#[allow(dead_code)]
fn synthesize_var_fstab_entry(mounts: &str) -> Option<String> {
    let var_line = mounts.lines().find(|line| {
        let parts: Vec<&str> = line.split_whitespace().collect();
        parts.len() >= 4 && parts[1] == "/var" && parts[2] == "btrfs"
    })?;
    println!("[phase4] /proc/mounts /var line: {}", var_line);

    let parts: Vec<&str> = var_line.split_whitespace().collect();
    let device = parts[0];
    let raw_opts = parts[3];

    let subvol_token = raw_opts
        .split(',')
        .find(|o| o.starts_with("subvol=") && *o != "subvol=/")
        .or_else(|| raw_opts.split(',').find(|o| o.starts_with("subvolid=")))
        .unwrap_or("subvol=/");

    let uuid = resolve_device_uuid(device);
    let source = uuid
        .map(|u| format!("UUID={}", u))
        .unwrap_or_else(|| device.to_string());

    let opts = format!("rw,relatime,{}", subvol_token);
    Some(format!("{}\t/var\tbtrfs\t{}\t0 0\n", source, opts))
}

#[allow(dead_code)]
fn resolve_device_uuid(device: &str) -> Option<String> {
    let by_uuid = Path::new("/dev/disk/by-uuid");
    let entries = fs::read_dir(by_uuid).ok()?;
    for entry in entries.flatten() {
        let link = fs::read_link(entry.path()).ok()?;
        let resolved = by_uuid.join(&link).canonicalize().ok()?;
        if resolved == Path::new(device) {
            return entry.file_name().to_str().map(|s| s.to_string());
        }
    }
    None
}

/// Perform 3-way /etc merge: old OSTree default, current live /etc, new ComposeFS default.
fn perform_etc_merge(
    target_image: &str,
    sealed_config: &str,
    etc_dir: &Path,
    etc_overrides: Option<&crate::mergetc::EtcDriftManifest>,
) -> Result<()> {
    let temp_mount =
        TempDir::new_in("/var/tmp").context("failed to create temp mount directory")?;
    let mut mount_path = temp_mount.path().to_path_buf();

    // Mount the target rootfs via bootc's composefs overlay using the *sealed
    // config digest* (not the rootfs verity): `cfs oci mount` looks up
    // `streams/oci-config-<sealed-config>`, so the rootfs verity would miss and
    // drop us to a raw EROFS mount that zero-fills file content above the inline
    // threshold. With the sealed digest the overlay exposes real content, so we
    // can read /etc straight off the mount (and validate prune symlink targets).
    //
    // On hosts where the composefs overlay mounts into bootc's private namespace
    // (see phase5_setup_bootloader), the mount is empty here. Fall back to a
    // `podman image mount` of the already-cached image — local, real content, and
    // no dependency on reaching the registry mid-migration.
    let target = super::image_access::open_target(
        target_image,
        sealed_config,
        &mount_path,
        "etc",
        "phase4",
    )?;
    mount_path = target.path().to_path_buf();

    let old_default_etc = find_ostree_etc_default()?;
    let current_etc = Path::new("/etc");

    // Use the target's /etc straight off the mount (real content). The registry
    // stream is kept only as a last-resort fallback for when /etc is somehow
    // absent from both the composefs overlay and the podman mount.
    // (The temp dir is held to function scope so it outlives merge_etc_files.)
    let registry_etc_temp =
        TempDir::new_in("/var/tmp").context("failed to create temp dir for registry /etc")?;
    let registry_etc = registry_etc_temp.path().to_path_buf();
    let mount_etc = mount_path.join("etc");
    let new_default_etc = if mount_etc
        .read_dir()
        .map(|mut d| d.next().is_some())
        .unwrap_or(false)
    {
        println!("[phase4] using mounted /etc for merge source");
        mount_etc
    } else {
        println!("[phase4] /etc absent from mount; streaming target /etc from registry...");
        extract_subtree_via_registry(target_image, "etc/", &registry_etc)
            .context("registry /etc extraction failed")?;
        registry_etc
    };

    crate::mergetc::merge_etc_files_with_overrides(
        &old_default_etc,
        current_etc,
        &new_default_etc,
        etc_dir,
        etc_overrides,
    )
    .context("3-way /etc merge failed")?;

    match super::target_compat::apply_target_gbm_backend_compat(target_image, &mount_path, etc_dir)
    {
        Ok(true) => println!(
            "[phase4] added Mesa's GBM backend directory for hybrid NVIDIA graphics compatibility"
        ),
        Ok(false) => {}
        Err(e) => {
            eprintln!("[phase4] warning: failed to configure split GBM backend paths: {e:#}")
        }
    }

    match apply_legacy_var_home_compat(Path::new("/"), &mount_path, etc_dir) {
        Ok(Some(direction)) => println!(
            "[phase4] added {} compatibility bind for the target's native /home layout",
            direction.description()
        ),
        Ok(None) => {}
        Err(e) => {
            eprintln!("[phase4] warning: failed to configure /var/home compatibility: {e:#}")
        }
    }

    // Drop /etc symlinks whose /usr/* target does not exist in the target image.
    // Bluefin → Dakota: e.g. /etc/systemd/system/dbus.service points to
    // dbus-broker.service which Dakota doesn't ship; the dangling symlink
    // breaks dbus and everything downstream (polkit, logind, sshd).
    match crate::mergetc::prune_dangling_symlinks(etc_dir, &mount_path) {
        Ok(n) if n > 0 => println!("[phase4] pruned {} dangling /etc symlink(s)", n),
        Ok(_) => {}
        Err(e) => eprintln!("[phase4] warning: dangling-symlink prune failed: {e:#}"),
    }

    // Drop OSTree/GRUB-era /etc artifacts that don't belong on a composefs
    // deployment. The 3-way merge keeps these because Bluefin's factory has
    // them and the user didn't modify them, but they actively lie about
    // system state on Dakota.
    drop_ostree_era_etc_artifacts(etc_dir);

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LegacyHomeCompat {
    HomeToVarHome,
    VarHomeToHome,
}

impl LegacyHomeCompat {
    fn description(self) -> &'static str {
        match self {
            Self::HomeToVarHome => "/home → /var/home",
            Self::VarHomeToHome => "/var/home → /home",
        }
    }

    fn fstab_entry(self) -> &'static str {
        match self {
            Self::HomeToVarHome => {
                "/home /var/home none bind,x-systemd.requires-mounts-for=/home 0 0"
            }
            Self::VarHomeToHome => {
                "/var/home /home none bind,x-systemd.requires-mounts-for=/var/home 0 0"
            }
        }
    }
}

fn fstab_home_compat_direction(fstab: &str) -> Option<LegacyHomeCompat> {
    let mut has_home_mount = false;
    let mut has_var_home_mount = false;
    let mut already_bridged = false;

    for line in fstab.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let mut fields = trimmed.split_whitespace();
        let Some(source) = fields.next() else {
            continue;
        };
        let Some(mountpoint) = fields.next() else {
            continue;
        };

        has_home_mount |= mountpoint == "/home";
        has_var_home_mount |= mountpoint == "/var/home";
        already_bridged |= matches!(
            (source, mountpoint),
            ("/home", "/var/home") | ("/var/home", "/home")
        );
    }

    if already_bridged || (has_home_mount && has_var_home_mount) {
        None
    } else if has_home_mount {
        Some(LegacyHomeCompat::HomeToVarHome)
    } else {
        Some(LegacyHomeCompat::VarHomeToHome)
    }
}

/// Preserve both home-directory spellings when an OSTree source uses
/// `/home -> /var/home` but the target image has a native `/home` directory.
///
/// Existing files are not rewritten. A bind mount keeps absolute symlinks,
/// script interpreters, desktop settings, and other persisted paths valid
/// while allowing the target to use its preferred `/home` location. When the
/// source has a dedicated `/home` mount, `/home` is canonical; otherwise the
/// preserved `/var` tree remains canonical.
fn apply_legacy_var_home_compat(
    source_root: &Path,
    target_root: &Path,
    etc_dir: &Path,
) -> Result<Option<LegacyHomeCompat>> {
    let source_home = source_root.join("home");
    let source_var_home = source_root.join("var/home");
    let source_uses_var_home = match (source_home.canonicalize(), source_var_home.canonicalize()) {
        (Ok(home), Ok(var_home)) => home == var_home,
        _ => false,
    };
    if !source_uses_var_home {
        return Ok(None);
    }

    let target_home = target_root.join("home");
    let target_home_metadata = match fs::symlink_metadata(&target_home) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(e).with_context(|| format!("failed to inspect {}", target_home.display()));
        }
    };
    if !target_home_metadata.file_type().is_dir() {
        return Ok(None);
    }

    let fstab_path = etc_dir.join("fstab");
    let mut fstab = match fs::read_to_string(&fstab_path) {
        Ok(fstab) => fstab,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(e).with_context(|| format!("failed to read {}", fstab_path.display()));
        }
    };
    let Some(direction) = fstab_home_compat_direction(&fstab) else {
        return Ok(None);
    };

    if !fstab.is_empty() && !fstab.ends_with('\n') {
        fstab.push('\n');
    }
    fstab.push_str("# Preserve OSTree /var/home paths when the target uses a native /home\n");
    fstab.push_str(direction.fstab_entry());
    fstab.push('\n');
    fs::write(&fstab_path, fstab)
        .with_context(|| format!("failed to update {}", fstab_path.display()))?;

    Ok(Some(direction))
}

/// Compute the "Config Drift Review" diff (issue #15): every path under live
/// `/etc` that differs from the OSTree factory default, categorized as
/// Added/Modified/Removed/TypeChanged. Read-only — does not touch the target
/// image's `/etc` at all, since this is specifically about the user's own
/// drift, independent of migration target.
///
/// This is the computation half of #15's proposal. The interactive checkbox
/// TUI lives in `bootc-migrate`'s `drift_review` module (it needs a real
/// terminal, so it can't live in this crate); its resulting
/// [`crate::mergetc::EtcDriftManifest`] is threaded through
/// [`phase4_stage_deploy`] as `etc_overrides`.
pub fn compute_etc_drift() -> Result<Vec<crate::mergetc::EtcDriftEntry>> {
    let factory_etc = find_ostree_etc_default()?;
    crate::mergetc::diff_etc_factory_vs_live(&factory_etc, Path::new("/etc"))
}

/// Drop GRUB / rpm-ostree / ostree-remount artifacts that don't belong on a composefs +
/// systemd-boot deploy. These come from the source OS's /etc but reference
/// boot/state mechanisms the target no longer uses.
fn drop_ostree_era_etc_artifacts(etc_dir: &Path) {
    // Concrete known-cruft paths. Keep this tight — only paths that are
    // unambiguously misleading (lying state files) or actively wrong for
    // the new bootloader.
    let drops = [
        ".rpm-ostree-shadow-mode-fixed2.stamp",
        ".updated",
        "grub2.cfg",
        "grub2-efi.cfg",
        "grub.d",
        "systemd/system/local-fs.target.wants/ostree-remount.service",
    ];
    for name in &drops {
        let p = etc_dir.join(name);
        let exists = p.exists() || p.is_symlink();
        if !exists {
            continue;
        }
        let res = if p.is_dir() && !p.is_symlink() {
            fs::remove_dir_all(&p)
        } else {
            fs::remove_file(&p)
        };
        match res {
            Ok(()) => println!("[phase4] dropped OSTree-era /etc artifact: {}", name),
            Err(e) => eprintln!("[phase4] warning: failed to drop {}: {}", p.display(), e),
        }
    }
}

/// Legacy single-DB supplement path. Kept for callers that don't want the full
/// `/etc` subtree; not used by `perform_etc_merge` anymore since the full
/// subtree extract subsumes it.
#[allow(dead_code)]
fn supplement_identity_dbs_from_registry(target_image: &str, etc_dir: &Path) -> Result<()> {
    let scratch =
        TempDir::new_in("/var/tmp").context("failed to create temp dir for identity-DB extract")?;
    let scratch_etc = scratch.path().join("etc");
    fs::create_dir_all(&scratch_etc).context("failed to create scratch etc dir")?;

    // Try each file individually; tolerate "missing in image" because not
    // every bootc target ships every identity DB (Dakota has no /etc/subuid
    // or /etc/subgid). Any other error from a given file is logged and the
    // others continue.
    let names = ["passwd", "shadow", "group", "gshadow", "subuid", "subgid"];
    for name in &names {
        let src = PathBuf::from("/etc").join(name);
        let dst = scratch_etc.join(name);
        let pair = [(src.as_path(), dst.as_path())];
        if let Err(e) = extract_files_via_registry(target_image, &pair) {
            let es = format!("{e:#}");
            if es.contains("missing files") || es.contains("No such file") {
                // Image doesn't ship this file; that's fine.
                continue;
            }
            eprintln!("[phase4] warning: skopeo extract of /etc/{name} failed: {es}");
        }
    }

    let mut supplemented = 0usize;
    for name in &names {
        let dakota_path = scratch_etc.join(name);
        let merged_path = etc_dir.join(name);
        if !dakota_path.exists() {
            continue;
        }
        let dakota = fs::read_to_string(&dakota_path).unwrap_or_default();
        if dakota.trim().is_empty() {
            continue;
        }
        let current = fs::read_to_string(&merged_path).unwrap_or_default();
        let merged = line_union_by_first_colon(&current, &dakota);
        if merged != current {
            // Permissions on shadow/gshadow must stay 000; the existing file
            // already has them, so write in place and preserve mode/xattrs.
            let perms = fs::metadata(&merged_path).ok().map(|m| m.permissions());
            fs::write(&merged_path, merged.as_bytes())
                .with_context(|| format!("failed to rewrite {}", merged_path.display()))?;
            if let Some(p) = perms {
                let _ = fs::set_permissions(&merged_path, p);
            }
            supplemented += 1;
        }
    }
    if supplemented > 0 {
        println!(
            "[phase4] supplemented {} identity-DB file(s) with target's system users",
            supplemented
        );
    }
    Ok(())
}

#[allow(dead_code)]
fn line_union_by_first_colon(current: &str, new: &str) -> String {
    use std::collections::HashSet;
    let key_of = |line: &str| line.split(':').next().unwrap_or("").to_string();
    let mut keys: HashSet<String> = HashSet::new();
    let mut out = String::with_capacity(current.len() + new.len());
    for line in current.lines() {
        if !line.is_empty() {
            keys.insert(key_of(line));
        }
        out.push_str(line);
        out.push('\n');
    }
    for line in new.lines() {
        if line.is_empty() {
            continue;
        }
        let k = key_of(line);
        if !keys.contains(&k) {
            out.push_str(line);
            out.push('\n');
            keys.insert(k);
        }
    }
    out
}

fn find_ostree_etc_default() -> Result<PathBuf> {
    let cmdline = fs::read_to_string("/proc/cmdline")?;
    for word in cmdline.split_whitespace() {
        if let Some(_ostree_arg) = word.strip_prefix("ostree=") {
            let deploy_base = Path::new("/sysroot/ostree/deploy/default/deploy");
            if deploy_base.exists() {
                for entry in fs::read_dir(deploy_base)? {
                    let entry = entry?;
                    let name = entry.file_name();
                    let name_str = name.to_string_lossy();
                    if name_str.ends_with(".0") && entry.path().is_dir() {
                        let usr_etc = entry.path().join("usr/etc");
                        if usr_etc.exists() {
                            return Ok(usr_etc);
                        }
                    }
                }
            }
            break;
        }
    }
    anyhow::bail!("could not locate OSTree deployment default /etc");
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn fstab_sanitizer_drops_only_composefs_owned_mounts() {
        let temp = tempdir().unwrap();
        let etc = temp.path().join("etc");
        fs::create_dir_all(&etc).unwrap();
        fs::write(
            etc.join("fstab"),
            "# generated by anaconda\n\
             UUID=root / btrfs subvol=root,ro 0 0\n\
             UUID=boot /boot ext4 defaults 1 2\n\
             UUID=esp /boot/efi vfat umask=0077 0 2\n\
             UUID=home /home btrfs subvol=home 0 0\n\
             UUID=var /var btrfs subvol=var 0 0\n\
             /var/swap/swapfile none swap defaults,nofail 0 0\n",
        )
        .unwrap();

        assert_eq!(sanitize_composefs_fstab(&etc).unwrap(), 2);
        assert_eq!(
            fs::read_to_string(etc.join("fstab")).unwrap(),
            "# generated by anaconda\n\
             UUID=boot /boot ext4 defaults 1 2\n\
             UUID=esp /boot/efi vfat umask=0077 0 2\n\
             UUID=home /home btrfs subvol=home 0 0\n\
             /var/swap/swapfile none swap defaults,nofail 0 0\n"
        );
        assert_eq!(sanitize_composefs_fstab(&etc).unwrap(), 0);
    }

    #[test]
    fn home_compat_direction_follows_preserved_home_mount() {
        struct Case {
            name: &'static str,
            fstab: &'static str,
            expected: Option<LegacyHomeCompat>,
        }

        let cases = [
            Case {
                name: "dedicated home becomes canonical",
                fstab: "UUID=home /home btrfs subvol=home 0 0\n",
                expected: Some(LegacyHomeCompat::HomeToVarHome),
            },
            Case {
                name: "home inside var remains canonical",
                fstab: "UUID=boot /boot ext4 defaults 1 2\n",
                expected: Some(LegacyHomeCompat::VarHomeToHome),
            },
            Case {
                name: "legacy home mount remains canonical",
                fstab: "UUID=home /var/home btrfs subvol=home 0 0\n",
                expected: Some(LegacyHomeCompat::VarHomeToHome),
            },
            Case {
                name: "both locations already mounted",
                fstab: "UUID=home /home btrfs subvol=home 0 0\n\
                        UUID=home /var/home btrfs subvol=home 0 0\n",
                expected: None,
            },
            Case {
                name: "existing forward bridge is idempotent",
                fstab: "/home /var/home none bind 0 0\n",
                expected: None,
            },
            Case {
                name: "existing reverse bridge is idempotent",
                fstab: "/var/home /home none bind 0 0\n",
                expected: None,
            },
            Case {
                name: "commented mounts are ignored",
                fstab: "# UUID=home /home btrfs subvol=home 0 0\n",
                expected: Some(LegacyHomeCompat::VarHomeToHome),
            },
        ];

        for case in cases {
            assert_eq!(
                fstab_home_compat_direction(case.fstab),
                case.expected,
                "{}",
                case.name
            );
        }
    }

    #[test]
    fn legacy_var_home_compat_is_layout_gated_and_idempotent() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        let target = temp.path().join("target");
        let etc = temp.path().join("etc");
        fs::create_dir_all(source.join("var/home")).unwrap();
        fs::create_dir_all(target.join("home")).unwrap();
        fs::create_dir_all(&etc).unwrap();
        std::os::unix::fs::symlink("var/home", source.join("home")).unwrap();
        fs::write(etc.join("fstab"), "UUID=home /home btrfs subvol=home 0 0\n").unwrap();

        assert_eq!(
            apply_legacy_var_home_compat(&source, &target, &etc).unwrap(),
            Some(LegacyHomeCompat::HomeToVarHome)
        );
        assert_eq!(
            apply_legacy_var_home_compat(&source, &target, &etc).unwrap(),
            None
        );
        assert_eq!(
            fs::read_to_string(etc.join("fstab")).unwrap(),
            "UUID=home /home btrfs subvol=home 0 0\n\
             # Preserve OSTree /var/home paths when the target uses a native /home\n\
             /home /var/home none bind,x-systemd.requires-mounts-for=/home 0 0\n"
        );

        let native_source = temp.path().join("native-source");
        let native_etc = temp.path().join("native-etc");
        fs::create_dir_all(native_source.join("home")).unwrap();
        fs::create_dir_all(native_source.join("var/home")).unwrap();
        fs::create_dir_all(&native_etc).unwrap();
        assert_eq!(
            apply_legacy_var_home_compat(&native_source, &target, &native_etc).unwrap(),
            None
        );
        assert!(!native_etc.join("fstab").exists());
    }
}
