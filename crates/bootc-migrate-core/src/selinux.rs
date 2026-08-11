//! SELinux policy-type detection and cross-base `.autorelabel` scheduling
//! (issue #67, scenario C).
//!
//! When re-basing across distinct base images (e.g. Fedora-derived →
//! CentOS-derived), the target image may ship a different SELinux policy
//! type (`targeted` ↔ `mls`) or a different policy version. File labels
//! written by the source's policy are not guaranteed to be recognized by
//! the target's, so the migration schedules `/.autorelabel` when the base
//! family changes — the same mechanism Anaconda and other OS installers
//! use to force a full relabel on first boot.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

/// Parsed SELinux system-configuration from `/etc/selinux/config`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelinuxConfig {
    /// `SELINUX=enforcing|permissive|disabled`
    pub selinux: Option<String>,
    /// `SELINUXTYPE=targeted|mls|minimum`
    pub selinux_type: Option<String>,
}

/// Parse `/etc/selinux/config`. Missing keys and unparseable lines are
/// tolerated — a migration report should never fail because a config file
/// has a comment the parser doesn't understand.
pub fn parse_selinux_config(content: &str) -> SelinuxConfig {
    let mut config = SelinuxConfig {
        selinux: None,
        selinux_type: None,
    };
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim().to_ascii_uppercase();
            let v = v.trim().trim_matches('"').trim_matches('\'');
            if k == "SELINUX" && config.selinux.is_none() {
                config.selinux = Some(v.to_ascii_lowercase());
            } else if k == "SELINUXTYPE" && config.selinux_type.is_none() {
                config.selinux_type = Some(v.to_string());
            }
        }
    }
    config
}

/// Read the host's SELinux config from `/etc/selinux/config`. Returns
/// `None` when the file does not exist (SELinux not enabled/installed)
/// rather than erring — a system without `/etc/selinux/config` is one
/// the OS is not enforcing labels on.
pub fn read_host_selinux_config() -> Option<SelinuxConfig> {
    let content = fs::read_to_string("/etc/selinux/config").ok()?;
    Some(parse_selinux_config(&content))
}

/// Read a deployment's SELinux config from its `/etc/selinux/config`.
/// `None` when the file does not exist.
pub fn read_deployment_selinux_config(root: &Path) -> Option<SelinuxConfig> {
    let content = fs::read_to_string(root.join("etc/selinux/config")).ok()?;
    Some(parse_selinux_config(&content))
}

/// Read the target image's vendor SELinux config from its `usr/etc/selinux/config`
/// (OSTree's convention for vendor-default /etc). Falls back to
/// `etc/selinux/config` for composefs-style deployments.
pub fn read_target_selinux_config(staged_root: &Path) -> Option<SelinuxConfig> {
    let vendor = staged_root.join("usr/etc/selinux/config");
    if let Ok(content) = fs::read_to_string(&vendor) {
        return Some(parse_selinux_config(&content));
    }
    // Fall back: the deployment's own /etc/selinux/config.
    read_deployment_selinux_config(staged_root)
}

/// Whether the SELinux policy type changed between source and target, and
/// an autorelabel is advisable. True when:
/// - Both sides have a policy type configured,
/// - Neither is disabled,
/// - The types differ.
pub fn policy_type_changed(host: &SelinuxConfig, target: &SelinuxConfig) -> bool {
    let host_type = match &host.selinux_type {
        Some(t) => t.as_str(),
        None => return false,
    };
    let target_type = match &target.selinux_type {
        Some(t) => t.as_str(),
        None => return false,
    };
    let host_enforcing = host.selinux.as_deref().unwrap_or("") != "disabled";
    let target_enforcing = target.selinux.as_deref().unwrap_or("") != "disabled";
    host_enforcing && target_enforcing && host_type != target_type
}

/// Schedule `/.autorelabel` in the staged deployment root. Creating this
/// file at the root of the filesystem tells systemd's selinux-autorelabel
/// service (or the equivalent initrd mechanism) to run `fixfiles restore`
/// on first boot.
pub fn schedule_autorelabel(staged_root: &Path) -> Result<()> {
    let autorelabel = staged_root.join(".autorelabel");
    fs::write(&autorelabel, b"").with_context(|| {
        format!(
            "failed to create /.autorelabel in staged deployment at {}",
            staged_root.display()
        )
    })?;
    Ok(())
}

/// The full cross-base SELinux check: compare source and target policy
/// types, schedule `/.autorelabel` if they differ. Returns `true` if
/// autorelabel was scheduled (the caller should report this).
pub fn check_and_schedule_autorelabel(staged_root: &Path) -> Result<bool> {
    let Some(host_config) = read_host_selinux_config() else {
        return Ok(false);
    };
    let Some(target_config) = read_target_selinux_config(staged_root) else {
        return Ok(false);
    };
    if !policy_type_changed(&host_config, &target_config) {
        return Ok(false);
    }
    schedule_autorelabel(staged_root)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_standard_config() {
        let config = parse_selinux_config(
            "# This file controls the state of SELinux on the system.\n\
             SELINUX=enforcing\n\
             # SELINUXTYPE= can take one of these three values:\n\
             SELINUXTYPE=targeted\n",
        );
        assert_eq!(config.selinux.as_deref(), Some("enforcing"));
        assert_eq!(config.selinux_type.as_deref(), Some("targeted"));
    }

    #[test]
    fn parse_quoted_values() {
        let config = parse_selinux_config(
            "SELINUX=\"permissive\"\nSELINUXTYPE='mls'\n",
        );
        assert_eq!(config.selinux.as_deref(), Some("permissive"));
        assert_eq!(config.selinux_type.as_deref(), Some("mls"));
    }

    #[test]
    fn parse_missing_keys() {
        let config = parse_selinux_config("SELINUX=enforcing\n");
        assert_eq!(config.selinux.as_deref(), Some("enforcing"));
        assert_eq!(config.selinux_type, None);
    }

    #[test]
    fn parse_disabled() {
        let config = parse_selinux_config("SELINUX=disabled\n");
        assert_eq!(config.selinux.as_deref(), Some("disabled"));
    }

    #[test]
    fn policy_type_changed_detects_difference() {
        let host = SelinuxConfig {
            selinux: Some("enforcing".into()),
            selinux_type: Some("targeted".into()),
        };
        let target = SelinuxConfig {
            selinux: Some("enforcing".into()),
            selinux_type: Some("mls".into()),
        };
        assert!(policy_type_changed(&host, &target));
    }

    #[test]
    fn policy_type_changed_same_type_is_no_change() {
        let host = SelinuxConfig {
            selinux: Some("enforcing".into()),
            selinux_type: Some("targeted".into()),
        };
        let target = SelinuxConfig {
            selinux: Some("enforcing".into()),
            selinux_type: Some("targeted".into()),
        };
        assert!(!policy_type_changed(&host, &target));
    }

    #[test]
    fn policy_type_changed_disabled_is_ignored() {
        let host = SelinuxConfig {
            selinux: Some("disabled".into()),
            selinux_type: Some("targeted".into()),
        };
        let target = SelinuxConfig {
            selinux: Some("enforcing".into()),
            selinux_type: Some("mls".into()),
        };
        assert!(!policy_type_changed(&host, &target));
    }

    #[test]
    fn policy_type_changed_missing_type_is_no_change() {
        let host = SelinuxConfig {
            selinux: Some("enforcing".into()),
            selinux_type: None,
        };
        let target = SelinuxConfig {
            selinux: Some("enforcing".into()),
            selinux_type: Some("targeted".into()),
        };
        assert!(!policy_type_changed(&host, &target));
    }
}
