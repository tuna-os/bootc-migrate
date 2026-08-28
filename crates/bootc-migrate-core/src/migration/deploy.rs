//! Phase 4: stage the deployment.
//!
//! This module is a coordinator. It sequences the deployment layout
//! ([`super::deploy_layout`]), the `/etc` transition
//! ([`super::etc_transition`]), and target compatibility
//! ([`super::target_compat`]), and reports what they did. The policy for each
//! lives in those modules, not here.

use anyhow::{Context, Result};
use std::path::PathBuf;

use crate::VerityDigest;
use crate::migration::deploy_layout::{self, DeploymentLayout};
use crate::migration::etc_transition::EtcTransition;
use crate::migration::pull::PulledImage;
use crate::migration::target_compat;

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

    let layout = DeploymentLayout::for_verity(verity);
    let content_image = &pulled_image.image_reference;

    if dry_run {
        println!(
            "[DRY RUN] Would stage deployment at: {}",
            layout.deploy_dir.display()
        );
        return Ok(layout.deploy_dir);
    }

    if layout.already_staged && !force {
        refresh_staged_compat(content_image, &layout)?;
        println!(
            "Deployment already staged at {}. Skipping Phase 4.",
            layout.deploy_dir.display()
        );
        return Ok(layout.deploy_dir);
    }
    if layout.already_staged {
        println!("[phase4] --force: refreshing the existing staged deployment.");
    }
    layout.create_dirs()?;

    // 3-way /etc merge
    println!("Performing 3-way /etc merge...");
    if let Some(overrides) = etc_overrides {
        println!(
            "[phase4] applying {} Config Drift Review decision(s) to the /etc merge",
            overrides.decisions.len()
        );
    }
    let etc_report = EtcTransition {
        target_image: content_image,
        sealed_config,
        etc_dir: &layout.etc_dir,
        overrides: etc_overrides,
    }
    .run()?;
    if etc_report.fell_back_to_flat_copy {
        println!(
            "[phase4] /etc came from a flat copy of the live tree; target-specific /etc cleanup was skipped"
        );
    }
    if etc_report.removed_host_mounts > 0 {
        println!(
            "[phase4] removed {} host root or /var mount(s) from the target /etc/fstab",
            etc_report.removed_host_mounts
        );
    }

    let networkmanager_compat = target_compat::sanitize_staged_networkmanager_backend_compat(
        content_image,
        &layout.etc_dir,
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

    layout.stage_var_symlink()?;
    layout.write_origin(target_image, verity, &pulled_image.manifest_digest)?;
    layout.write_imginfo(&pulled_image.config_digest);

    deploy_layout::migrate_var_state()?;

    layout.install_runtime_composefs_mount();

    Ok(layout.deploy_dir)
}

/// Re-apply target compatibility fixes to an already-staged deployment.
///
/// A deployment staged by an older run can predate a compatibility fix, so
/// the skip path still refreshes these rather than returning untouched.
fn refresh_staged_compat(content_image: &str, layout: &DeploymentLayout) -> Result<()> {
    match target_compat::refresh_staged_nvidia_gbm_compat(content_image, &layout.etc_dir) {
        Ok(true) => {
            println!("[phase4] refreshed Mesa's GBM backend path for the staged NVIDIA target")
        }
        Ok(false) => {}
        Err(e) => {
            eprintln!("[phase4] warning: failed to refresh staged GBM backend paths: {e:#}")
        }
    }
    let networkmanager_compat = target_compat::sanitize_staged_networkmanager_backend_compat(
        content_image,
        &layout.etc_dir,
    )
    .context(
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
    Ok(())
}
