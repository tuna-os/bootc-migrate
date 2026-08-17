//! Orchestration for deciding whether a re-base needs desktop migration.
//!
//! Detection and passwd parsing live in their focused modules; this module
//! owns the policy that combines their results into one plan for CLI callers.

use crate::de_detect::{self, DesktopDetection};
use crate::de_migrate::{self, DesktopEnvironment, UserHome};
use anyhow::{Context, Result};

/// A cross-desktop migration that should run for the listed users.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopMigrationPlan {
    pub from: DesktopEnvironment,
    pub to: DesktopEnvironment,
    pub users: Vec<UserHome>,
}

/// Why planning did or did not produce work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DesktopMigrationDecision {
    Disabled,
    NotCrossDesktop {
        host: DesktopDetection,
        target: DesktopDetection,
    },
    NoUsers {
        from: DesktopEnvironment,
        to: DesktopEnvironment,
    },
    Planned(DesktopMigrationPlan),
}

/// Inputs and I/O boundary for desktop-migration planning.
#[derive(Debug)]
pub struct DesktopMigrationController<'a> {
    enabled: bool,
    target_image: &'a str,
}

impl<'a> DesktopMigrationController<'a> {
    pub fn new(enabled: bool, target_image: &'a str) -> Self {
        Self {
            enabled,
            target_image,
        }
    }

    /// Detect both desktops and enumerate human accounts, without mutating
    /// either the host or the target image.
    pub fn plan(&self) -> Result<DesktopMigrationDecision> {
        if !self.enabled {
            return Ok(DesktopMigrationDecision::Disabled);
        }

        let host = de_detect::detect_host_desktop().context("detecting this host's desktop")?;
        let target = de_detect::detect_image_desktop(self.target_image)
            .with_context(|| format!("detecting the desktop shipped by {}", self.target_image))?;
        let passwd = std::fs::read_to_string("/etc/passwd").context("reading /etc/passwd")?;

        Ok(decide(host, target, &passwd))
    }
}

fn decide(
    host: DesktopDetection,
    target: DesktopDetection,
    passwd: &str,
) -> DesktopMigrationDecision {
    let Some((from, to)) = de_detect::cross_desktop_pair(&host, &target) else {
        return DesktopMigrationDecision::NotCrossDesktop { host, target };
    };

    let users = de_migrate::parse_user_homes(passwd);
    if users.is_empty() {
        DesktopMigrationDecision::NoUsers { from, to }
    } else {
        DesktopMigrationDecision::Planned(DesktopMigrationPlan { from, to, users })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planning_decisions_are_separate_from_io() {
        let cases = [
            (
                DesktopDetection::Single(DesktopEnvironment::Gnome),
                DesktopDetection::Single(DesktopEnvironment::Gnome),
                "alice:x:1000:1000::/home/alice:/bin/bash\n",
                "same",
            ),
            (
                DesktopDetection::Single(DesktopEnvironment::Gnome),
                DesktopDetection::Single(DesktopEnvironment::Kde),
                "root:x:0:0::/root:/bin/bash\n",
                "no-users",
            ),
            (
                DesktopDetection::Single(DesktopEnvironment::Gnome),
                DesktopDetection::Single(DesktopEnvironment::Kde),
                "alice:x:1000:1000::/home/alice:/bin/bash\n",
                "planned",
            ),
        ];

        for (host, target, passwd, expected) in cases {
            let actual = match decide(host, target, passwd) {
                DesktopMigrationDecision::NotCrossDesktop { .. } => "same",
                DesktopMigrationDecision::NoUsers { .. } => "no-users",
                DesktopMigrationDecision::Planned(_) => "planned",
                DesktopMigrationDecision::Disabled => "disabled",
            };
            assert_eq!(actual, expected);
        }
    }
}
