//! Versioned image catalog. Network failure leaves the bundled catalog usable.
use super::ImageChoice;
use serde_json::Value;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

const URL: &str =
    "https://raw.githubusercontent.com/tuna-os/bootc-migrate/catalog-feed/images.json";
const BUNDLED: &str = include_str!("../../../../catalog/images.json");

pub(super) fn load() -> (Vec<ImageChoice>, &'static str) {
    static CATALOG: OnceLock<(Vec<ImageChoice>, &'static str)> = OnceLock::new();
    CATALOG.get_or_init(load_once).clone()
}

fn load_once() -> (Vec<ImageChoice>, &'static str) {
    let bundled = parse(BUNDLED).expect("bundled image catalog must be valid");
    if cfg!(test) {
        return (bundled, "bundled catalog");
    }
    let cache = cache_path();
    let output = Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--max-time",
            "5",
            "--max-filesize",
            "262144",
            URL,
        ])
        .output();
    if let Ok(output) = output
        && output.status.success()
        && let Ok(raw) = std::str::from_utf8(&output.stdout)
        && let Ok(rows) = parse(raw)
    {
        if let Some(path) = cache.as_ref()
            && let Some(parent) = path.parent()
            && std::fs::create_dir_all(parent).is_ok()
        {
            let _ = std::fs::write(path, raw);
        }
        return (rows, "latest online catalog");
    }
    if let Some(path) = cache
        && let Ok(raw) = std::fs::read_to_string(path)
        && let Ok(rows) = parse(&raw)
    {
        return (rows, "cached catalog (offline)");
    }
    (bundled, "bundled catalog (offline)")
}

fn cache_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
    Some(base.join("bootc-migrate/catalog.json"))
}

fn parse(raw: &str) -> Result<Vec<ImageChoice>, String> {
    let value: Value = serde_json::from_str(raw).map_err(|e| e.to_string())?;
    if value.get("version").and_then(Value::as_u64) != Some(1) {
        return Err("unsupported catalog version".into());
    }
    let entries = value
        .get("images")
        .and_then(Value::as_array)
        .ok_or("missing images")?;
    if entries.is_empty() || entries.len() > 512 {
        return Err("invalid catalog size".into());
    }
    let mut rows = Vec::new();
    for entry in entries {
        let name = entry
            .get("name")
            .and_then(Value::as_str)
            .ok_or("missing name")?;
        let image = entry
            .get("image")
            .and_then(Value::as_str)
            .ok_or("missing image")?;
        let backend = entry
            .get("backend")
            .and_then(Value::as_str)
            .ok_or("missing backend")?;
        let published = entry
            .get("published")
            .and_then(Value::as_bool)
            .ok_or("missing published")?;
        if !matches!(backend, "ostree" | "composefs")
            || name.len() > 64
            || name.chars().any(char::is_control)
            || (!image.is_empty() && !valid_image(image))
            || (published && image.is_empty())
        {
            return Err("invalid catalog entry".into());
        }
        if rows
            .iter()
            .any(|r: &ImageChoice| r.image == image && published)
        {
            return Err("duplicate image".into());
        }
        rows.push(ImageChoice {
            label: name.to_owned(),
            image: image.to_owned(),
            note: if published {
                backend.to_owned()
            } else {
                if image.is_empty() {
                    "coming soon"
                } else {
                    "unavailable"
                }
                .to_owned()
            },
            custom: false,
            backend: backend.to_owned(),
            published,
        });
    }
    Ok(rows)
}

fn valid_image(image: &str) -> bool {
    image.starts_with("ghcr.io/")
        && image.len() <= 200
        && image.contains(':')
        && image
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"./_:-".contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bundled_catalog_is_valid_and_covers_requested_families() {
        let rows = parse(BUNDLED).unwrap();
        for name in [
            "Bazzite",
            "Aurora",
            "Bluefin",
            "Dakota",
            "Utah",
            "Skipjack",
            "Marlin",
            "Sailfin",
            "Bonito",
            "Flounder",
            "Grouper",
            "Guppy",
            "Albacore",
            "Yellowfin",
            "Zirconium",
        ] {
            assert!(rows.iter().any(|r| r.label.starts_with(name)), "{name}");
        }
        assert!(!rows.iter().find(|r| r.label == "Utah").unwrap().published);
    }
    #[test]
    fn rejects_bad_catalogs() {
        for raw in [
            r#"{"version":2,"images":[]}"#,
            r#"{"version":1,"images":[{"name":"x","image":"ghcr.io/x/a:stable;touch /tmp/x","backend":"composefs","published":true}]}"#,
        ] {
            assert!(parse(raw).is_err());
        }
    }
}
