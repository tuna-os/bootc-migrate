//! Shared application replacement catalog for package-host reports and wootc.
//!
//! The reviewed JSON is published with the image catalog. Keeping parsing and
//! matching here makes package inventory collection independent from replacement
//! policy and gives consumers a versioned, typed contract.

use std::collections::HashSet;
use std::sync::LazyLock;

use serde::Deserialize;

use crate::source_host::PackageDb;

const BUNDLED: &str = include_str!("../../../catalog/apps.json");

/// Versioned application replacement catalog.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicationCatalog {
    pub version: u32,
    pub applications: Vec<ApplicationMapping>,
}

/// One known desktop application and its Flatpak replacement.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicationMapping {
    pub id: String,
    pub flatpak: String,
    pub portability: Portability,
    pub note: String,
    #[serde(rename = "match")]
    pub matches: ApplicationMatches,
}

/// How much application state can move without a new sign-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Portability {
    Portable,
    Signin,
    None,
}

/// Names used by each source package inventory.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApplicationMatches {
    pub windows_display_name: Vec<String>,
    pub rpm: Vec<String>,
    pub deb: Vec<String>,
    pub pacman: Vec<String>,
    #[serde(default)]
    pub apk: Vec<String>,
}

impl ApplicationMatches {
    fn packages(&self, db: PackageDb) -> &[String] {
        match db {
            PackageDb::Rpm => &self.rpm,
            PackageDb::Dpkg => &self.deb,
            PackageDb::Pacman => &self.pacman,
            PackageDb::Apk => &self.apk,
        }
    }
}

impl ApplicationCatalog {
    /// Parse and semantically validate a catalog.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let catalog: Self = serde_json::from_str(raw).map_err(|e| e.to_string())?;
        catalog.validate()?;
        Ok(catalog)
    }

    /// Return the application whose package alias exactly matches `package`.
    pub fn find_package(&self, db: PackageDb, package: &str) -> Option<&ApplicationMapping> {
        self.applications
            .iter()
            .find(|app| app.matches.packages(db).iter().any(|name| name == package))
    }

    /// Return the longest case-insensitive substring match for a Windows display name.
    ///
    /// Longest-match selection is deterministic when aliases overlap, unlike the
    /// associative-array iteration used by wootc's legacy detector.
    pub fn find_windows_display_name(&self, display_name: &str) -> Option<&ApplicationMapping> {
        self.applications
            .iter()
            .flat_map(|app| {
                app.matches
                    .windows_display_name
                    .iter()
                    .filter(move |alias| contains_ascii_case_insensitive(display_name, alias))
                    .map(move |alias| (alias.len(), app))
            })
            .max_by_key(|(length, _)| *length)
            .map(|(_, app)| app)
    }

    fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err(format!(
                "unsupported application catalog version {}",
                self.version
            ));
        }
        if self.applications.is_empty() || self.applications.len() > 512 {
            return Err("invalid application count".into());
        }

        let mut ids = HashSet::new();
        let mut windows = HashSet::new();
        let mut rpm = HashSet::new();
        let mut deb = HashSet::new();
        let mut pacman = HashSet::new();
        for app in &self.applications {
            if app.id.is_empty()
                || !app
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-+".contains(&b))
                || !ids.insert(app.id.as_str())
            {
                return Err(format!("invalid or duplicate application id `{}`", app.id));
            }
            if !valid_flatpak_id(&app.flatpak) {
                return Err(format!("invalid Flatpak id `{}`", app.flatpak));
            }
            if app.note.trim().is_empty()
                || app.note.len() > 512
                || app.note.chars().any(char::is_control)
            {
                return Err(format!("invalid note for `{}`", app.id));
            }
            if app.matches.windows_display_name.is_empty()
                && app.matches.rpm.is_empty()
                && app.matches.deb.is_empty()
                && app.matches.pacman.is_empty()
            {
                return Err(format!("application `{}` has no match aliases", app.id));
            }
            validate_aliases(
                "windowsDisplayName",
                &app.id,
                &app.matches.windows_display_name,
                &mut windows,
                true,
            )?;
            validate_aliases("rpm", &app.id, &app.matches.rpm, &mut rpm, false)?;
            validate_aliases("deb", &app.id, &app.matches.deb, &mut deb, false)?;
            validate_aliases("pacman", &app.id, &app.matches.pacman, &mut pacman, false)?;
        }
        Ok(())
    }
}

/// The reviewed catalog bundled into the binary for offline consumers.
pub fn bundled() -> &'static ApplicationCatalog {
    static CATALOG: LazyLock<ApplicationCatalog> = LazyLock::new(|| {
        ApplicationCatalog::parse(BUNDLED).expect("bundled application catalog must be valid")
    });
    &CATALOG
}

fn contains_ascii_case_insensitive(haystack: &str, needle: &str) -> bool {
    !needle.is_empty()
        && haystack.as_bytes().windows(needle.len()).any(|candidate| {
            candidate
                .iter()
                .zip(needle.bytes())
                .all(|(left, right)| left.eq_ignore_ascii_case(&right))
        })
}

fn valid_flatpak_id(id: &str) -> bool {
    id.len() <= 255
        && id.split('.').count() >= 3
        && id.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        })
}

fn validate_aliases<'a>(
    kind: &str,
    app_id: &str,
    aliases: &'a [String],
    seen: &mut HashSet<&'a str>,
    require_lowercase: bool,
) -> Result<(), String> {
    for alias in aliases {
        if alias.trim() != alias
            || alias.is_empty()
            || alias.len() > 128
            || alias.chars().any(char::is_control)
            || (!require_lowercase && alias.chars().any(char::is_whitespace))
            || (require_lowercase && alias.bytes().any(|b| b.is_ascii_uppercase()))
            || !seen.insert(alias)
        {
            return Err(format!(
                "invalid or duplicate {kind} alias `{alias}` for `{app_id}`"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_catalog_matches_packages_from_each_supported_database() {
        let catalog = bundled();
        for (db, package, expected_id, expected_flatpak) in [
            (PackageDb::Rpm, "obs-studio", "obs", "com.obsproject.Studio"),
            (
                PackageDb::Dpkg,
                "handbrake-gtk",
                "handbrake",
                "fr.handbrake.ghb",
            ),
            (
                PackageDb::Pacman,
                "libreoffice-fresh",
                "libreoffice",
                "org.libreoffice.LibreOffice",
            ),
        ] {
            let app = catalog
                .find_package(db, package)
                .unwrap_or_else(|| panic!("no mapping for {db} package {package}"));
            assert_eq!(app.id, expected_id);
            assert_eq!(app.flatpak, expected_flatpak);
        }
        assert!(
            catalog
                .find_package(PackageDb::Rpm, "unmapped-package")
                .is_none()
        );
    }

    #[test]
    fn windows_matching_is_case_insensitive_and_prefers_the_longest_alias() {
        let catalog = ApplicationCatalog::parse(
            r#"{
              "version": 1,
              "applications": [
                {"id":"code","flatpak":"org.example.Code","portability":"none","note":"Short match.",
                 "match":{"windowsDisplayName":["code"],"rpm":[],"deb":[],"pacman":[]}},
                {"id":"vscode","flatpak":"org.example.VSCode","portability":"portable","note":"Specific match.",
                 "match":{"windowsDisplayName":["visual studio code"],"rpm":[],"deb":[],"pacman":[]}}
              ]
            }"#,
        )
        .unwrap();
        let app = catalog
            .find_windows_display_name("Microsoft Visual Studio Code (User)")
            .unwrap();
        assert_eq!(app.id, "vscode");
    }

    #[test]
    fn rejects_invalid_catalog_contracts() {
        for (label, raw, needle) in [
            (
                "version",
                r#"{"version":2,"applications":[{"id":"a","flatpak":"org.example.A","portability":"none","note":"x","match":{"windowsDisplayName":[],"rpm":[],"deb":[],"pacman":[]}}]}"#,
                "unsupported application catalog version",
            ),
            (
                "duplicate package alias",
                r#"{"version":1,"applications":[{"id":"a","flatpak":"org.example.A","portability":"none","note":"x","match":{"windowsDisplayName":[],"rpm":["same"],"deb":[],"pacman":[]}},{"id":"b","flatpak":"org.example.B","portability":"none","note":"x","match":{"windowsDisplayName":[],"rpm":["same"],"deb":[],"pacman":[]}}]}"#,
                "duplicate rpm alias",
            ),
            (
                "unmatchable application",
                r#"{"version":1,"applications":[{"id":"a","flatpak":"org.example.A","portability":"none","note":"x","match":{"windowsDisplayName":[],"rpm":[],"deb":[],"pacman":[]}}]}"#,
                "has no match aliases",
            ),
            (
                "unknown field",
                r#"{"version":1,"extra":true,"applications":[]}"#,
                "unknown field",
            ),
        ] {
            let error = ApplicationCatalog::parse(raw).unwrap_err();
            assert!(error.contains(needle), "{label}: {error}");
        }
    }
}
