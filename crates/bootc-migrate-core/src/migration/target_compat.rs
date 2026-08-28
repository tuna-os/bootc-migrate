//! Target-dependent compatibility policy, applied to a deployment that
//! `bootc switch`/Phase 4 has already staged.
//!
//! Two unrelated hazards, both of which depend on what the TARGET image ships
//! rather than on anything the source system did — which is why they live
//! together and apart from the `/etc` merge (#179):
//!
//! - **GBM backends.** Some target images replace Mesa's `GL/lib/gbm` symlink
//!   with a directory holding only the NVIDIA backend. On a hybrid laptop that
//!   makes Mutter fail to initialise the AMD GPU driving the internal panel.
//! - **NetworkManager Wi-Fi backend.** A target selecting `iwd` on a host set
//!   up for `wpa_supplicant` (or vice versa) leaves Wi-Fi dead after the
//!   re-base.
//!
//! Both are deliberately conservative: user-provided values are preserved, a
//! target drop-in wins over a guess, and a symlink is refused rather than
//! followed. Parsing is kept separate from mutation so those decisions are
//! table-tested without a staged deployment.

use anyhow::{Context, Result, anyhow};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use super::PodmanImageMount;

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

pub(crate) fn apply_target_gbm_backend_compat(
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

pub(crate) fn refresh_staged_nvidia_gbm_compat(target_image: &str, etc_dir: &Path) -> Result<bool> {
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
pub(crate) struct NetworkManagerBackendCompat {
    pub(crate) removed_iwd_settings: usize,
    pub(crate) removed_wpa_supplicant_mask: bool,
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

pub(crate) fn apply_target_networkmanager_backend_compat(
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

pub(crate) fn sanitize_staged_networkmanager_backend_compat(
    content_image: &str,
    etc_dir: &Path,
) -> Result<NetworkManagerBackendCompat> {
    let target = PodmanImageMount::new(content_image)
        .context("failed to mount target image for NetworkManager configuration inspection")?;
    apply_target_networkmanager_backend_compat(&target.path, etc_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

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
}
