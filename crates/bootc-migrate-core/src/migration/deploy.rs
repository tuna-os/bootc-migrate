//! Phase 4: stage the deployment — /etc merge, /var migration, state root.

use super::*;

pub fn phase4_stage_deploy(
    verity: &VerityDigest,
    target_image: &str,
    manifest_digest: &str,
    config_digest: &str,
    sealed_config: &str,
    dry_run: bool,
    force: bool,
) -> Result<PathBuf> {
    println!("=== Phase 4: Staging Deployment State ===");

    let deploy_dir = Path::new("/sysroot/state/deploy").join(verity.as_hex());

    if dry_run {
        println!(
            "[DRY RUN] Would stage deployment at: {}",
            deploy_dir.display()
        );
        return Ok(deploy_dir);
    }

    // Idempotency: skip if already staged with valid .origin.
    // bootc expects the filename as `<bare-hex-verity>.origin` (no `sha512:`
    // prefix); using as_prefixed() here would cause `bootc status` to fail
    // with "Opening origin file: No such file or directory" and break the
    // post-reboot validation.
    let origin_path = deploy_dir.join(format!("{}.origin", verity.as_hex()));
    let already_staged = deploy_dir.exists() && origin_path.exists();
    if already_staged && !force {
        println!(
            "Deployment already staged at {}. Skipping Phase 4.",
            deploy_dir.display()
        );
        return Ok(deploy_dir);
    }
    if already_staged {
        println!("[phase4] --force: refreshing the existing staged deployment.");
    }

    fs::create_dir_all(&deploy_dir).context("failed to create deployment directory")?;

    let etc_dir = deploy_dir.join("etc");
    fs::create_dir_all(&etc_dir).context("failed to create deployment etc directory")?;

    // 3-way /etc merge
    println!("Performing 3-way /etc merge...");
    if let Err(e) = perform_etc_merge(target_image, sealed_config, &etc_dir) {
        eprintln!(
            "3-way /etc merge failed ({}), falling back to flat /etc copy.",
            e
        );
        xattr::copy_dir_all_with_xattrs("/etc", &etc_dir)
            .context("failed to copy /etc (fallback)")?;
    }
    let removed = sanitize_composefs_fstab(&etc_dir)?;
    if removed > 0 {
        println!(
            "[phase4] removed {removed} host root or /var mount(s) from the target /etc/fstab"
        );
    }

    // Stage /var symlink
    let var_symlink = deploy_dir.join("var");
    if var_symlink.exists() || var_symlink.is_symlink() {
        fs::remove_file(&var_symlink).context("failed to remove existing var entry")?;
    }
    std::os::unix::fs::symlink("../../os/default/var", &var_symlink)
        .context("failed to create /var symlink")?;

    // Write .origin file using bootc's expected schema (testutils.rs:316-331).
    // Use the same `tini::Ini` library bootc uses to parse it so the output
    // is byte-compatible. Placeholder boot_digest gets patched in Phase 5
    // with sha256(vmlinuz || initrd) once those files are on the ESP.
    //
    // Key names are load-bearing:
    // - `container-image-reference` is `ostree_ext::container::deploy::ORIGIN_CONTAINER`
    //   — bootc reads this to populate the BootEntry's image field.
    // - `manifest_digest` under [boot] lets bootc fetch the OCI manifest from
    //   the registry without a separate .imginfo file (`bootc internals cfs oci
    //   inspect` is unreliable in our flow).
    let origin_content = build_origin_content(target_image, verity, manifest_digest);
    fs::write(&origin_path, &origin_content).context("failed to write .origin file")?;

    // Write .imginfo file
    println!("Writing .imginfo file...");
    if let Ok(config_json) = crate::migration::inspect_image(config_digest) {
        let imginfo_path = deploy_dir.join(format!("{}.imginfo", verity.as_hex()));
        if let Err(e) = fs::write(&imginfo_path, &config_json) {
            eprintln!(
                "Warning: failed to write .imginfo file ({}): {}",
                imginfo_path.display(),
                e
            );
        }
    }

    // Handle /var migration
    phase4_var_migration()?;

    // For XFS roots, the composefs repo lives in an ext4 loopback file; the
    // booted system must mount it at /sysroot/composefs so `bootc status` and
    // day-2 updates can read the repo (the initrd mount is torn down at
    // switch-root). Install a runtime mount unit into the deployment's /etc.
    if Path::new("/sysroot/composefs-loopback.ext4").exists()
        && let Err(e) = write_runtime_composefs_loopback_mount(&etc_dir)
    {
        eprintln!("[phase4] Warning: failed to install runtime composefs mount: {e:#}");
    }

    Ok(deploy_dir)
}

/// Install a systemd mount unit into the deployment's /etc so the booted system
/// loop-mounts the composefs ext4 store at /sysroot/composefs. Idempotent with
/// any mount that survives the initrd: systemd treats an already-mounted target
/// as active.
fn write_runtime_composefs_loopback_mount(etc_dir: &Path) -> Result<()> {
    let unit_dir = etc_dir.join("systemd/system");
    fs::create_dir_all(&unit_dir)?;
    fs::write(
        unit_dir.join("sysroot-composefs.mount"),
        "[Unit]\n\
         Description=ComposeFS Loopback Store (runtime)\n\
         DefaultDependencies=no\n\
         After=sysroot.mount\n\
         Before=local-fs.target\n\
         \n\
         [Mount]\n\
         What=/sysroot/composefs-loopback.ext4\n\
         Where=/sysroot/composefs\n\
         Type=ext4\n\
         Options=loop,ro\n\
         \n\
         [Install]\n\
         WantedBy=local-fs.target\n",
    )?;
    let wants_dir = unit_dir.join("local-fs.target.wants");
    fs::create_dir_all(&wants_dir)?;
    let link = wants_dir.join("sysroot-composefs.mount");
    let _ = fs::remove_file(&link);
    std::os::unix::fs::symlink("../sysroot-composefs.mount", &link)
        .context("failed to enable runtime sysroot-composefs.mount")?;
    Ok(())
}

fn phase4_var_migration() -> Result<()> {
    println!("=== Migrating /var data to ComposeFS state ===");
    let target_var = Path::new("/sysroot/state/os/default/var");
    let completion_marker =
        Path::new("/sysroot/state/os/default/.bootc-migrate-composefs-var-complete");

    // A dedicated filesystem or direct Btrfs subvolume is mounted at this
    // stateroot path by the initrd (Phase 5), so its existing data remains in
    // place. Copying it would be both wasteful and lossy for special files.
    if let Some(var) = detect_separate_var()? {
        println!(
            "[phase4] preserving /var in place ({}, UUID={}, options={}); Phase 5 will mount it at the composefs stateroot",
            var.fstype, var.uuid, var.options
        );
        return Ok(());
    }

    // A target /var can contain a directory skeleton before migration. Only our
    // completion marker proves the live data copy finished successfully.
    if completion_marker.exists() {
        println!(
            "/var migration already completed at {}. Skipping.",
            target_var.display()
        );
        return Ok(());
    }

    fs::create_dir_all(target_var)?;

    // Copy the mounted live tree. The old OSTree deployment path may exist but
    // is not necessarily the source backing the active /var mount.
    println!("Migrating /var data from /var to ComposeFS state...");
    xattr::copy_dir_all_with_xattrs("/var", target_var)
        .context("failed to migrate /var data to ComposeFS state")?;
    fs::write(
        completion_marker,
        "The live /var tree was copied by bootc-migrate-composefs.\n",
    )
    .context("failed to write /var migration completion marker")?;
    println!("/var data migrated successfully.");

    Ok(())
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

/// Work around target images that replace Mesa's `GL/lib/gbm` symlink with a
/// directory containing only the NVIDIA GBM backend. On hybrid laptops that
/// makes Mutter fail to initialize the AMD GPU driving the internal panel.
///
/// Mesa's loader supports a colon-separated `GBM_BACKENDS_PATH`; retain the
/// NVIDIA directory first and add the target's Mesa-default directory as a
/// fallback. `/etc/environment` is consumed by GDM's PAM session as well as
/// normal user sessions.
fn apply_split_gbm_backend_compat(target_root: &Path, etc_dir: &Path) -> Result<bool> {
    let multiarch = format!("{}-linux-gnu", std::env::consts::ARCH);
    let relative_gl = Path::new("usr/lib").join(&multiarch).join("GL");
    let active = target_root.join(&relative_gl).join("lib/gbm");
    let mesa = target_root.join(&relative_gl).join("default/lib/gbm");

    // A normal image exposes Mesa through the active directory already. Only
    // amend the environment for the precise split-backend layout.
    if !active.is_dir() || !mesa.join("dri_gbm.so").is_file() || active.join("dri_gbm.so").is_file()
    {
        return Ok(false);
    }

    let environment_path = etc_dir.join("environment");
    let mut environment = match fs::read_to_string(&environment_path) {
        Ok(environment) => environment,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(e)
                .with_context(|| format!("failed to read {}", environment_path.display()));
        }
    };
    let already_configured = environment.lines().any(|line| {
        let line = line.trim_start();
        !line.starts_with('#')
            && line
                .split_once('=')
                .is_some_and(|(key, _)| key.trim() == "GBM_BACKENDS_PATH")
    });
    if already_configured {
        return Ok(false);
    }

    if !environment.is_empty() && !environment.ends_with('\n') {
        environment.push('\n');
    }
    environment
        .push_str("# Compatibility for target images with split Mesa and NVIDIA GBM backends\n");
    environment.push_str(&format!(
        "GBM_BACKENDS_PATH=/usr/lib/{multiarch}/GL/lib/gbm:/usr/lib/{multiarch}/GL/default/lib/gbm\n"
    ));
    fs::write(&environment_path, environment)
        .with_context(|| format!("failed to update {}", environment_path.display()))?;
    Ok(true)
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
fn perform_etc_merge(target_image: &str, sealed_config: &str, etc_dir: &Path) -> Result<()> {
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
    let composefs_mounted = match mount_image_for(target_image, sealed_config, &mount_path) {
        Ok(()) if mount_path.join("etc").is_dir() => true,
        _ => {
            eprintln!(
                "[phase4] composefs /etc mount unavailable; falling back to podman image mount"
            );
            false
        }
    };
    let _cfs_guard = if composefs_mounted {
        Some(MountGuard::new(&mount_path))
    } else {
        None
    };
    let _podman_guard = if composefs_mounted {
        None
    } else {
        let pm = PodmanImageMount::new(target_image)
            .context("composefs /etc mount unavailable and podman image mount fallback failed")?;
        println!(
            "[phase4] using podman image mount at {} for /etc",
            pm.path.display()
        );
        mount_path = pm.path.clone();
        Some(pm)
    };

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

    crate::mergetc::merge_etc_files(&old_default_etc, current_etc, &new_default_etc, etc_dir)
        .context("3-way /etc merge failed")?;

    match apply_split_gbm_backend_compat(&mount_path, etc_dir) {
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
/// This is the computation half of #15's proposal; presenting it as an
/// interactive checkbox TUI and feeding selections into Phase 4 as an
/// include/exclude manifest is not implemented yet.
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

    fn make_split_gbm_tree(root: &Path) -> (PathBuf, PathBuf) {
        let multiarch = format!("{}-linux-gnu", std::env::consts::ARCH);
        let gl = root.join("usr/lib").join(multiarch).join("GL");
        let active = gl.join("lib/gbm");
        let mesa = gl.join("default/lib/gbm");
        fs::create_dir_all(&active).unwrap();
        fs::create_dir_all(&mesa).unwrap();
        fs::write(mesa.join("dri_gbm.so"), b"mesa").unwrap();
        (active, mesa)
    }

    #[test]
    fn split_gbm_compat_adds_mesa_fallback_idempotently() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("target");
        let etc = temp.path().join("etc");
        make_split_gbm_tree(&target);
        fs::create_dir_all(&etc).unwrap();
        fs::write(etc.join("environment"), "EXISTING=value").unwrap();

        assert!(apply_split_gbm_backend_compat(&target, &etc).unwrap());
        assert!(!apply_split_gbm_backend_compat(&target, &etc).unwrap());

        let environment = fs::read_to_string(etc.join("environment")).unwrap();
        let expected = format!(
            "GBM_BACKENDS_PATH=/usr/lib/{0}-linux-gnu/GL/lib/gbm:/usr/lib/{0}-linux-gnu/GL/default/lib/gbm",
            std::env::consts::ARCH
        );
        assert!(environment.contains("EXISTING=value\n"));
        assert_eq!(environment.matches(&expected).count(), 1);
    }

    #[test]
    fn split_gbm_compat_leaves_working_or_user_configured_layout_alone() {
        let temp = tempdir().unwrap();
        let working_target = temp.path().join("working-target");
        let (active, _) = make_split_gbm_tree(&working_target);
        fs::write(active.join("dri_gbm.so"), b"mesa").unwrap();
        let working_etc = temp.path().join("working-etc");
        fs::create_dir_all(&working_etc).unwrap();
        assert!(
            !apply_split_gbm_backend_compat(&working_target, &working_etc).unwrap(),
            "a target whose active GBM path already exposes Mesa needs no override"
        );
        assert!(!working_etc.join("environment").exists());

        let overridden_target = temp.path().join("overridden-target");
        make_split_gbm_tree(&overridden_target);
        let overridden_etc = temp.path().join("overridden-etc");
        fs::create_dir_all(&overridden_etc).unwrap();
        let custom = "GBM_BACKENDS_PATH=/custom/gbm\n";
        fs::write(overridden_etc.join("environment"), custom).unwrap();
        assert!(!apply_split_gbm_backend_compat(&overridden_target, &overridden_etc).unwrap());
        assert_eq!(
            fs::read_to_string(overridden_etc.join("environment")).unwrap(),
            custom
        );
    }
}
