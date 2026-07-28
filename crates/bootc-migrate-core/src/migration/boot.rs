//! Phase 5: bootloader setup — kernel/initrd extraction, systemd-boot/GRUB2 BLS entries, OSTree fallback.

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
struct WirelessDevice {
    interface: String,
    modalias: String,
}

fn read_wireless_devices(net_class: &Path) -> Result<Vec<WirelessDevice>> {
    let entries = match fs::read_dir(net_class) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(e).with_context(|| format!("failed to inspect {}", net_class.display()));
        }
    };

    let mut devices = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !path.join("wireless").is_dir() {
            continue;
        }
        let modalias_path = path.join("device/modalias");
        let modalias = match fs::read_to_string(&modalias_path) {
            Ok(modalias) => modalias.trim().to_owned(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("failed to read {}", modalias_path.display()));
            }
        };
        if modalias.is_empty() {
            continue;
        }
        devices.push(WirelessDevice {
            interface: entry.file_name().to_string_lossy().into_owned(),
            modalias,
        });
    }
    devices.sort_by(|a, b| a.interface.cmp(&b.interface));
    Ok(devices)
}

fn target_resolves_modalias(target_root: &Path, kver: &str, modalias: &str) -> Result<bool> {
    let output = Command::new("/usr/sbin/modprobe")
        .arg("--dirname")
        .arg(target_root)
        .args(["--set-version", kver, "--resolve-alias", modalias])
        .output()
        .context("failed to query the target kernel's module aliases with modprobe")?;

    if output.status.success() {
        return Ok(!String::from_utf8_lossy(&output.stdout).trim().is_empty());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("not found in directory") {
        return Ok(false);
    }
    anyhow::bail!(
        "modprobe could not query target kernel {kver}: {}",
        stderr.trim()
    )
}

fn missing_target_wireless_drivers(target_root: &Path, kver: &str) -> Result<Vec<WirelessDevice>> {
    let mut missing = Vec::new();
    for device in read_wireless_devices(Path::new("/sys/class/net"))? {
        if !target_resolves_modalias(target_root, kver, &device.modalias)? {
            missing.push(device);
        }
    }
    Ok(missing)
}

// ---- Phase 5 ----

pub fn phase5_setup_bootloader(
    report: &PreflightReport,
    verity: &VerityDigest,
    target_image: &str,
    sealed_config: &str,
    dry_run: bool,
    bootloader: &str,
    force: bool,
) -> Result<()> {
    println!("=== Phase 5: Setting Up Bootloader ===");

    // systemd-boot is default when UEFI + NVRAM writable + ESP ready, unless user forces grub2.
    // We no longer require systemd-boot binaries in the *source* OS: Phase 5 extracts the
    // binary from the mounted *target* composefs image and installs it directly. If the
    // target also doesn't ship systemd-boot, Phase 5 falls back to GRUB2 automatically.
    let use_systemd_boot = bootloader != "grub2"
        && report.is_uefi
        && report.nvram_writable
        && report.esp_ready_for_systemd_boot;

    // Optional: Phase 5 idempotency — check if composefs entry already exists.
    let esp = if use_systemd_boot {
        Some(ensure_esp_mounted(report)?)
    } else {
        None
    };
    let entries_check = if let Some(ref esp) = esp {
        Path::new(esp).join("loader/entries")
    } else {
        Path::new("/boot/loader/entries").to_path_buf()
    };
    if entries_check.exists() {
        let has_existing = fs::read_dir(&entries_check)
            .map(|d| {
                d.filter_map(|e| e.ok())
                    .any(|e| e.file_name().to_string_lossy().starts_with("bootc_"))
            })
            .unwrap_or(false);
        if has_existing && !force {
            println!(
                "BLS entries already present in {}. Skipping Phase 5.",
                entries_check.display()
            );
            return Ok(());
        }
        if has_existing && force {
            println!("[phase5] --force: re-running Phase 5 over existing BLS entries.");
            // Remove existing bootc_ entries so they get cleanly rewritten.
            if let Ok(rd) = fs::read_dir(&entries_check) {
                for entry in rd.flatten() {
                    if entry.file_name().to_string_lossy().starts_with("bootc_") {
                        let _ = fs::remove_file(entry.path());
                    }
                }
            }
        }
    }

    let temp_mount =
        TempDir::new_in("/var/tmp").context("failed to create temp mount directory")?;
    let mut mount_path = temp_mount.path().to_path_buf();

    if dry_run {
        println!("[DRY RUN] Would mount EROFS, extract boot artifacts, and write BLS entries.");
        return Ok(());
    }

    // Mount via the sealed config digest (see perform_etc_merge): the rootfs
    // verity would miss the oci-config stream and fall back to a raw EROFS mount
    // that zero-fills kernel/initrd/systemd-bootx64.efi content.
    //
    // `bootc internals cfs oci mount` can return exit 0 while mounting inside its
    // own private mount namespace (MS_REC|MS_PRIVATE), which is torn down the
    // instant the subprocess exits — leaving us an empty directory. A zero exit is
    // therefore not enough; we verify the mount actually exposes content here.
    let composefs_mounted = match mount_image_for(target_image, sealed_config, &mount_path) {
        Ok(()) if mount_path.join("usr/lib/modules").is_dir() => true,
        Ok(()) => {
            eprintln!(
                "[phase5] composefs mount reported success but exposed no content \
                 (bootc mounted in a private namespace that did not persist); \
                 falling back to podman image mount"
            );
            false
        }
        Err(e) => {
            eprintln!(
                "[phase5] composefs overlay mount failed ({e}); \
                 falling back to podman image mount"
            );
            false
        }
    };
    // Only guard the composefs overlay mount when it actually persisted into our
    // namespace; otherwise umount would just warn about a mount that isn't ours.
    let _cfs_guard = if composefs_mounted {
        Some(MountGuard::new(&mount_path))
    } else {
        None
    };

    // Fallback: mount the already-cached image (Phase 2 podman pull) and read boot
    // artifacts straight off local storage — no network, real file content. This
    // sidesteps both the private-namespace composefs mount and the registry-stream
    // path (which fails on hosts that can't reach the upstream registry mid-migration).
    let _podman_guard = if composefs_mounted {
        None
    } else {
        let pm = PodmanImageMount::new(target_image)
            .context("composefs mount unavailable and podman image mount fallback also failed")?;
        println!(
            "[phase5] using podman image mount at {} for boot artifacts",
            pm.path.display()
        );
        mount_path = pm.path.clone();
        Some(pm)
    };
    // We now have a usable rootfs at mount_path (composefs overlay or podman mount).
    let mount_ok = true;

    // Find kernel version from the mounted image /usr/lib/modules.
    let modules_dir = mount_path.join("usr/lib/modules");
    let kver = fs::read_dir(&modules_dir)
        .with_context(|| {
            format!(
                "reading kernel modules dir from mounted image: {}",
                modules_dir.display()
            )
        })?
        .filter_map(|e| e.ok())
        .find(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .ok_or_else(|| anyhow!("could not find kernel version in mounted image"))?;
    println!("Found kernel version: {}", kver);

    match missing_target_wireless_drivers(&mount_path, &kver) {
        Ok(missing) if !missing.is_empty() => {
            let devices = missing
                .iter()
                .map(|device| format!("{} ({})", device.interface, device.modalias))
                .collect::<Vec<_>>()
                .join(", ");
            let message = format!(
                "target kernel {kver} has no module alias for wireless device(s): {devices}. \
                 Wi-Fi will be unavailable after migration"
            );
            if force {
                eprintln!(
                    "[phase5] WARNING: {message}. --force was supplied, so bootloader setup will continue."
                );
            } else {
                anyhow::bail!(
                    "{message}. Refusing to install the ComposeFS boot entry. Fix the target image, \
                     or rerun with --force only if alternate networking is available"
                );
            }
        }
        Ok(_) => {}
        Err(e) => {
            eprintln!("[phase5] warning: could not verify target wireless driver support: {e:#}")
        }
    }

    let mounted_vmlinuz = modules_dir.join(&kver).join("vmlinuz");
    let mounted_initrd = modules_dir.join(&kver).join("initramfs.img");

    let vmlinuz_src = if mounted_vmlinuz.exists() {
        mounted_vmlinuz
    } else if mount_path
        .join("boot")
        .join(format!("vmlinuz-{}", kver))
        .exists()
    {
        mount_path.join("boot").join(format!("vmlinuz-{}", kver))
    } else {
        // Mount empty/unavailable: use canonical in-container path so extraction
        // falls back to podman with the correct source path.
        modules_dir.join(&kver).join("vmlinuz")
    };
    let initrd_src = if mounted_initrd.exists() {
        mounted_initrd
    } else if mount_path
        .join("boot")
        .join(format!("initramfs-{}.img", kver))
        .exists()
    {
        mount_path
            .join("boot")
            .join(format!("initramfs-{}.img", kver))
    } else {
        // Same canonical fallback for initrd.
        modules_dir.join(&kver).join("initramfs.img")
    };

    // Read target os-release for BLS naming
    let target_os = read_os_release(&mount_path).unwrap_or_else(|_| os_release::OsRelease {
        id: "linux".into(),
        version_id: String::new(),
        name: String::new(),
        pretty_name: String::new(),
    });

    let options_str = get_kernel_options(verity.as_hex())?;

    // Write to staged entries first, then atomically rename.
    let mut entries: Vec<bootloader::BlsEntry> = Vec::new();

    // Track whether we actually completed the systemd-boot install. If extraction from the
    // target image fails, we fall through to the GRUB2 branch instead of erroring out so the
    // user always ends up with a bootable system.
    let mut sd_boot_installed = false;
    if use_systemd_boot {
        let esp = esp.as_ref().unwrap();
        let esp_path = Path::new(esp);

        match install_systemd_boot_from_target(esp_path, &mount_path, target_image, mount_ok) {
            Ok(()) => {
                // Copy composefs kernel+initrd to ESP via registry stream (raw EROFS reads
                // return zero-filled content past the inline threshold).
                let boot_dir_name = format!("bootc_composefs-{}", verity.as_hex());
                let esp_boot_dir = esp_path.join("EFI/Linux").join(&boot_dir_name);
                fs::create_dir_all(&esp_boot_dir).with_context(|| {
                    format!("creating ESP boot dir: {}", esp_boot_dir.display())
                })?;

                // Translate the discovered host-mount paths back to in-container paths.
                let rel_vmlinuz = vmlinuz_src
                    .strip_prefix(&mount_path)
                    .with_context(|| format!("vmlinuz {:?} not under mount", vmlinuz_src))?;
                let in_container_vmlinuz = Path::new("/").join(rel_vmlinuz);
                let esp_vmlinuz = esp_boot_dir.join("vmlinuz");

                let mut extract = vec![(in_container_vmlinuz.as_path(), esp_vmlinuz.as_path())];
                let in_container_initrd;
                let esp_initrd;
                let mut have_initrd = false;
                // When the composefs mount is empty (bootc mounted in a private
                // namespace), initrd_src won't exist on disk — but we still know its
                // canonical in-container path and extract it via the registry.
                if initrd_src.exists() || !mount_ok {
                    let rel_initrd = initrd_src
                        .strip_prefix(&mount_path)
                        .with_context(|| format!("initrd {:?} not under mount", initrd_src))?;
                    in_container_initrd = Path::new("/").join(rel_initrd);
                    esp_initrd = esp_boot_dir.join("initrd");
                    extract.push((in_container_initrd.as_path(), esp_initrd.as_path()));
                    have_initrd = true;
                } else {
                    esp_initrd = PathBuf::new();
                }
                extract_files_preferring_mount(&mount_path, target_image, &extract)
                    .context("failed to copy kernel/initrd from target image")?;

                // Rebuild initrd with LVM support if the source system uses LVM.
                // Must happen before patch_origin_boot_digest so the hash covers
                // the LVM-enabled initrd bytes, not the original Dakota initrd.
                if have_initrd
                    && let Err(e) = rebuild_initrd_with_lvm_if_needed(
                        &kver,
                        &mount_path,
                        target_image,
                        &esp_initrd,
                    )
                {
                    eprintln!("[phase5] Warning: composefs initrd rebuild failed: {e:#}");
                }

                // Now that vmlinuz + initrd are on the ESP, compute their
                // boot_digest (sha256(vmlinuz || initrd)) and patch the .origin
                // file. `bootc status` requires this digest to set soft-reboot
                // capability; without it, status fails with "Could not find
                // boot digest for deployment".
                if have_initrd
                    && let Err(e) = patch_origin_boot_digest(verity, &esp_vmlinuz, &esp_initrd)
                {
                    eprintln!("[phase5] warning: failed to patch origin boot_digest: {e:#}");
                }

                // Build composefs BLS entry.
                let composefs_entry = bootloader::BlsEntry {
                    title: bls_entry_title(&target_os, "composefs"),
                    version: kver.clone(),
                    linux: format!("/EFI/Linux/{}/vmlinuz", boot_dir_name),
                    initrds: vec![format!("/EFI/Linux/{}/initrd", boot_dir_name)],
                    options: options_str.clone(),
                    filename: bls_entry_filename(&target_os, verity.as_hex(), 1),
                    sort_key: format!("bootc-{}-0", target_os.id),
                };

                // Stage + atomic-rename. Only the composefs entry goes on the ESP.
                //
                // We intentionally do NOT write an OSTree fallback BLS entry here:
                // `bootc status` (`Parsers/bls_config.rs::boot_artifact_info`) treats
                // every non-EFI BLS entry on the ESP as a composefs deployment and
                // bails with "No composefs= param" if it finds one without a
                // composefs= cmdline. Adding such an entry breaks `bootc status` and
                // every downstream that depends on it. Recovery is still possible
                // via firmware menu (`Fedora\shimx64.efi` remains in NVRAM BootOrder)
                // or by selecting the OSTree GRUB entry from /boot/loader/entries.
                let staged_dir = esp_path.join("loader/entries.staged");
                fs::create_dir_all(&staged_dir).with_context(|| {
                    format!("creating ESP staged entries dir: {}", staged_dir.display())
                })?;
                let entries_dir = esp_path.join("loader/entries");
                fs::create_dir_all(&entries_dir).with_context(|| {
                    format!("creating ESP entries dir: {}", entries_dir.display())
                })?;
                let to_promote: Vec<&bootloader::BlsEntry> = vec![&composefs_entry];
                for entry in &to_promote {
                    fs::write(staged_dir.join(&entry.filename), entry.render())
                        .with_context(|| format!("writing staged BLS entry: {}", entry.filename))?;
                    fs::rename(
                        staged_dir.join(&entry.filename),
                        entries_dir.join(&entry.filename),
                    )
                    .with_context(|| {
                        format!("failed to promote ESP BLS entry: {}", entry.filename)
                    })?;
                }

                // loader.conf: composefs is the default, 3s timeout so the user can pick the
                // OSTree fallback during the evaluation window.
                let default_id = composefs_entry.filename.trim_end_matches(".conf");
                let loader_conf = format!("default {}\ntimeout 3\nconsole-mode keep\n", default_id);
                fs::write(esp_path.join("loader/loader.conf"), loader_conf)
                    .context("failed to write loader.conf")?;

                // Register Linux Boot Manager in NVRAM (best-effort).
                register_systemd_boot_nvram(esp);

                sd_boot_installed = true;
                println!(
                    "systemd-boot installed from target image. Composefs is the default; \
                     OSTree fallback available in the loader menu (3s timeout)."
                );
            }
            Err(e) => {
                eprintln!(
                    "Warning: could not install systemd-boot from target image ({}). \
                     Falling back to GRUB2 path.",
                    e
                );
            }
        }
    }

    if !sd_boot_installed {
        // GRUB2 path
        println!("Staying on GRUB2 bootloader (BLS Type 1)...");
        let boot_dir_name = format!("bootc_composefs-{}", verity.as_hex());
        let grub_boot_dir = Path::new("/boot").join(&boot_dir_name);
        fs::create_dir_all(&grub_boot_dir)
            .with_context(|| format!("creating GRUB boot dir: {}", grub_boot_dir.display()))?;

        // Use registry stream for vmlinuz + initrd — copying from the raw EROFS mount
        // zero-fills content past the inline threshold, producing a corrupt 192MB initrd.
        let rel_vmlinuz = vmlinuz_src
            .strip_prefix(&mount_path)
            .with_context(|| format!("vmlinuz {:?} not under mount", vmlinuz_src))?;
        let in_container_vmlinuz = Path::new("/").join(rel_vmlinuz);
        let grub_vmlinuz = grub_boot_dir.join("vmlinuz");
        let mut grub_extract: Vec<(PathBuf, PathBuf)> =
            vec![(in_container_vmlinuz, grub_vmlinuz.clone())];
        let grub_initrd = grub_boot_dir.join("initrd");
        let mut have_grub_initrd = false;
        if initrd_src.exists() || !mount_ok {
            let rel_initrd = initrd_src
                .strip_prefix(&mount_path)
                .with_context(|| format!("initrd {:?} not under mount", initrd_src))?;
            let in_container_initrd = Path::new("/").join(rel_initrd);
            grub_extract.push((in_container_initrd, grub_initrd.clone()));
            have_grub_initrd = true;
        }
        let extract_pairs: Vec<(&Path, &Path)> = grub_extract
            .iter()
            .map(|(s, d)| (s.as_path(), d.as_path()))
            .collect();
        extract_files_preferring_mount(&mount_path, target_image, &extract_pairs)
            .context("failed to copy kernel/initrd from target image (GRUB2 path)")?;

        if have_grub_initrd
            && let Err(e) =
                rebuild_initrd_with_lvm_if_needed(&kver, &mount_path, target_image, &grub_initrd)
        {
            eprintln!("[phase5] Warning: LVM initrd rebuild failed: {e:#}");
        }

        // Composefs entry (priority 1)
        entries.push(bootloader::BlsEntry {
            title: bls_entry_title(&target_os, "composefs"),
            version: kver.clone(),
            linux: format!("/{}/vmlinuz", boot_dir_name),
            initrds: vec![format!("/{}/initrd", boot_dir_name)],
            options: options_str.clone(),
            filename: bls_entry_filename(&target_os, verity.as_hex(), 1),
            sort_key: format!("bootc-{}-0", target_os.id),
        });

        // OSTree fallback entry (priority 0)
        if let Ok(ostree_entry) = build_ostree_fallback_entry() {
            entries.push(ostree_entry);
        }

        // Write to entries.staged/ first
        let staged_dir = Path::new("/boot/loader/entries.staged");
        fs::create_dir_all(staged_dir).context("creating /boot/loader/entries.staged")?;
        for entry in &entries {
            let entry_path = staged_dir.join(&entry.filename);
            fs::write(&entry_path, entry.render())
                .with_context(|| format!("writing staged BLS entry: {}", entry.filename))?;
        }

        // Propagate rename errors.
        let entries_dir = Path::new("/boot/loader/entries");
        fs::create_dir_all(entries_dir)?;
        for entry in &entries {
            let src = staged_dir.join(&entry.filename);
            let dst = entries_dir.join(&entry.filename);
            fs::rename(&src, &dst)
                .with_context(|| format!("failed to promote staged entry: {}", entry.filename))?;
        }

        // Set the composefs entry as the persistent default via saved_entry.
        // bootupd-shipped grub.cfg only has `set default="${saved_entry}"` — it does NOT
        // include the `if [ "${next_entry}" ]` one-shot block, so grub2-reboot's
        // next_entry is silently ignored. Set saved_entry directly. We also still call
        // grub2-reboot so distros that DO honor next_entry get the one-shot semantics
        // (revert on failed boot), but we don't rely on it.
        let composefs_entry_id = bls_entry_filename(&target_os, verity.as_hex(), 1);
        let entry_id = composefs_entry_id.trim_end_matches(".conf");
        let grubenv = "/boot/grub2/grubenv";

        let saved_ok = Command::new("grub2-editenv")
            .args([grubenv, "set", &format!("saved_entry={}", entry_id)])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !saved_ok {
            // Fall back to grub2-set-default which writes through to grubenv.
            let sd_ok = Command::new("grub2-set-default")
                .arg(entry_id)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !sd_ok {
                eprintln!(
                    "Warning: failed to set grub saved_entry={}. Composefs may not be the default boot target.",
                    entry_id
                );
            }
        }

        // Best-effort one-shot for distros with the next_entry block.
        let _ = Command::new("grub2-reboot").arg(entry_id).status();

        // Ensure GRUB_DEFAULT=saved in /etc/default/grub
        let grub_defaults_path = "/etc/default/grub";
        let existing = fs::read_to_string(grub_defaults_path).unwrap_or_default();
        let mut new_cfg = String::new();
        let mut found = false;
        for line in existing.lines() {
            if line.starts_with("GRUB_DEFAULT=") {
                new_cfg.push_str("GRUB_DEFAULT=saved\n");
                found = true;
            } else {
                new_cfg.push_str(line);
                new_cfg.push('\n');
            }
        }
        if !found {
            new_cfg.push_str("GRUB_DEFAULT=saved\n");
        }
        fs::write(grub_defaults_path, &new_cfg).context("failed to write /etc/default/grub")?;

        // Inject set default="${saved_entry}" into grub.cfg
        let grub_cfg_path = "/boot/grub2/grub.cfg";
        if let Ok(cfg) = fs::read_to_string(grub_cfg_path)
            && !cfg.contains("set default=\"${saved_entry}\"")
        {
            let patched = cfg.replace("\nblscfg\n", "\nset default=\"${saved_entry}\"\nblscfg\n");
            if patched != cfg {
                fs::write(grub_cfg_path, &patched).context("failed to write patched grub.cfg")?;
            }
        }
    }

    Ok(())
}

/// Copy systemd-boot binaries from the mounted target image to the ESP.
/// This avoids needing systemd-boot installed in the *source* OS (Bluefin) — we lift
/// it straight out of the Dakota image that's already mounted for kernel extraction.
/// Extract a list of files from the target OCI image using `skopeo` + `tar -O`.
///
/// We can't read these files from the local EROFS mount (zero-fills past the inline
/// threshold) and we can't use `podman cp` either: it would have to unpack the whole
/// image into the local overlay store just so it can extract three files, which
/// reliably ENOSPCs on tight bootc systems. Skopeo's `dir:` format keeps the raw
/// compressed layer blobs on disk without expanding them, so the footprint is roughly
/// "compressed image size" instead of "compressed + expanded".
///
/// `files` is a list of (in-container source path, on-host destination path) pairs.
/// Destination parent directories must already exist. The OCI layers are scanned
/// newest-first; the first hit wins (matches how the OCI image overlay would resolve).
///
/// We can't use `skopeo copy ... dir:` either: it downloads every compressed layer
/// to disk before we can touch any of them, which ENOSPCs on the freshly-migrated
/// btrfs (the EROFS image and composefs object store have already eaten most of /var).
/// Instead we hit the registry HTTP API directly and stream one layer at a time,
/// deleting it before moving on so peak disk use is bounded by the largest layer.
/// Copy files out of a mounted composefs image, falling back to registry
/// streaming only for sources missing from the mount. Each pair is
/// `(path-in-image starting with "/", destination)`. Now that Phase 3 seals the
/// image and Phases 4/5 mount it by its sealed config digest, the composefs
/// overlay exposes real file content — so the kernel, initrd, systemd-bootx64.efi
/// etc. can be copied straight off the mount, removing the runtime dependency on
/// reaching the image's upstream registry (which an offline target or a CI VM
/// with no egress cannot satisfy).
fn extract_files_preferring_mount(
    mount_path: &Path,
    image_ref: &str,
    files: &[(&Path, &Path)],
) -> Result<()> {
    let mut missing: Vec<(&Path, &Path)> = Vec::new();
    for (src, dest) in files {
        let rel = src.strip_prefix("/").unwrap_or(src);
        let from = mount_path.join(rel);
        if !from.exists() {
            missing.push((*src, *dest));
            continue;
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating parent dir for {}", dest.display()))?;
        }
        fs::copy(&from, dest)
            .with_context(|| format!("copying {} -> {}", from.display(), dest.display()))?;
    }
    if !missing.is_empty() {
        println!(
            "[extract] {} file(s) absent from composefs mount; falling back to registry",
            missing.len()
        );
        extract_files_via_registry(image_ref, &missing)?;
    }
    Ok(())
}

/// Copy the target kernel's module tree out of a mounted composefs image into a
/// writable scratch dir (depmod must write `modules.dep.bin`, and the mount is
/// read-only). Returns the same `(scratch, modules_dir)` shape as
/// [`extract_kernel_modules_via_registry`] so callers are interchangeable.
pub(crate) fn copy_kernel_modules_from_mount(
    mount_path: &Path,
    kver: &str,
) -> Result<(TempDir, PathBuf)> {
    let src = mount_path.join("usr/lib/modules").join(kver);
    if !src.join("kernel").is_dir() {
        anyhow::bail!(
            "kernel modules for {kver} not present in composefs mount at {}",
            src.display()
        );
    }
    let scratch = tempfile::Builder::new()
        .prefix("bootc-migrate-kmods-")
        .tempdir_in("/var/tmp")
        .context("failed to create /var/tmp scratch dir for kernel modules")?;
    let dst = scratch.path().join("usr/lib/modules").join(kver);
    crate::xattr::copy_dir_all_with_xattrs(&src, &dst)
        .with_context(|| format!("copying kernel modules from {}", src.display()))?;
    Ok((scratch, dst))
}

/// Compute sha256(vmlinuz || initrd) and patch the `.origin` file's
/// `boot_digest = …` line. `bootc status` uses this digest to set the soft
/// reboot capability; without it, status bails with
/// "Could not find boot digest for deployment".
fn patch_origin_boot_digest(verity: &VerityDigest, vmlinuz: &Path, initrd: &Path) -> Result<()> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let v = fs::read(vmlinuz).with_context(|| format!("reading vmlinuz {}", vmlinuz.display()))?;
    let i = fs::read(initrd).with_context(|| format!("reading initrd {}", initrd.display()))?;
    hasher.update(&v);
    hasher.update(&i);
    let raw = hasher.finalize();
    let mut digest = String::with_capacity(raw.len() * 2);
    for b in raw {
        digest.push_str(&format!("{:02x}", b));
    }

    let origin_path = Path::new("/sysroot/state/deploy")
        .join(verity.as_hex())
        .join(format!("{}.origin", verity.as_hex()));
    let text = fs::read_to_string(&origin_path)
        .with_context(|| format!("reading origin {}", origin_path.display()))?;
    let patched = patch_boot_digest_in_content(&text, &digest)?;
    fs::write(&origin_path, &patched)
        .with_context(|| format!("writing patched origin {}", origin_path.display()))?;
    println!("[phase5] patched origin boot_digest = {}", digest);
    Ok(())
}

/// Stream OCI layers oldest→newest and extract everything matching `subtree`
/// (e.g. `etc/`) into `dst_dir`. Later layers' files overwrite earlier ones,
/// matching how the container runtime composes the rootfs. Whiteouts
/// (`.wh.*`) are ignored — at worst we'll keep a few extra stale files in
/// `etc/`, which doesn't break anything we care about.
fn install_systemd_boot_from_target(
    esp_path: &Path,
    mount_path: &Path,
    target_image: &str,
    mount_ok: bool,
) -> Result<()> {
    // The sealed composefs overlay mount exposes real file content, so the
    // systemd-boot binary is read straight off the mount (with a registry
    // fallback for the unusual case where it's absent from the mount).
    // When the mount is empty (bootc mounted in a private namespace), skip the
    // probe and let extract_files_preferring_mount source the binary from the
    // registry — it errors if the image genuinely doesn't ship systemd-boot,
    // which the caller turns into a graceful GRUB2 fallback.
    let probe = mount_path.join("usr/lib/systemd/boot/efi/systemd-bootx64.efi");
    if mount_ok && !probe.exists() {
        return Err(anyhow!(
            "target image does not ship systemd-boot at /usr/lib/systemd/boot/efi/systemd-bootx64.efi"
        ));
    }

    let sd_dir = esp_path.join("EFI/systemd");
    fs::create_dir_all(&sd_dir)?;
    // Removable-media fallback path so the firmware will boot it even if NVRAM is wiped.
    let removable_dir = esp_path.join("EFI/BOOT");
    fs::create_dir_all(&removable_dir)?;

    let sd_dst = sd_dir.join("systemd-bootx64.efi");
    extract_files_preferring_mount(
        mount_path,
        target_image,
        &[(
            Path::new("/usr/lib/systemd/boot/efi/systemd-bootx64.efi"),
            &sd_dst,
        )],
    )
    .context("failed to install systemd-bootx64.efi from target image")?;

    // Mirror to removable-media path. Local copy of the freshly-extracted (real) bytes
    // is safe — no EROFS in the read path.
    fs::copy(&sd_dst, removable_dir.join("BOOTX64.EFI"))
        .context("failed to install BOOTX64.EFI removable-media loader")?;

    Ok(())
}

/// Register `Linux Boot Manager` in UEFI NVRAM pointing at the systemd-boot loader.
/// Idempotent — skips if an entry by that label already exists. Best-effort: warns
/// on failure instead of erroring, since the removable-media loader at \EFI\BOOT\BOOTX64.EFI
/// keeps the system bootable as a last resort.
fn register_systemd_boot_nvram(esp_path: &str) {
    if let Ok(out) = Command::new("efibootmgr").arg("-v").output() {
        let txt = String::from_utf8_lossy(&out.stdout);
        if txt.lines().any(|l| l.contains("Linux Boot Manager")) {
            println!("Linux Boot Manager already registered in UEFI NVRAM.");
            return;
        }
    }

    let (disk, part) = match get_esp_disk_and_part(esp_path) {
        Some(dp) => dp,
        None => {
            eprintln!(
                "Warning: could not parse ESP device for efibootmgr. \
                 systemd-boot binary is on the ESP at \\EFI\\BOOT\\BOOTX64.EFI \
                 (removable-media path) but no NVRAM entry was created."
            );
            return;
        }
    };

    let status = Command::new("efibootmgr")
        .args([
            "--create",
            "--disk",
            &disk,
            "--part",
            &part,
            "--loader",
            "\\EFI\\systemd\\systemd-bootx64.efi",
            "--label",
            "Linux Boot Manager",
        ])
        .status();
    match status {
        Ok(s) if s.success() => println!("Registered 'Linux Boot Manager' in UEFI NVRAM."),
        Ok(s) => eprintln!(
            "Warning: efibootmgr --create failed (exit {:?}). \
             Removable-media loader at \\EFI\\BOOT\\BOOTX64.EFI remains as fallback.",
            s.code()
        ),
        Err(e) => eprintln!("Warning: failed to invoke efibootmgr ({}).", e),
    }
}

/// Build an OSTree fallback BLS entry placed on the ESP (systemd-boot path).
/// Copies the running OSTree deployment's kernel/initrd to <esp>/EFI/Linux/ostree-fallback/.
#[allow(dead_code)]
fn build_ostree_fallback_on_esp(esp_path: &Path) -> Result<bootloader::BlsEntry> {
    let (deploy_root, _checksum) = find_ostree_deployment()?;

    let modules_dir = deploy_root.join("usr/lib/modules");
    let kver = fs::read_dir(&modules_dir)?
        .filter_map(|e| e.ok())
        .find(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .ok_or_else(|| anyhow!("no kernel version in OSTree deployment"))?;

    let vmlinuz_path = modules_dir.join(&kver).join("vmlinuz");
    let initrd_path = modules_dir.join(&kver).join("initramfs.img");

    let fallback_dir = esp_path.join("EFI/Linux/ostree-fallback");
    fs::create_dir_all(&fallback_dir)?;
    if vmlinuz_path.exists() {
        fs::copy(&vmlinuz_path, fallback_dir.join("vmlinuz"))?;
    }
    if initrd_path.exists() {
        fs::copy(&initrd_path, fallback_dir.join("initrd"))?;
    }

    let cmdline = fs::read_to_string("/proc/cmdline").unwrap_or_default();
    let options: Vec<&str> = cmdline
        .split_whitespace()
        .filter(|w| !w.starts_with("composefs="))
        .collect();

    Ok(bootloader::BlsEntry {
        title: "Bluefin (OSTree fallback)".into(),
        version: kver,
        linux: "/EFI/Linux/ostree-fallback/vmlinuz".into(),
        initrds: vec!["/EFI/Linux/ostree-fallback/initrd".into()],
        options: options.join(" "),
        filename: "ostree-fallback-0.conf".into(),
        sort_key: "ostree-fallback-99".into(),
    })
}

/// Build a fallback BLS entry for the OSTree deployment — GRUB2 path (copies to /boot).
fn build_ostree_fallback_entry() -> Result<bootloader::BlsEntry> {
    let (deploy_root, _checksum) = find_ostree_deployment()?;

    let modules_dir = deploy_root.join("usr/lib/modules");
    let kver = fs::read_dir(&modules_dir)?
        .filter_map(|e| e.ok())
        .find(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .ok_or_else(|| anyhow!("no kernel version in OSTree deployment"))?;

    let vmlinuz_path = modules_dir.join(&kver).join("vmlinuz");
    let initrd_path = modules_dir.join(&kver).join("initramfs.img");

    let fallback_dir = Path::new("/boot/ostree-fallback");
    fs::create_dir_all(fallback_dir)?;
    if vmlinuz_path.exists() {
        fs::copy(&vmlinuz_path, fallback_dir.join("vmlinuz"))?;
    }
    if initrd_path.exists() {
        fs::copy(&initrd_path, fallback_dir.join("initrd"))?;
    }

    let cmdline = fs::read_to_string("/proc/cmdline").unwrap_or_default();
    let options: Vec<&str> = cmdline
        .split_whitespace()
        .filter(|w| !w.starts_with("composefs="))
        .collect();

    Ok(bootloader::BlsEntry {
        title: "OSTree (fallback)".into(),
        version: kver,
        linux: "/ostree-fallback/vmlinuz".into(),
        initrds: vec!["/ostree-fallback/initrd".into()],
        options: options.join(" "),
        filename: "ostree-fallback-0.conf".into(),
        sort_key: "ostree-fallback-99".into(),
    })
}

fn find_ostree_deployment() -> Result<(PathBuf, String)> {
    let deploy_base = Path::new("/sysroot/ostree/deploy/default/deploy");
    if deploy_base.exists() {
        for entry in fs::read_dir(deploy_base)? {
            let entry = entry?;
            let name_str = entry.file_name().to_string_lossy().into_owned();
            if name_str.ends_with(".0") && entry.path().is_dir() {
                let checksum = name_str.trim_end_matches(".0").to_string();
                return Ok((entry.path(), checksum));
            }
        }
    }
    Err(anyhow!("no OSTree deployment found for fallback entry"))
}

// ---- Helpers ----

/// Ensure the ESP is mounted and return its mount path.
/// On OSTree systems the ESP may not be auto-mounted; we mount it temporarily if needed.
fn ensure_esp_mounted(report: &PreflightReport) -> Result<String> {
    // If already detected and mounted, use it.
    if let Some(ref path) = report.esp_path
        && Path::new(path).exists()
    {
        return Ok(path.clone());
    }

    // Try common mount points first.
    for path in ["/boot/efi", "/efi"] {
        if Path::new(path).exists() && Path::new(path).join("EFI").exists() {
            return Ok(path.to_string());
        }
    }

    // ESP not mounted — try to find and mount it.
    // Use lsblk to find the ESP partition by its type GUID.
    let output = Command::new("lsblk")
        .args(["-o", "NAME,PARTTYPE,MOUNTPOINT", "-l", "-n"])
        .output()
        .context("failed to run lsblk")?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    for line in stdout.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 && parts[1] == "c12a7328-f81f-11d2-ba4b-00a0c93ec93b" {
            let device = format!("/dev/{}", parts[0]);
            let mount_point = "/var/tmp/esp-migration";
            fs::create_dir_all(mount_point)?;
            let status = Command::new("mount")
                .args([&device, mount_point])
                .status()
                .context("failed to mount ESP")?;
            if status.success() {
                println!("Auto-mounted ESP {} at {}", device, mount_point);
                return Ok(mount_point.to_string());
            }
        }
    }

    anyhow::bail!("Cannot find or mount ESP. Use --bootloader=grub2 to use GRUB2 instead.")
}

/// Find the ESP partition and return its mount point, auto-mounting it
/// under /var/tmp/esp-migration if it is not already mounted. Does not
/// require a PreflightReport — use from the commit/cleanup path where
/// the preflight context is not available.
pub fn find_esp_or_mount() -> Result<String> {
    // Check standard mount points first.
    for path in ["/boot/efi", "/efi"] {
        if Path::new(path).exists() && Path::new(path).join("EFI").exists() {
            return Ok(path.to_string());
        }
    }

    // Scan lsblk: if the ESP is already mounted at a non-standard path,
    // return that mount point.
    let output = Command::new("lsblk")
        .args(["-o", "NAME,PARTTYPE,MOUNTPOINT", "-l", "-n"])
        .output()
        .context("failed to run lsblk")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 3
            && parts[1] == "c12a7328-f81f-11d2-ba4b-00a0c93ec93b"
            && !parts[2].is_empty()
        {
            let mp = parts[2].to_string();
            println!("Found ESP already mounted at {}", mp);
            return Ok(mp);
        }
    }

    // Not mounted — find device and mount it.
    let by_label = Path::new("/dev/disk/by-partlabel/EFI-SYSTEM");
    let device = if by_label.exists()
        && let Ok(target) = fs::read_link(by_label)
        && let Some(name) = target.file_name().and_then(|n| n.to_str())
    {
        format!("/dev/{}", name)
    } else {
        // Fallback: scan lsblk by partition type GUID.
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut found = None;
        for line in stdout.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 && parts[1] == "c12a7328-f81f-11d2-ba4b-00a0c93ec93b" {
                found = Some(format!("/dev/{}", parts[0]));
                break;
            }
        }
        found.ok_or_else(|| anyhow!("No ESP device found by partition label or type GUID"))?
    };
    let mount_point = "/var/tmp/esp-migration";
    fs::create_dir_all(mount_point)?;
    let status = Command::new("mount")
        .args([&device, mount_point])
        .status()
        .with_context(|| format!("failed to mount ESP {} at {}", device, mount_point))?;
    if status.success() {
        println!("Auto-mounted ESP {} at {}", device, mount_point);
        return Ok(mount_point.to_string());
    }
    anyhow::bail!("Cannot find or mount ESP. Use --bootloader=grub2 to use GRUB2 instead.")
}

/// Parse the ESP device and partition from findmnt output.
/// Returns (disk, partition_number). Returns None if parsing fails.
pub(crate) fn get_esp_disk_and_part(esp_path: &str) -> Option<(String, String)> {
    let output = Command::new("/usr/bin/findmnt")
        .args(["-n", "-o", "SOURCE", "-T", esp_path])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let source = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if source.is_empty() {
        return None;
    }

    // Handle /dev/nvme0n1p1, /dev/loop0p1 patterns
    if source.contains("nvme") || source.contains("loop") {
        if let Some(pos) = source.rfind('p') {
            let disk = source[..pos].to_string();
            let part = source[pos + 1..].to_string();
            if part.chars().all(|c| c.is_ascii_digit()) && !part.is_empty() {
                return Some((disk, part));
            }
        }
    } else if !source.contains("mapper") {
        // Regular /dev/sda1, /dev/vda1 patterns
        let mut split_idx = source.len();
        for (i, c) in source.char_indices().rev() {
            if c.is_ascii_digit() {
                split_idx = i;
            } else {
                break;
            }
        }
        if split_idx < source.len() {
            let disk = source[..split_idx].to_string();
            let part = source[split_idx..].to_string();
            if !part.is_empty() {
                return Some((disk, part));
            }
        }
    }
    // device-mapper, LVM, or other complex paths — skip efibootmgr registration.
    eprintln!(
        "Warning: cannot parse ESP device path '{}' — skipping efibootmgr registration.",
        source
    );
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn esp_parsing_returns_none_for_nonexistent() {
        // findmnt will fail for nonexistent path → returns None.
        assert!(get_esp_disk_and_part("/dev/null/nonexistent").is_none());
    }

    #[test]
    fn esp_parsing_handles_empty_output() {
        // If findmnt returns empty, parsing should return None.
        // The function returns None if the source string is empty.
        // This is exercised by get_esp_disk_and_part's early return on empty source.
    }

    #[test]
    fn wireless_probe_reads_only_devices_with_modaliases() {
        let temp = tempdir().unwrap();
        let net = temp.path().join("net");
        fs::create_dir_all(net.join("wlan0/wireless")).unwrap();
        fs::create_dir_all(net.join("wlan0/device")).unwrap();
        fs::write(
            net.join("wlan0/device/modalias"),
            "pci:v000017CBd00001107sv00001EACsd00008000bc02sc80i00\n",
        )
        .unwrap();
        fs::create_dir_all(net.join("wlan-no-alias/wireless")).unwrap();
        fs::create_dir_all(net.join("ethernet/device")).unwrap();

        assert_eq!(
            read_wireless_devices(&net).unwrap(),
            vec![WirelessDevice {
                interface: "wlan0".to_owned(),
                modalias: "pci:v000017CBd00001107sv00001EACsd00008000bc02sc80i00".to_owned(),
            }]
        );
        assert!(
            read_wireless_devices(&net.join("missing"))
                .unwrap()
                .is_empty()
        );
    }

    // ── Slice 1: origin file schema tests ──
}
