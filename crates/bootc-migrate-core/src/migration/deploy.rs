//! Phase 4: stage the deployment — /etc merge, /var migration, state root.

use super::*;

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
        match super::target_compat::refresh_staged_nvidia_gbm_compat(content_image, &etc_dir) {
            Ok(true) => {
                println!("[phase4] refreshed Mesa's GBM backend path for the staged NVIDIA target")
            }
            Ok(false) => {}
            Err(e) => {
                eprintln!("[phase4] warning: failed to refresh staged GBM backend paths: {e:#}")
            }
        }
        let networkmanager_compat =
            super::target_compat::sanitize_staged_networkmanager_backend_compat(content_image, &etc_dir).context(
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
    let etc_report = super::etc_transition::EtcTransition {
        target_image: content_image,
        sealed_config,
        etc_dir: &etc_dir,
        overrides: etc_overrides,
    }
    .run()?;
    if etc_report.fell_back_to_flat_copy {
        println!(
            "[phase4] /etc came from a flat copy of the live tree; target-specific /etc cleanup was skipped"
        );
    }
    let removed = etc_report.removed_host_mounts;
    if removed > 0 {
        println!(
            "[phase4] removed {removed} host root or /var mount(s) from the target /etc/fstab"
        );
    }
    let networkmanager_compat =
        super::target_compat::sanitize_staged_networkmanager_backend_compat(
            content_image,
            &etc_dir,
        )
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
    if let Ok(config_json) =
        crate::migration::image_access::inspect_image(&pulled_image.config_digest)
    {
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

#[cfg(test)]
mod tests {
    use super::*;

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
