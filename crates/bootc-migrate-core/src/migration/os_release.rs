use anyhow::{Context, Result, anyhow};
use std::fs;
use std::path::Path;

/// Represents key fields from /etc/os-release used to construct BLS entry names.
#[derive(Debug, Clone)]
pub struct OsRelease {
    pub id: String,
    pub version_id: String,
    pub name: String,
    pub pretty_name: String,
}

/// Read /etc/os-release from a given root filesystem path (e.g., a mounted EROFS image).
pub fn read_os_release(root: &Path) -> Result<OsRelease> {
    let path = root.join("etc/os-release");
    let content = fs::read_to_string(&path)
        .with_context(|| format!("failed to read os-release from {}", path.display()))?;

    let mut id = String::new();
    let mut version_id = String::new();
    let mut name = String::new();
    let mut pretty_name = String::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(val) = parse_os_release_value(line, "ID=")
            && id.is_empty()
        {
            id = val.to_string();
        }
        if let Some(val) = parse_os_release_value(line, "VERSION_ID=")
            && version_id.is_empty()
        {
            version_id = val.to_string();
        }
        if let Some(val) = parse_os_release_value(line, "NAME=")
            && name.is_empty()
        {
            name = val.to_string();
        }
        if let Some(val) = parse_os_release_value(line, "PRETTY_NAME=")
            && pretty_name.is_empty()
        {
            pretty_name = val.to_string();
        }
    }

    if id.is_empty() {
        return Err(anyhow!("ID not found in os-release"));
    }

    Ok(OsRelease {
        id,
        version_id,
        name,
        pretty_name,
    })
}

fn parse_os_release_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    if !line.starts_with(key) {
        return None;
    }
    let val = &line[key.len()..];
    // Strip surrounding quotes
    let val = val.strip_prefix('"').unwrap_or(val);
    let val = val.strip_suffix('"').unwrap_or(val);
    let val = val.strip_prefix('\'').unwrap_or(val);
    let val = val.strip_suffix('\'').unwrap_or(val);
    Some(val)
}

/// Build a BLS entry filename following the convention in SPECIFICATION.md §4.2:
///
/// Format: `bootc_{os_id}-{version}-{priority}.conf`
///
/// Where:
/// - `os_id` comes from /etc/os-release's ID (hyphens → underscores)
/// - `version` is the target image version (from os-release VERSION_ID)
/// - `priority` is an integer: higher = preferred by GRUB (DESCENDING sort)
pub fn bls_entry_filename(os: &OsRelease, verity_hash: &str, priority: u32) -> String {
    let id = os.id.replace('-', "_");
    let ver = if os.version_id.is_empty() {
        // Fall back to a short hash prefix; tolerate hashes shorter than 12 chars
        // rather than panicking on the slice.
        verity_hash.get(..12).unwrap_or(verity_hash).to_string()
    } else {
        os.version_id.clone()
    };
    format!("bootc_{id}-{ver}-{priority}.conf")
}

/// The title displayed in the boot menu for this entry. Matches bootc's convention:
/// `<NAME> <VERSION_ID> (<deployment-kind>)` — e.g. "Fedora Linux 42 (ostree:0)"
/// becomes "Fedora Linux 42 (composefs)" for the migrated target.
/// Falls back to PRETTY_NAME, then NAME, then the capitalized ID.
pub fn bls_entry_title(os: &OsRelease, kind: &str) -> String {
    let display_name = if !os.name.is_empty() && !os.version_id.is_empty() {
        format!("{} {}", os.name, os.version_id)
    } else if !os.pretty_name.is_empty() {
        os.pretty_name.clone()
    } else if !os.name.is_empty() {
        os.name.clone()
    } else if let Some(first) = os.id.chars().next() {
        let rest = &os.id[first.len_utf8()..];
        format!("{}{}", first.to_uppercase(), rest)
    } else {
        os.id.clone()
    };
    format!("{display_name} ({kind})")
}

#[cfg(test)]
mod tests {
    use super::*;

    // TDD tests for BLS entry naming.

    #[test]
    fn parses_os_release_basic() {
        let dir = tempfile::tempdir().unwrap();
        let etc = dir.path().join("etc");
        fs::create_dir_all(&etc).unwrap();
        fs::write(etc.join("os-release"), "ID=fedora\nVERSION_ID=41\n").unwrap();

        let os = read_os_release(dir.path()).unwrap();
        assert_eq!(os.id, "fedora");
        assert_eq!(os.version_id, "41");
    }

    #[test]
    fn parses_os_release_with_quotes() {
        let dir = tempfile::tempdir().unwrap();
        let etc = dir.path().join("etc");
        fs::create_dir_all(&etc).unwrap();
        fs::write(
            etc.join("os-release"),
            "ID=\"fedora\"\nVERSION_ID=\"41.20251125.0\"\n",
        )
        .unwrap();

        let os = read_os_release(dir.path()).unwrap();
        assert_eq!(os.id, "fedora");
        assert_eq!(os.version_id, "41.20251125.0");
    }

    #[test]
    fn os_release_missing_id_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let etc = dir.path().join("etc");
        fs::create_dir_all(&etc).unwrap();
        fs::write(etc.join("os-release"), "NAME=Fedora\n").unwrap();

        let result = read_os_release(dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn bls_entry_filename_cases() {
        // (id, version_id, hash, priority) -> expected filename.
        let cases = [
            // Basic id-version-priority composition.
            (
                "fedora",
                "41.20251125.0",
                "abc123def456",
                1,
                "bootc_fedora-41.20251125.0-1.conf",
            ),
            // Hyphens in the id become underscores (BLS filename safety).
            (
                "bluefin-dakota",
                "1.0",
                "hash123",
                1,
                "bootc_bluefin_dakota-1.0-1.conf",
            ),
            // Priority 0 is rendered literally.
            ("fedora", "41", "hash", 0, "bootc_fedora-41-0.conf"),
            // No version_id: fall back to the first 12 chars of the hash.
            (
                "dakota",
                "",
                "abc123def4567890abcdef",
                1,
                "bootc_dakota-abc123def456-1.conf",
            ),
            // No version and a short (<12 char) hash: use the hash as-is.
            ("dakota", "", "abc", 2, "bootc_dakota-abc-2.conf"),
        ];
        for (id, version_id, hash, priority, expected) in cases {
            let os = OsRelease {
                id: id.into(),
                version_id: version_id.into(),
                name: String::new(),
                pretty_name: String::new(),
            };
            assert_eq!(
                bls_entry_filename(&os, hash, priority),
                expected,
                "id={id} version={version_id} hash={hash} priority={priority}"
            );
        }
    }

    #[test]
    fn bls_entry_name_does_not_contain_literal_bluefin_dakota() {
        // Regression test: we must not hardcode "bluefin_dakota"
        let os = OsRelease {
            id: "dakota".into(),
            version_id: "42".into(),
            name: String::new(),
            pretty_name: String::new(),
        };
        let name = bls_entry_filename(&os, "abc123", 1);
        // The old code used "bootc_bluefin_dakota-{hash}.conf" — make sure the
        // string "bluefin_dakota" does not appear.
        assert!(!name.contains("bluefin_dakota"));
        assert!(name.contains("dakota"));
    }

    #[test]
    fn bls_entry_title_uses_name_and_version() {
        let os = OsRelease {
            id: "fedora".into(),
            version_id: "42".into(),
            name: "Fedora Linux".into(),
            pretty_name: String::new(),
        };
        let title = bls_entry_title(&os, "composefs");
        assert_eq!(title, "Fedora Linux 42 (composefs)");
    }

    #[test]
    fn bls_entry_title_falls_back_to_pretty_name() {
        let os = OsRelease {
            id: "fedora".into(),
            version_id: String::new(),
            name: String::new(),
            pretty_name: "Fedora Linux 42 (Container Image)".into(),
        };
        let title = bls_entry_title(&os, "composefs");
        assert_eq!(title, "Fedora Linux 42 (Container Image) (composefs)");
    }

    #[test]
    fn bls_entry_title_falls_back_to_id() {
        let os = OsRelease {
            id: "dakota".into(),
            version_id: String::new(),
            name: String::new(),
            pretty_name: String::new(),
        };
        let title = bls_entry_title(&os, "composefs");
        assert_eq!(title, "Dakota (composefs)");
    }
}
