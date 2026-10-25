//! The record flatpak keeps of the apps the old OS preinstalled.
//!
//! `flatpak preinstall` writes each ref it installs to the `xa.preinstalled`
//! key in the `[core]` group of the installation's repo config. On a later
//! run it uninstalls every recorded ref that no file in `preinstall.d` lists
//! any more. That is correct when one OS updates itself. After a re-base it
//! is wrong: the record names the old OS's apps, the new OS lists its own,
//! and its first-boot `flatpak preinstall` removes the user's apps (Firefox
//! from a tunaOS host that re-bases onto Utah, for example).
//!
//! The re-base removes the key from the carried `/var`. With no key, flatpak
//! records only the installed refs that the new `preinstall.d` lists, so the
//! old OS's apps stay installed as the user's own.

use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

/// The system installation's repo config, relative to a `/var` root.
pub const SYSTEM_REPO_CONFIG: &str = "lib/flatpak/repo/config";

const KEY: &str = "xa.preinstalled";

/// The repo config without the preinstall record, and the refs the record
/// named. `None` when the config has no record.
pub fn strip_preinstalled(config: &str) -> Option<(String, Vec<String>)> {
    let mut in_core = false;
    let mut refs = None;
    let mut kept = String::with_capacity(config.len());
    for line in config.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_core = trimmed == "[core]";
        } else if in_core
            && let Some((key, value)) = trimmed.split_once('=')
            && key.trim() == KEY
        {
            let named = value
                .split(';')
                .map(str::trim)
                .filter(|r| !r.is_empty())
                .map(String::from);
            refs.get_or_insert_with(Vec::new).extend(named);
            continue;
        }
        kept.push_str(line);
    }
    refs.map(|refs| (kept, refs))
}

/// Remove the preinstall record from the system installation under
/// `var_root`. Returns the refs it named, empty when there was no record or
/// no installation. The file is rewritten in place, so its owner, mode and
/// extended attributes stay.
pub fn forget_preinstalled(var_root: &Path) -> Result<Vec<String>> {
    let path = var_root.join(SYSTEM_REPO_CONFIG);
    let config = match fs::read_to_string(&path) {
        Ok(config) => config,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("failed to read {}", path.display())),
    };
    let Some((kept, refs)) = strip_preinstalled(&config) else {
        return Ok(Vec::new());
    };
    fs::write(&path, kept).with_context(|| format!("failed to rewrite {}", path.display()))?;
    Ok(refs)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = "[core]\nrepo_version=1\nmode=bare-user-only\n\
        xa.preinstalled=app/org.mozilla.firefox/x86_64/stable;app/com.mattjakeman.ExtensionManager/x86_64/stable;\n\
        min-free-space-size=500MB\n\n[remote \"flathub\"]\nurl=https://dl.flathub.org/repo/\n\
        xa.preinstalled=not-in-core\n";

    #[test]
    fn the_record_leaves_core_and_nothing_else_changes() {
        let (kept, refs) = strip_preinstalled(CONFIG).unwrap();
        assert_eq!(
            refs,
            [
                "app/org.mozilla.firefox/x86_64/stable",
                "app/com.mattjakeman.ExtensionManager/x86_64/stable"
            ]
        );
        assert_eq!(
            kept,
            "[core]\nrepo_version=1\nmode=bare-user-only\nmin-free-space-size=500MB\n\n\
             [remote \"flathub\"]\nurl=https://dl.flathub.org/repo/\nxa.preinstalled=not-in-core\n"
        );
    }

    #[test]
    fn a_config_without_the_record_is_left_alone() {
        assert_eq!(strip_preinstalled("[core]\nrepo_version=1\n"), None);
        assert_eq!(strip_preinstalled(""), None);
    }

    #[test]
    fn forget_rewrites_the_carried_config_and_tolerates_no_installation() {
        let var = tempfile::tempdir().unwrap();
        assert!(forget_preinstalled(var.path()).unwrap().is_empty());

        let path = var.path().join(SYSTEM_REPO_CONFIG);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, CONFIG).unwrap();
        assert_eq!(forget_preinstalled(var.path()).unwrap().len(), 2);
        let after = fs::read_to_string(&path).unwrap();
        assert!(!after.contains("x86_64/stable"), "{after}");
        assert!(forget_preinstalled(var.path()).unwrap().is_empty());
    }
}
