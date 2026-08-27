//! Phase 4: stage the deployment — /etc merge, /var migration, state root.

use super::*;
use std::collections::HashSet;

pub fn phase4_stage_deploy(
    verity: &VerityDigest,
    target_image: &str,
    pulled_image: &PulledImage,
    sealed_config: &str,
    dry_run: bool,
    force: bool,
    etc_overrides: Option<&crate::mergetc::EtcDriftManifest>,
) -> Result<PathBuf> {
    println!("=== Phase 4: Staging Deployment State ===");

    let deploy_dir = Path::new("/sysroot/state/deploy").join(verity.as_hex());
    let content_image = &pulled_image.image_reference;

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
    let etc_dir = deploy_dir.join("etc");
    if already_staged && !force {
        match refresh_staged_nvidia_gbm_compat(content_image, &etc_dir) {
            Ok(true) => {
                println!("[phase4] refreshed Mesa's GBM backend path for the staged NVIDIA target")
            }
            Ok(false) => {}
            Err(e) => {
                eprintln!("[phase4] warning: failed to refresh staged GBM backend paths: {e:#}")
            }
        }
        let networkmanager_compat =
            sanitize_staged_networkmanager_backend_compat(content_image, &etc_dir).context(
                "failed to validate NetworkManager Wi-Fi backend compatibility for the staged deployment",
            )?;
        if networkmanager_compat.removed_iwd_settings > 0 {
            println!(
                "[phase4] removed {} incompatible source NetworkManager iwd setting(s) from the staged target",
                networkmanager_compat.removed_iwd_settings
            );
        }
        if networkmanager_compat.removed_wpa_supplicant_mask {
            println!(
                "[phase4] removed the source wpa_supplicant mask so the target's NetworkManager can activate its default Wi-Fi backend"
            );
        }
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
    fs::create_dir_all(&deploy_dir).context("failed to create deployment directory")?;

    fs::create_dir_all(&etc_dir).context("failed to create deployment etc directory")?;

    // 3-way /etc merge
    println!("Performing 3-way /etc merge...");
    if let Some(overrides) = etc_overrides {
        println!(
            "[phase4] applying {} Config Drift Review decision(s) to the /etc merge",
            overrides.decisions.len()
        );
    }
    if let Err(e) = perform_etc_merge(content_image, sealed_config, &etc_dir, etc_overrides) {
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
    let networkmanager_compat =
        sanitize_staged_networkmanager_backend_compat(content_image, &etc_dir)
            .context("failed to validate NetworkManager Wi-Fi backend compatibility")?;
    if networkmanager_compat.removed_iwd_settings > 0 {
        println!(
            "[phase4] removed {} incompatible source NetworkManager iwd setting(s); the target will use its default Wi-Fi backend",
            networkmanager_compat.removed_iwd_settings
        );
    }
    if networkmanager_compat.removed_wpa_supplicant_mask {
        println!(
            "[phase4] removed the source wpa_supplicant mask so the target can activate its default Wi-Fi backend"
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
    let origin_content = build_origin_content(target_image, verity, &pulled_image.manifest_digest);
    fs::write(&origin_path, &origin_content).context("failed to write .origin file")?;

    // Write .imginfo file
    println!("Writing .imginfo file...");
    if let Ok(config_json) = crate::migration::inspect_image(&pulled_image.config_digest) {
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
    // day-2 updates can reach the repo. Usually the initrd's mount survives
    // switch-root and this unit merely goes active over it; it is here for the
    // case where it does not. Install it into the deployment's /etc.
    if Path::new("/sysroot/composefs-loopback.ext4").exists()
        && let Err(e) = write_runtime_composefs_loopback_mount(&etc_dir)
    {
        eprintln!("[phase4] Warning: failed to install runtime composefs mount: {e:#}");
    }

    Ok(deploy_dir)
}

/// Install a systemd mount unit into the deployment's /etc so the booted system
/// loop-mounts the composefs ext4 store at /sysroot/composefs.
///
/// This is a fallback, not the mount the running system normally uses. The
/// initrd unit from `migration::prepare_composefs_loopback_include` mounts the
/// same source at the same target under /sysroot, which *becomes* / at
/// switch-root, so that mount survives — and systemd then treats this unit's
/// already-mounted target as active without issuing a second mount. Options
/// set here therefore do not override the initrd's; they apply only if the
/// initrd mount is somehow absent.
///
/// Kept in sync at **rw** with the initrd unit for exactly that case: day-2
/// updates write to the store, and a `ro` store makes `bootc upgrade` fail
/// with "Repository is not writable: read-only file system". See that
/// function's docs for the full account.
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
         Options=loop,rw\n\
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
    if let Some(var) = super::var_layout::detect_separate_var()? {
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
fn is_nvidia_target(target_image: &str) -> bool {
    target_image.to_ascii_lowercase().contains("nvidia")
}

fn has_split_gbm_backend_layout(target_root: &Path) -> bool {
    let multiarch = format!("{}-linux-gnu", std::env::consts::ARCH);
    let relative_gl = Path::new("usr/lib").join(multiarch).join("GL");
    let active = target_root.join(&relative_gl).join("lib/gbm");
    let mesa = target_root.join(relative_gl).join("default/lib/gbm");

    active.is_dir() && mesa.join("dri_gbm.so").is_file() && !active.join("dri_gbm.so").is_file()
}

fn configure_gbm_backend_compat(etc_dir: &Path) -> Result<bool> {
    let multiarch = format!("{}-linux-gnu", std::env::consts::ARCH);

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

fn apply_target_gbm_backend_compat(
    target_image: &str,
    target_root: &Path,
    etc_dir: &Path,
) -> Result<bool> {
    // Dakota's NVIDIA layer can mask the Mesa backend while retaining the
    // normal layout in a ComposeFS inspection mount. The fallback is harmless
    // on a working layout because the active directory remains first.
    if !is_nvidia_target(target_image) && !has_split_gbm_backend_layout(target_root) {
        return Ok(false);
    }
    configure_gbm_backend_compat(etc_dir)
}

fn refresh_staged_nvidia_gbm_compat(target_image: &str, etc_dir: &Path) -> Result<bool> {
    if !is_nvidia_target(target_image) {
        return Ok(false);
    }
    configure_gbm_backend_compat(etc_dir)
}

/// NetworkManager Wi-Fi profiles are portable, but the daemon backend is part
/// of the target image's implementation. A source image can select iwd even
/// when the target's NetworkManager was built without iwd support, preventing
/// NetworkManager from creating any Wi-Fi device.
fn networkmanager_config_files_in(config_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    if let Some(main_config) = networkmanager_main_config_in(config_dir)? {
        files.push(main_config);
    }
    files.extend(networkmanager_drop_in_files_in(config_dir)?);
    Ok(files)
}

fn networkmanager_main_config_in(config_dir: &Path) -> Result<Option<PathBuf>> {
    let main_config = config_dir.join("NetworkManager.conf");
    match fs::symlink_metadata(&main_config) {
        Ok(metadata) if metadata.is_file() => Ok(Some(main_config)),
        Ok(metadata) if metadata.file_type().is_symlink() => anyhow::bail!(
            "refusing to follow symlinked NetworkManager configuration {}",
            main_config.display()
        ),
        Ok(_) => Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("failed to inspect {}", main_config.display())),
    }
}

fn networkmanager_drop_in_files_in(config_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let drop_in_dir = config_dir.join("conf.d");
    let entries = match fs::read_dir(&drop_in_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(files),
        Err(e) => {
            return Err(e).with_context(|| format!("failed to read {}", drop_in_dir.display()));
        }
    };
    for entry in entries {
        let entry =
            entry.with_context(|| format!("failed to read entry in {}", drop_in_dir.display()))?;
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "conf") {
            continue;
        }
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() => files.push(path),
            Ok(metadata) if metadata.file_type().is_symlink() => anyhow::bail!(
                "refusing to follow symlinked NetworkManager configuration {}",
                path.display()
            ),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| format!("failed to inspect {}", path.display()));
            }
        }
    }
    files.sort();
    Ok(files)
}

fn target_networkmanager_config_files(target_root: &Path) -> Result<Vec<PathBuf>> {
    let vendor_dir = target_root.join("usr/lib/NetworkManager");
    let run_dir = target_root.join("run/NetworkManager");
    let etc_dir = target_root.join("etc/NetworkManager");
    let vendor_drop_ins = networkmanager_drop_in_files_in(&vendor_dir)?;
    let run_drop_ins = networkmanager_drop_in_files_in(&run_dir)?;
    let etc_drop_ins = networkmanager_drop_in_files_in(&etc_dir)?;

    // NetworkManager reads vendor drop-ins, then runtime drop-ins, then the
    // main config, then /etc drop-ins. A same-named higher-priority drop-in
    // shadows the lower-priority file instead of supplementing it.
    let run_names: HashSet<_> = run_drop_ins
        .iter()
        .filter_map(|path| path.file_name().map(ToOwned::to_owned))
        .collect();
    let etc_names: HashSet<_> = etc_drop_ins
        .iter()
        .filter_map(|path| path.file_name().map(ToOwned::to_owned))
        .collect();
    let mut files: Vec<_> = vendor_drop_ins
        .into_iter()
        .filter(|path| {
            path.file_name()
                .is_none_or(|name| !run_names.contains(name) && !etc_names.contains(name))
        })
        .collect();
    files.extend(run_drop_ins.into_iter().filter(|path| {
        path.file_name()
            .is_none_or(|name| !etc_names.contains(name))
    }));
    if let Some(main_config) = networkmanager_main_config_in(&etc_dir)? {
        files.push(main_config);
    }
    files.extend(etc_drop_ins);
    Ok(files)
}

fn parse_networkmanager_config(contents: &str) -> Result<tini::Ini> {
    tini::Ini::from_string(contents)
        .map_err(|e| anyhow!("invalid NetworkManager configuration: {e}"))
}

fn networkmanager_config_backend(contents: &str) -> Result<Option<String>> {
    Ok(parse_networkmanager_config(contents)?.get::<String>("device", "wifi.backend"))
}

fn target_selects_iwd_backend(target_root: &Path) -> Result<bool> {
    let mut backend = None;
    for path in target_networkmanager_config_files(target_root)? {
        let contents = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if let Some(configured_backend) = networkmanager_config_backend(&contents)
            .with_context(|| format!("failed to parse {}", path.display()))?
        {
            backend = Some(configured_backend);
        }
    }
    Ok(backend.is_some_and(|backend| backend.eq_ignore_ascii_case("iwd")))
}

fn strip_iwd_backend_settings(contents: &str) -> Result<(Option<String>, usize)> {
    let config = parse_networkmanager_config(contents)?;
    let selects_iwd = config
        .get::<String>("device", "wifi.backend")
        .is_some_and(|backend| backend.eq_ignore_ascii_case("iwd"));
    let iwd_keys: Vec<String> = config
        .section_iter("device")
        .filter(|(key, _)| key.starts_with("wifi.iwd."))
        .map(|(key, _)| key.clone())
        .collect();
    if !selects_iwd && iwd_keys.is_empty() {
        return Ok((None, 0));
    }

    let mut sanitized = config;
    let mut removed_settings = 0;
    if selects_iwd {
        sanitized = sanitized.section("device").erase("wifi.backend");
        removed_settings += 1;
    }
    for key in &iwd_keys {
        sanitized = sanitized.section("device").erase(key);
        removed_settings += 1;
    }

    let has_settings = sanitized
        .iter()
        .any(|(_, section)| section.iter().next().is_some());
    let contents = has_settings.then(|| sanitized.to_string());
    Ok((contents, removed_settings))
}

#[derive(Debug, Default, PartialEq, Eq)]
struct NetworkManagerBackendCompat {
    removed_iwd_settings: usize,
    removed_wpa_supplicant_mask: bool,
}

fn target_has_wpa_supplicant_service(target_root: &Path) -> bool {
    [
        "etc/systemd/system/wpa_supplicant.service",
        "usr/local/lib/systemd/system/wpa_supplicant.service",
        "usr/lib/systemd/system/wpa_supplicant.service",
        "lib/systemd/system/wpa_supplicant.service",
    ]
    .iter()
    .any(|path| target_root.join(path).is_file())
}

fn remove_wpa_supplicant_mask(etc_dir: &Path) -> Result<bool> {
    let path = etc_dir.join("systemd/system/wpa_supplicant.service");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).with_context(|| format!("failed to inspect {}", path.display())),
    };
    if !metadata.file_type().is_symlink() || fs::read_link(&path)? != Path::new("/dev/null") {
        return Ok(false);
    }

    fs::remove_file(&path).with_context(|| format!("failed to remove {}", path.display()))?;
    Ok(true)
}

fn apply_target_networkmanager_backend_compat(
    target_root: &Path,
    etc_dir: &Path,
) -> Result<NetworkManagerBackendCompat> {
    if target_selects_iwd_backend(target_root)? {
        return Ok(NetworkManagerBackendCompat::default());
    }

    let mut compat = NetworkManagerBackendCompat::default();
    let config_dir = etc_dir.join("NetworkManager");
    for path in networkmanager_config_files_in(&config_dir)? {
        let contents = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let (replacement, removed) = strip_iwd_backend_settings(&contents)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        if removed == 0 {
            continue;
        }

        if let Some(replacement) = replacement {
            fs::write(&path, replacement)
                .with_context(|| format!("failed to update {}", path.display()))?;
        } else {
            fs::remove_file(&path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
        }
        compat.removed_iwd_settings += removed;
    }
    if target_has_wpa_supplicant_service(target_root) {
        compat.removed_wpa_supplicant_mask = remove_wpa_supplicant_mask(etc_dir)?;
    }
    Ok(compat)
}

fn sanitize_staged_networkmanager_backend_compat(
    content_image: &str,
    etc_dir: &Path,
) -> Result<NetworkManagerBackendCompat> {
    let target = PodmanImageMount::new(content_image)
        .context("failed to mount target image for NetworkManager configuration inspection")?;
    apply_target_networkmanager_backend_compat(&target.path, etc_dir)
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

    crate::mergetc::merge_etc_files_with_overrides(
        &old_default_etc,
        current_etc,
        &new_default_etc,
        etc_dir,
        etc_overrides,
    )
    .context("3-way /etc merge failed")?;

    match apply_target_gbm_backend_compat(target_image, &mount_path, etc_dir) {
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

        assert!(
            apply_target_gbm_backend_compat("ghcr.io/projectbluefin/dakota:testing", &target, &etc)
                .unwrap()
        );
        assert!(
            !apply_target_gbm_backend_compat(
                "ghcr.io/projectbluefin/dakota:testing",
                &target,
                &etc
            )
            .unwrap()
        );

        let environment = fs::read_to_string(etc.join("environment")).unwrap();
        let expected = format!(
            "GBM_BACKENDS_PATH=/usr/lib/{0}-linux-gnu/GL/lib/gbm:/usr/lib/{0}-linux-gnu/GL/default/lib/gbm",
            std::env::consts::ARCH
        );
        assert!(environment.contains("EXISTING=value\n"));
        assert_eq!(environment.matches(&expected).count(), 1);
    }

    #[test]
    fn gbm_compat_leaves_working_non_nvidia_and_user_configured_layouts_alone() {
        let temp = tempdir().unwrap();
        let working_target = temp.path().join("working-target");
        let (active, _) = make_split_gbm_tree(&working_target);
        fs::write(active.join("dri_gbm.so"), b"mesa").unwrap();
        let working_etc = temp.path().join("working-etc");
        fs::create_dir_all(&working_etc).unwrap();
        assert!(
            !apply_target_gbm_backend_compat(
                "ghcr.io/projectbluefin/dakota:testing",
                &working_target,
                &working_etc
            )
            .unwrap(),
            "a target whose active GBM path already exposes Mesa needs no override"
        );
        assert!(!working_etc.join("environment").exists());

        let overridden_target = temp.path().join("overridden-target");
        make_split_gbm_tree(&overridden_target);
        let overridden_etc = temp.path().join("overridden-etc");
        fs::create_dir_all(&overridden_etc).unwrap();
        let custom = "GBM_BACKENDS_PATH=/custom/gbm\n";
        fs::write(overridden_etc.join("environment"), custom).unwrap();
        assert!(
            !apply_target_gbm_backend_compat(
                "ghcr.io/projectbluefin/dakota-nvidia:testing",
                &overridden_target,
                &overridden_etc
            )
            .unwrap()
        );
        assert_eq!(
            fs::read_to_string(overridden_etc.join("environment")).unwrap(),
            custom
        );
    }

    #[test]
    fn nvidia_gbm_compat_does_not_depend_on_the_inspection_mount_layout() {
        let temp = tempdir().unwrap();
        let incomplete_target = temp.path().join("incomplete-target");
        let etc = temp.path().join("etc");
        fs::create_dir_all(&incomplete_target).unwrap();
        fs::create_dir_all(&etc).unwrap();

        assert!(
            apply_target_gbm_backend_compat(
                "ghcr.io/projectbluefin/dakota-nvidia:testing",
                &incomplete_target,
                &etc
            )
            .unwrap()
        );
        assert!(
            fs::read_to_string(etc.join("environment"))
                .unwrap()
                .contains("GBM_BACKENDS_PATH=")
        );
    }

    #[test]
    fn staged_nvidia_gbm_compat_refreshes_an_existing_deployment() {
        let temp = tempdir().unwrap();
        let etc = temp.path().join("etc");
        fs::create_dir_all(&etc).unwrap();
        fs::write(etc.join("environment"), "EXISTING=value\n").unwrap();

        assert!(
            refresh_staged_nvidia_gbm_compat("ghcr.io/projectbluefin/dakota-nvidia:testing", &etc)
                .unwrap()
        );
        assert!(
            !refresh_staged_nvidia_gbm_compat("ghcr.io/projectbluefin/dakota-nvidia:testing", &etc)
                .unwrap()
        );
    }

    #[test]
    fn networkmanager_iwd_backend_parser_is_precise() {
        struct Case {
            name: &'static str,
            config: &'static str,
            selects_iwd: bool,
        }

        let cases = [
            Case {
                name: "iwd device backend",
                config: "[device]\nwifi.backend=iwd\n",
                selects_iwd: true,
            },
            Case {
                name: "case insensitive iwd value",
                config: "[device]\nwifi.backend=IWD\n",
                selects_iwd: true,
            },
            Case {
                name: "wpa backend",
                config: "[device]\nwifi.backend=wpa_supplicant\n",
                selects_iwd: false,
            },
            Case {
                name: "iwd value in another section",
                config: "[main]\nwifi.backend=iwd\n",
                selects_iwd: false,
            },
        ];

        for case in cases {
            assert_eq!(
                networkmanager_config_backend(case.config)
                    .unwrap()
                    .is_some_and(|backend| backend.eq_ignore_ascii_case("iwd")),
                case.selects_iwd,
                "{}",
                case.name
            );
        }
    }

    #[test]
    fn strip_iwd_backend_settings_removes_only_iwd_device_settings() {
        struct Case {
            name: &'static str,
            config: &'static str,
            expected: Option<&'static str>,
            removed_settings: usize,
        }

        let cases = [
            Case {
                name: "iwd-only drop-in is empty after cleanup",
                config: "[device]\nwifi.backend=iwd\nwifi.iwd.autoconnect=yes\n",
                expected: None,
                removed_settings: 2,
            },
            Case {
                name: "mixed config preserves unrelated settings",
                config: "[main]\nplugins=keyfile\n\n[device]\nwifi.backend=iwd\nwifi.iwd.autoconnect=yes\nwifi.scan-rand-mac-address=no\n",
                expected: Some(
                    "[main]\nplugins = keyfile\n\n[device]\nwifi.scan-rand-mac-address = no\n",
                ),
                removed_settings: 2,
            },
            Case {
                name: "another backend retains only its compatible settings",
                config: "[device]\nwifi.backend=wpa_supplicant\nwifi.iwd.autoconnect=yes\n",
                expected: Some("[device]\nwifi.backend = wpa_supplicant\n"),
                removed_settings: 1,
            },
            Case {
                name: "config without iwd settings is retained",
                config: "[device]\nwifi.backend=wpa_supplicant\n",
                expected: None,
                removed_settings: 0,
            },
        ];

        for case in cases {
            let (contents, removed_settings) = strip_iwd_backend_settings(case.config).unwrap();
            assert_eq!(contents.as_deref(), case.expected, "{}", case.name);
            assert_eq!(removed_settings, case.removed_settings, "{}", case.name);
        }
    }

    #[test]
    fn networkmanager_backend_compat_uses_target_default() {
        struct Case {
            name: &'static str,
            target_configs: &'static [(&'static str, &'static str)],
            expected_removed_iwd_settings: usize,
            iwd_configs_remain: bool,
            wpa_supplicant_mask_remains: bool,
        }

        let cases = [
            Case {
                name: "target default backend replaces source iwd backend",
                target_configs: &[(
                    "etc/NetworkManager/conf.d/default.conf",
                    "[device]\nwifi.backend=wpa_supplicant\n",
                )],
                expected_removed_iwd_settings: 2,
                iwd_configs_remain: false,
                wpa_supplicant_mask_remains: false,
            },
            Case {
                name: "target-provided iwd backend is retained",
                target_configs: &[(
                    "usr/lib/NetworkManager/conf.d/iwd.conf",
                    "[device]\nwifi.backend=iwd\n",
                )],
                expected_removed_iwd_settings: 0,
                iwd_configs_remain: true,
                wpa_supplicant_mask_remains: true,
            },
            Case {
                name: "later target drop-in overrides vendor iwd",
                target_configs: &[
                    (
                        "usr/lib/NetworkManager/conf.d/10-backend.conf",
                        "[device]\nwifi.backend=iwd\n",
                    ),
                    (
                        "etc/NetworkManager/conf.d/20-backend.conf",
                        "[device]\nwifi.backend=wpa_supplicant\n",
                    ),
                ],
                expected_removed_iwd_settings: 2,
                iwd_configs_remain: false,
                wpa_supplicant_mask_remains: false,
            },
            Case {
                name: "etc drop-in shadows same-named vendor drop-in",
                target_configs: &[
                    (
                        "usr/lib/NetworkManager/conf.d/10-backend.conf",
                        "[device]\nwifi.backend=iwd\n",
                    ),
                    (
                        "etc/NetworkManager/conf.d/10-backend.conf",
                        "[device]\nwifi.backend=wpa_supplicant\n",
                    ),
                ],
                expected_removed_iwd_settings: 2,
                iwd_configs_remain: false,
                wpa_supplicant_mask_remains: false,
            },
        ];

        let source_iwd_backend = "[device]\nwifi.backend=iwd\n";
        let source_iwd_options = "[device]\nwifi.iwd.autoconnect=yes\n";
        for case in cases {
            let temp = tempdir().unwrap();
            let target = temp.path().join("target");
            let etc = temp.path().join("etc");
            for (path, contents) in case.target_configs {
                let target_config = target.join(path);
                fs::create_dir_all(target_config.parent().unwrap()).unwrap();
                fs::write(target_config, contents).unwrap();
            }
            let target_wpa_supplicant =
                target.join("usr/lib/systemd/system/wpa_supplicant.service");
            fs::create_dir_all(target_wpa_supplicant.parent().unwrap()).unwrap();
            fs::write(
                &target_wpa_supplicant,
                "[Service]\nExecStart=/usr/sbin/wpa_supplicant\n",
            )
            .unwrap();

            let iwd_backend = etc.join("NetworkManager/conf.d/10-iwd-backend.conf");
            let iwd_options = etc.join("NetworkManager/conf.d/20-iwd-options.conf");
            let wpa_supplicant_mask = etc.join("systemd/system/wpa_supplicant.service");
            fs::create_dir_all(iwd_backend.parent().unwrap()).unwrap();
            fs::write(&iwd_backend, source_iwd_backend).unwrap();
            fs::write(&iwd_options, source_iwd_options).unwrap();
            fs::create_dir_all(wpa_supplicant_mask.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink("/dev/null", &wpa_supplicant_mask).unwrap();

            let compat = apply_target_networkmanager_backend_compat(&target, &etc).unwrap();
            assert_eq!(
                compat.removed_iwd_settings, case.expected_removed_iwd_settings,
                "{}",
                case.name
            );
            assert_eq!(
                compat.removed_wpa_supplicant_mask, !case.wpa_supplicant_mask_remains,
                "{}",
                case.name
            );
            assert_eq!(
                iwd_backend.exists(),
                case.iwd_configs_remain,
                "{}",
                case.name
            );
            assert_eq!(
                iwd_options.exists(),
                case.iwd_configs_remain,
                "{}",
                case.name
            );
            if case.iwd_configs_remain {
                assert_eq!(
                    fs::read_to_string(&iwd_backend).unwrap(),
                    source_iwd_backend
                );
                assert_eq!(
                    fs::read_to_string(&iwd_options).unwrap(),
                    source_iwd_options
                );
            }
            assert_eq!(
                wpa_supplicant_mask.is_symlink(),
                case.wpa_supplicant_mask_remains,
                "{}",
                case.name
            );
            assert_eq!(
                apply_target_networkmanager_backend_compat(&target, &etc).unwrap(),
                NetworkManagerBackendCompat::default(),
                "{}",
                case.name
            );
        }
    }

    #[test]
    fn wpa_supplicant_mask_removal_preserves_non_masks() {
        enum SourceEntry {
            Missing,
            DevNullMask,
            UnitAlias,
            OverrideFile,
        }

        struct Case {
            name: &'static str,
            source_entry: SourceEntry,
            expected_removed: bool,
            expected_present_after: bool,
        }

        let cases = [
            Case {
                name: "missing unit",
                source_entry: SourceEntry::Missing,
                expected_removed: false,
                expected_present_after: false,
            },
            Case {
                name: "dev null mask",
                source_entry: SourceEntry::DevNullMask,
                expected_removed: true,
                expected_present_after: false,
            },
            Case {
                name: "unit alias",
                source_entry: SourceEntry::UnitAlias,
                expected_removed: false,
                expected_present_after: true,
            },
            Case {
                name: "custom override file",
                source_entry: SourceEntry::OverrideFile,
                expected_removed: false,
                expected_present_after: true,
            },
        ];

        for case in cases {
            let temp = tempdir().unwrap();
            let etc = temp.path().join("etc");
            let path = etc.join("systemd/system/wpa_supplicant.service");
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            match case.source_entry {
                SourceEntry::Missing => {}
                SourceEntry::DevNullMask => {
                    std::os::unix::fs::symlink("/dev/null", &path).unwrap();
                }
                SourceEntry::UnitAlias => {
                    std::os::unix::fs::symlink(
                        "/usr/lib/systemd/system/wpa_supplicant.service",
                        &path,
                    )
                    .unwrap();
                }
                SourceEntry::OverrideFile => {
                    fs::write(&path, "[Service]\nExecStart=/usr/sbin/wpa_supplicant\n").unwrap();
                }
            }

            assert_eq!(
                remove_wpa_supplicant_mask(&etc).unwrap(),
                case.expected_removed,
                "{}",
                case.name
            );
            assert_eq!(
                path.exists() || path.is_symlink(),
                case.expected_present_after,
                "{}",
                case.name
            );
        }
    }

    #[test]
    fn networkmanager_config_discovery_refuses_symlinks() {
        struct Case {
            name: &'static str,
            relative_path: &'static str,
        }

        let cases = [
            Case {
                name: "main config",
                relative_path: "NetworkManager.conf",
            },
            Case {
                name: "drop-in",
                relative_path: "conf.d/10-backend.conf",
            },
        ];

        for case in cases {
            let temp = tempdir().unwrap();
            let config_dir = temp.path().join("NetworkManager");
            let external_config = temp.path().join("external.conf");
            let config_path = config_dir.join(case.relative_path);
            fs::write(&external_config, "[device]\nwifi.backend=iwd\n").unwrap();
            fs::create_dir_all(config_path.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(&external_config, &config_path).unwrap();

            let error = networkmanager_config_files_in(&config_dir).unwrap_err();
            assert!(
                format!("{error:#}").contains("refusing to follow symlinked"),
                "{}",
                case.name
            );
        }
    }

    #[test]
    fn target_wpa_supplicant_service_uses_standard_systemd_paths() {
        let paths = [
            "etc/systemd/system/wpa_supplicant.service",
            "usr/local/lib/systemd/system/wpa_supplicant.service",
            "usr/lib/systemd/system/wpa_supplicant.service",
            "lib/systemd/system/wpa_supplicant.service",
        ];

        for path in paths {
            let temp = tempdir().unwrap();
            let target = temp.path().join("target");
            let unit = target.join(path);
            fs::create_dir_all(unit.parent().unwrap()).unwrap();
            fs::write(unit, "[Service]\nExecStart=/usr/sbin/wpa_supplicant\n").unwrap();
            assert!(target_has_wpa_supplicant_service(&target), "{path}");
        }
    }

    #[test]
    fn missing_target_wpa_supplicant_preserves_source_mask() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("target");
        let etc = temp.path().join("etc");
        let mask = etc.join("systemd/system/wpa_supplicant.service");
        fs::create_dir_all(mask.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink("/dev/null", &mask).unwrap();

        assert_eq!(
            apply_target_networkmanager_backend_compat(&target, &etc).unwrap(),
            NetworkManagerBackendCompat::default()
        );
        assert!(mask.is_symlink());
    }

    /// The composefs store must be mounted rw: day-2 updates write into it,
    /// and a `ro` store makes `bootc upgrade` fail with "Repository is not
    /// writable". This unit and the initrd unit in
    /// `migration::prepare_composefs_loopback_include` must agree — see the
    /// matching test there for why the initrd one is the binding copy.
    #[test]
    fn runtime_composefs_loopback_mount_is_writable() {
        let tmp = tempfile::tempdir().unwrap();
        write_runtime_composefs_loopback_mount(tmp.path()).unwrap();

        let unit = tmp.path().join("systemd/system/sysroot-composefs.mount");
        let body = std::fs::read_to_string(&unit).unwrap();
        let opts = body
            .lines()
            .find_map(|l| l.trim().strip_prefix("Options="))
            .expect("mount unit has no Options= line");
        assert!(
            opts.split(',').any(|o| o == "rw"),
            "runtime composefs loopback mount must be rw, got Options={opts}"
        );
        assert!(body.contains("Where=/sysroot/composefs"));
        assert!(body.contains("What=/sysroot/composefs-loopback.ext4"));

        // Writing the unit is not enough; it has to be enabled.
        let link = tmp
            .path()
            .join("systemd/system/local-fs.target.wants/sysroot-composefs.mount");
        assert!(link.is_symlink(), "mount unit was written but not enabled");
    }
}
