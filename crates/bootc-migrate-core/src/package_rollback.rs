//! Rollback artifacts for package-host migrations (issue #374, #461).
//!
//! `bootc install to-existing-root` keeps the package host's files on disk but
//! replaces `/boot`. The package-install engine calls [`snapshot_source_boot`]
//! before that destructive step and [`stage_rollback`] afterwards. The staged
//! entry is deliberately independent of the target deployment: it points at
//! the package host's kernel, initramfs, devicetree (if present) and original
//! kernel command line.
//!
//! The live filesystem operations are kept here so the package-install engine
//! does not need to know how BLS and legacy GRUB entries are parsed. The
//! parsers and renderers are pure and table-testable. [`run_undo`] only changes
//! the bootloader default; it never removes the new deployment or the saved
//! artifacts.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::boot_cleanup::live::{NvramSnapshot, snapshot_from_output};

/// Persistent state for a package-host rollback, kept on the old root's `/var`.
pub const STATE_DIR: &str = "/var/lib/bootc-rebase/package-rollback";
/// JSON snapshot filename below [`STATE_DIR`].
pub const SNAPSHOT_FILE: &str = "snapshot.json";
/// Directory below `/boot` containing the copied package-host artifacts.
pub const ROLLBACK_DIR: &str = "bootc-migrate-rollback";
/// Stable BLS entry filename and GRUB entry id.
pub const ROLLBACK_ENTRY: &str = "bootc-migrate-rollback.conf";
/// The stem used by GRUB's `saved_entry` and systemd-boot's `set-default`.
pub const ROLLBACK_ENTRY_ID: &str = "bootc-migrate-rollback";
/// Schema of the snapshot JSON this build writes and understands.
pub const SNAPSHOT_SCHEMA: u32 = 1;

/// Whether the source entry came from BLS or a legacy GRUB configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BootEntryFormat {
    Bls,
    Grub,
}

/// Identity used in the user-visible rollback title.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceIdentity {
    pub id: String,
    pub version_id: String,
    pub name: String,
    pub pretty_name: String,
}

impl SourceIdentity {
    /// The least surprising human-readable name available from os-release.
    pub fn label(&self) -> String {
        if !self.pretty_name.is_empty() {
            return self.pretty_name.clone();
        }
        if !self.name.is_empty() {
            if self.version_id.is_empty() {
                return self.name.clone();
            }
            return format!("{} {}", self.name, self.version_id);
        }
        if !self.id.is_empty() {
            let mut chars = self.id.chars();
            if let Some(first) = chars.next() {
                return format!("{}{}", first.to_uppercase(), chars.as_str());
            }
        }
        "Linux".to_string()
    }
}

/// Parse the fields needed from an os-release file without doing I/O.
pub fn parse_source_identity(content: &str) -> Option<SourceIdentity> {
    let mut identity = SourceIdentity {
        id: String::new(),
        version_id: String::new(),
        name: String::new(),
        pretty_name: String::new(),
    };
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value
            .trim()
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| {
                value
                    .trim()
                    .strip_prefix('\'')
                    .and_then(|v| v.strip_suffix('\''))
            })
            .unwrap_or_else(|| value.trim())
            .to_string();
        match key {
            "ID" => identity.id = value,
            "VERSION_ID" => identity.version_id = value,
            "NAME" => identity.name = value,
            "PRETTY_NAME" => identity.pretty_name = value,
            _ => {}
        }
    }
    (!identity.id.is_empty()).then_some(identity)
}

/// Read a source identity from `root/etc/os-release`.
pub fn source_identity_from_root(root: &Path) -> Result<SourceIdentity> {
    let path = root.join("etc/os-release");
    let content = fs::read_to_string(&path)
        .with_context(|| format!("reading source identity from {}", path.display()))?;
    parse_source_identity(&content)
        .ok_or_else(|| anyhow::anyhow!("{} has no usable ID= field", path.display()))
}

/// One parsed boot entry. Paths are as written in the source BLS/GRUB entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootEntry {
    pub filename: String,
    pub title: String,
    pub version: String,
    pub linux: String,
    pub initrds: Vec<String>,
    pub devicetree: Option<String>,
    pub options: String,
}

/// A BLS entry plus the file timestamp used to choose the newest kernel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlsCandidate {
    pub entry: BootEntry,
    pub modified_unix_secs: u64,
}

/// Parse one BLS Type 1 entry. Malformed entries return `None` so an unrelated
/// vendor entry cannot prevent a valid package-host entry from being saved.
pub fn parse_bls_entry(filename: &str, content: &str) -> Option<BootEntry> {
    let mut title = String::new();
    let mut version = String::new();
    let mut linux = String::new();
    let mut initrds = Vec::new();
    let mut devicetree = None;
    let mut options = String::new();

    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.splitn(2, |c: char| c.is_whitespace());
        let key = fields.next()?;
        let value = fields.next().unwrap_or_default().trim();
        match key {
            "title" => title = value.to_string(),
            "version" => version = value.to_string(),
            "linux" => linux = value.to_string(),
            "initrd" => initrds.push(value.to_string()),
            "devicetree" => devicetree = Some(value.to_string()),
            "options" => options = value.to_string(),
            _ => {}
        }
    }

    if linux.is_empty() || initrds.is_empty() {
        return None;
    }
    if title.is_empty() {
        title = "Linux".to_string();
    }
    if version.is_empty() {
        version = filename
            .strip_suffix(".conf")
            .unwrap_or(filename)
            .to_string();
    }
    Some(BootEntry {
        filename: filename.to_string(),
        title,
        version,
        linux,
        initrds,
        devicetree,
        options,
    })
}

/// Select the newest BLS candidate. BLS files are rewritten on kernel updates,
/// so mtime is authoritative; version and filename make ties deterministic.
pub fn select_newest_bls(candidates: &[BlsCandidate]) -> Option<&BlsCandidate> {
    candidates.iter().max_by(|left, right| {
        left.modified_unix_secs
            .cmp(&right.modified_unix_secs)
            .then_with(|| left.entry.version.cmp(&right.entry.version))
            .then_with(|| left.entry.filename.cmp(&right.entry.filename))
    })
}

fn parse_quoted_prefix(value: &str) -> String {
    let value = value.trim_start();
    let Some(quote) = value.chars().next() else {
        return String::new();
    };
    if quote != '\'' && quote != '"' {
        return value
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string();
    }
    value[quote.len_utf8()..]
        .find(quote)
        .map(|end| value[quote.len_utf8()..][..end].to_string())
        .unwrap_or_else(|| value[quote.len_utf8()..].to_string())
}

fn kernel_version(path: &str) -> String {
    let name = Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path);
    ["vmlinuz-", "vmlinux-", "kernel-"]
        .iter()
        .find_map(|prefix| name.strip_prefix(prefix))
        .unwrap_or(name)
        .to_string()
}

/// Parse ordinary `menuentry` blocks from a GRUB configuration.
///
/// This intentionally extracts only the kernel contract needed for rollback;
/// it does not interpret GRUB scripting or execute any configuration text.
pub fn parse_grub_menuentries(content: &str) -> Vec<BootEntry> {
    let mut entries = Vec::new();
    let mut current: Option<BootEntry> = None;

    let finish = |current: &mut Option<BootEntry>, entries: &mut Vec<BootEntry>| {
        if let Some(entry) = current.take()
            && !entry.linux.is_empty()
            && !entry.initrds.is_empty()
        {
            entries.push(entry);
        }
    };

    for raw in content.lines() {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix("menuentry ") {
            finish(&mut current, &mut entries);
            current = Some(BootEntry {
                filename: String::new(),
                title: parse_quoted_prefix(rest),
                version: String::new(),
                linux: String::new(),
                initrds: Vec::new(),
                devicetree: None,
                options: String::new(),
            });
            continue;
        }

        let Some(entry) = current.as_mut() else {
            continue;
        };
        let mut fields = line.split_whitespace();
        let Some(command) = fields.next() else {
            continue;
        };
        match command {
            "linux" | "linuxefi" | "linux16" => {
                if let Some(path) = fields.next() {
                    entry.linux = path.to_string();
                    entry.version = kernel_version(path);
                    entry.options = fields.collect::<Vec<_>>().join(" ");
                }
            }
            "initrd" | "initrdefi" | "initrd16" => {
                if let Some(path) = fields.next() {
                    entry.initrds.push(path.to_string());
                }
            }
            "devicetree" => {
                if let Some(path) = fields.next() {
                    entry.devicetree = Some(path.to_string());
                }
            }
            _ => {}
        }
    }
    finish(&mut current, &mut entries);
    entries
}

fn boot_relative_path(path: &str) -> Result<PathBuf> {
    let trimmed = path.trim();
    let relative = trimmed.strip_prefix('/').unwrap_or(trimmed);
    let relative = relative.strip_prefix("boot/").unwrap_or(relative);
    if relative.is_empty() {
        bail!("boot artifact path is empty");
    }
    let path = Path::new(relative);
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("boot artifact path is not relative and clean: {trimmed}");
    }
    Ok(path.to_path_buf())
}

fn source_boot_file(boot_root: &Path, path: &str) -> Result<PathBuf> {
    let relative = boot_relative_path(path)?;
    let full = boot_root.join(relative);
    if !full.is_file() {
        bail!("source boot artifact is missing: {}", full.display());
    }
    Ok(full)
}

fn modified_unix_secs(path: &Path) -> u64 {
    fs::metadata(path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// Capture time for the NVRAM snapshot; 0 on a clock set before the epoch,
/// which only makes the stamp useless, never the snapshot.
fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn valid_entry(boot_root: &Path, mut entry: BootEntry) -> Option<BootEntry> {
    if source_boot_file(boot_root, &entry.linux).is_err()
        || entry
            .initrds
            .iter()
            .any(|path| source_boot_file(boot_root, path).is_err())
    {
        return None;
    }
    if let Some(dtb) = &entry.devicetree
        && source_boot_file(boot_root, dtb).is_err()
    {
        return None;
    }
    if entry.version.is_empty() {
        entry.version = kernel_version(&entry.linux);
    }
    Some(entry)
}

fn discover_bls(boot_root: &Path) -> Result<Option<BootEntry>> {
    let entries_dir = boot_root.join("loader/entries");
    if !entries_dir.is_dir() {
        return Ok(None);
    }
    let mut candidates = Vec::new();
    for item in
        fs::read_dir(&entries_dir).with_context(|| format!("reading {}", entries_dir.display()))?
    {
        let item = item?;
        let path = item.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("conf") {
            continue;
        }
        let filename = item.file_name().to_string_lossy().into_owned();
        let Some(entry) = parse_bls_entry(&filename, &fs::read_to_string(&path)?) else {
            continue;
        };
        let Some(entry) = valid_entry(boot_root, entry) else {
            continue;
        };
        candidates.push(BlsCandidate {
            entry,
            modified_unix_secs: modified_unix_secs(&path),
        });
    }
    Ok(select_newest_bls(&candidates).map(|candidate| candidate.entry.clone()))
}

fn grub_config_paths(boot_root: &Path) -> [PathBuf; 3] {
    [
        boot_root.join("grub2/grub.cfg"),
        boot_root.join("grub/grub.cfg"),
        boot_root.join("grub.cfg"),
    ]
}

fn discover_grub(boot_root: &Path) -> Result<Option<BootEntry>> {
    for path in grub_config_paths(boot_root) {
        if !path.is_file() {
            continue;
        }
        let content = fs::read_to_string(&path)
            .with_context(|| format!("reading source GRUB configuration {}", path.display()))?;
        for entry in parse_grub_menuentries(&content) {
            if let Some(entry) = valid_entry(boot_root, entry) {
                return Ok(Some(entry));
            }
        }
    }
    Ok(None)
}

fn discover_kernel_fallback(
    boot_root: &Path,
    identity: &SourceIdentity,
    cmdline: &str,
) -> Result<BootEntry> {
    let mut kernels = Vec::new();
    for item in
        fs::read_dir(boot_root).with_context(|| format!("reading {}", boot_root.display()))?
    {
        let item = item?;
        let name = item.file_name().to_string_lossy().into_owned();
        if !item.path().is_file() {
            continue;
        }
        if name.starts_with("vmlinuz") || name.starts_with("vmlinux") || name == "kernel" {
            kernels.push((item.path(), name));
        }
    }
    kernels.sort_by_key(|(path, name)| (modified_unix_secs(path), name.clone()));
    let (_, kernel_name) = kernels
        .pop()
        .ok_or_else(|| anyhow::anyhow!("no kernel was found below {}", boot_root.display()))?;
    let version = kernel_version(&kernel_name);
    let initrd_names = [
        format!("initramfs-{version}.img"),
        format!("initrd.img-{version}"),
        format!("initrd-{version}"),
        "initramfs".to_string(),
        "initrd.img".to_string(),
        "initrd".to_string(),
    ];
    let initrd = initrd_names
        .into_iter()
        .find(|name| boot_root.join(name).is_file())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no initramfs matching kernel {kernel_name} was found below {}",
                boot_root.display()
            )
        })?;

    // Check for devicetree file
    let mut devicetree = None;
    if let Ok(entries) = fs::read_dir(boot_root) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".dtb") && entry.path().is_file() {
                devicetree = Some(format!("/{name}"));
                break;
            }
        }
    }

    let options = cmdline.trim().to_string();
    if options.is_empty() {
        bail!("the package host has no kernel command line to preserve");
    }
    Ok(BootEntry {
        filename: String::new(),
        title: format!("{} {}", identity.name, identity.version_id)
            .trim()
            .to_string(),
        version,
        linux: format!("/{kernel_name}"),
        initrds: vec![format!("/{initrd}")],
        devicetree,
        options,
    })
}

#[derive(Debug, Clone)]
struct DiscoveredEntry {
    entry: BootEntry,
    format: BootEntryFormat,
}

fn discover_source_entry(
    boot_root: &Path,
    identity: &SourceIdentity,
    cmdline: &str,
) -> Result<DiscoveredEntry> {
    if let Some(entry) = discover_bls(boot_root)? {
        return Ok(DiscoveredEntry {
            entry,
            format: BootEntryFormat::Bls,
        });
    }
    if let Some(entry) = discover_grub(boot_root)? {
        return Ok(DiscoveredEntry {
            entry,
            format: BootEntryFormat::Grub,
        });
    }
    Ok(DiscoveredEntry {
        entry: discover_kernel_fallback(boot_root, identity, cmdline)?,
        format: BootEntryFormat::Grub,
    })
}

/// A source boot artifact copied into the persistent snapshot directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotArtifact {
    pub original_path: String,
    pub stored_name: String,
}

/// Persistent package-host rollback metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageRollbackSnapshot {
    pub schema_version: u32,
    pub source_format: BootEntryFormat,
    pub source: SourceIdentity,
    pub version: String,
    pub options: String,
    pub linux: SnapshotArtifact,
    pub initrds: Vec<SnapshotArtifact>,
    #[serde(default)]
    pub devicetree: Option<SnapshotArtifact>,
    /// BLS/GRUB paths on some hosts include `/boot`; retain that spelling when
    /// rendering the staged entry because the bootloader root differs by layout.
    pub uses_boot_prefix: bool,
    /// NVRAM as it was before the install: `undo` restores the `BootOrder`
    /// the firmware had, because re-pointing the loader default alone
    /// leaves the install's own entry first and the old chain unreachable
    /// (#461). `None` when `efibootmgr` could not be read.
    #[serde(default)]
    pub nvram: Option<NvramSnapshot>,
}

impl PackageRollbackSnapshot {
    fn artifact_path(&self, artifact: &SnapshotArtifact) -> String {
        let prefix = if self.uses_boot_prefix { "/boot/" } else { "/" };
        format!("{prefix}{ROLLBACK_DIR}/{}", artifact.stored_name)
    }

    /// Render the BLS entry written after the target install.
    pub fn render_bls_entry(&self) -> String {
        let title = clean_line(&format!("Previous system ({})", self.source.label()));
        let mut rendered = format!(
            "title {title}\nversion {}\nlinux {}\n",
            clean_line(&self.version),
            self.artifact_path(&self.linux),
        );
        for initrd in &self.initrds {
            rendered.push_str(&format!("initrd {}\n", self.artifact_path(initrd)));
        }
        if let Some(dtb) = &self.devicetree {
            rendered.push_str(&format!("devicetree {}\n", self.artifact_path(dtb)));
        }
        rendered.push_str(&format!(
            "options {}\nsort-key {}\n",
            clean_line(&self.options),
            ROLLBACK_ENTRY_ID,
        ));
        rendered
    }

    /// Render a legacy GRUB `custom.cfg` menuentry for targets without BLS.
    pub fn render_grub_menuentry(&self) -> String {
        let title = grub_quote(&clean_line(&format!(
            "Previous system ({})",
            self.source.label()
        )));
        let mut rendered = format!("menuentry '{title}' --id {ROLLBACK_ENTRY_ID} {{\n");
        rendered.push_str(&format!(
            "    linux {} {}\n",
            self.artifact_path(&self.linux),
            grub_options(&self.options),
        ));
        for initrd in &self.initrds {
            rendered.push_str(&format!("    initrd {}\n", self.artifact_path(initrd)));
        }
        if let Some(dtb) = &self.devicetree {
            rendered.push_str(&format!("    devicetree {}\n", self.artifact_path(dtb)));
        }
        rendered.push_str("}\n");
        rendered
    }
}

fn clean_line(value: &str) -> String {
    value.replace(['\n', '\r'], " ")
}

fn grub_quote(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '\'' => escaped.push_str("\\'"),
            other => escaped.push(other),
        }
    }
    escaped
}

fn grub_options(options: &str) -> String {
    options
        .split_whitespace()
        .map(|option| format!("'{}'", grub_quote(option)))
        .collect::<Vec<_>>()
        .join(" ")
}

fn copy_with_mode(source: &Path, destination: &Path) -> Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(source, destination)
        .with_context(|| format!("copying {} to {}", source.display(), destination.display()))?;
    let mode = fs::metadata(source)?.permissions();
    fs::set_permissions(destination, mode)
        .with_context(|| format!("setting permissions on {}", destination.display()))?;
    Ok(())
}

fn stored_name(kind: &str, index: usize, original_path: &str) -> String {
    let basename = Path::new(original_path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("artifact");
    if kind == "kernel" {
        format!("kernel-{basename}")
    } else if kind == "dtb" {
        format!("dtb-{basename}")
    } else {
        format!("initrd-{index}-{basename}")
    }
}

fn uses_boot_prefix(path: &str) -> bool {
    let path = path.trim_start_matches('/');
    path == "boot" || path.starts_with("boot/")
}

/// Snapshot the source package host's newest usable kernel/initramfs pair,
/// plus the NVRAM state `undo` has to put back. `nvram` is `efibootmgr -v`
/// output read before the install touched the firmware.
pub fn snapshot_source_boot(
    boot_root: &Path,
    state_dir: &Path,
    source: SourceIdentity,
    cmdline: &str,
    nvram: Option<&str>,
) -> Result<PackageRollbackSnapshot> {
    if state_dir.exists() {
        bail!(
            "package rollback snapshot already exists at {}",
            state_dir.display()
        );
    }
    let discovered = discover_source_entry(boot_root, &source, cmdline)?;
    let options = if discovered.entry.options.is_empty() {
        cmdline.trim().to_string()
    } else {
        discovered.entry.options.clone()
    };
    if options.is_empty() {
        bail!("the source boot entry has no kernel command line to preserve");
    }
    let devicetree_artifact = discovered
        .entry
        .devicetree
        .as_ref()
        .map(|dtb| SnapshotArtifact {
            original_path: dtb.clone(),
            stored_name: stored_name("dtb", 0, dtb),
        });

    let snapshot = PackageRollbackSnapshot {
        schema_version: SNAPSHOT_SCHEMA,
        source_format: discovered.format,
        source,
        version: discovered.entry.version,
        options,
        linux: SnapshotArtifact {
            original_path: discovered.entry.linux.clone(),
            stored_name: stored_name("kernel", 0, &discovered.entry.linux),
        },
        initrds: discovered
            .entry
            .initrds
            .iter()
            .enumerate()
            .map(|(index, path)| SnapshotArtifact {
                original_path: path.clone(),
                stored_name: stored_name("initrd", index, path),
            })
            .collect(),
        devicetree: devicetree_artifact,
        uses_boot_prefix: uses_boot_prefix(&discovered.entry.linux),
        nvram: nvram.map(|text| snapshot_from_output(text, now_unix_secs())),
    };

    fs::create_dir_all(
        state_dir
            .parent()
            .ok_or_else(|| anyhow::anyhow!("rollback state directory has no parent"))?,
    )?;
    fs::create_dir(state_dir)
        .with_context(|| format!("creating rollback state directory {}", state_dir.display()))?;
    let artifacts = state_dir.join("artifacts");
    fs::create_dir_all(&artifacts)?;
    let kernel_source = source_boot_file(boot_root, &snapshot.linux.original_path)?;
    copy_with_mode(&kernel_source, &artifacts.join(&snapshot.linux.stored_name))?;
    for initrd in &snapshot.initrds {
        let source_path = source_boot_file(boot_root, &initrd.original_path)?;
        copy_with_mode(&source_path, &artifacts.join(&initrd.stored_name))?;
    }
    if let Some(dtb) = &snapshot.devicetree {
        let source_path = source_boot_file(boot_root, &dtb.original_path)?;
        copy_with_mode(&source_path, &artifacts.join(&dtb.stored_name))?;
    }
    let rendered = serde_json::to_string_pretty(&snapshot)
        .expect("PackageRollbackSnapshot serialization cannot fail");
    fs::write(state_dir.join(SNAPSHOT_FILE), rendered)
        .with_context(|| format!("writing rollback snapshot below {}", state_dir.display()))?;
    Ok(snapshot)
}

/// Read a previously saved package-host rollback snapshot. A snapshot from
/// a newer schema is refused rather than half-understood: `undo` acts on a
/// boot chain, and silently ignoring fields it cannot read is how it would
/// restore the wrong one.
pub fn read_snapshot(state_dir: &Path) -> Result<PackageRollbackSnapshot> {
    let path = state_dir.join(SNAPSHOT_FILE);
    let content = fs::read_to_string(&path)
        .with_context(|| format!("reading package rollback snapshot {}", path.display()))?;
    let snapshot: PackageRollbackSnapshot = serde_json::from_str(&content)
        .with_context(|| format!("parsing package rollback snapshot {}", path.display()))?;
    if snapshot.schema_version > SNAPSHOT_SCHEMA {
        bail!(
            "package rollback snapshot {} has schema version {} but this build understands \
             version {SNAPSHOT_SCHEMA}; roll back with the bootc-rebase that wrote it",
            path.display(),
            snapshot.schema_version
        );
    }
    Ok(snapshot)
}

/// Result of staging the rollback artifacts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageReport {
    pub entry_path: PathBuf,
    pub kernel_path: PathBuf,
    pub initrd_paths: Vec<PathBuf>,
    pub devicetree_path: Option<PathBuf>,
}

/// Return the custom GRUB configuration path for a boot filesystem.
pub fn custom_cfg_path(boot_root: &Path) -> PathBuf {
    if boot_root.join("grub2").is_dir() || !boot_root.join("grub").is_dir() {
        boot_root.join("grub2/custom.cfg")
    } else {
        boot_root.join("grub/custom.cfg")
    }
}

/// Restore the saved kernel/initramfs and write a target-readable rollback
/// entry. `target_supports_bls` selects BLS or the legacy GRUB fallback.
pub fn stage_rollback(
    state_dir: &Path,
    boot_root: &Path,
    target_supports_bls: bool,
) -> Result<StageReport> {
    let snapshot = read_snapshot(state_dir)?;
    let source_artifacts = state_dir.join("artifacts");
    let rollback_root = boot_root.join(ROLLBACK_DIR);
    fs::create_dir_all(&rollback_root)?;

    let kernel_path = rollback_root.join(&snapshot.linux.stored_name);
    copy_with_mode(
        &source_artifacts.join(&snapshot.linux.stored_name),
        &kernel_path,
    )?;
    let mut initrd_paths = Vec::new();
    for initrd in &snapshot.initrds {
        let path = rollback_root.join(&initrd.stored_name);
        copy_with_mode(&source_artifacts.join(&initrd.stored_name), &path)?;
        initrd_paths.push(path);
    }
    let devicetree_path = if let Some(dtb) = &snapshot.devicetree {
        let path = rollback_root.join(&dtb.stored_name);
        copy_with_mode(&source_artifacts.join(&dtb.stored_name), &path)?;
        Some(path)
    } else {
        None
    };

    let entry_path = if target_supports_bls {
        let path = boot_root.join("loader/entries").join(ROLLBACK_ENTRY);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, snapshot.render_bls_entry())
            .with_context(|| format!("writing rollback BLS entry {}", path.display()))?;
        path
    } else {
        let path = custom_cfg_path(boot_root);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, snapshot.render_grub_menuentry())
            .with_context(|| format!("writing rollback GRUB entry {}", path.display()))?;
        path
    };

    Ok(StageReport {
        entry_path,
        kernel_path,
        initrd_paths,
        devicetree_path,
    })
}

/// Bootloader selected by [`run_undo`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bootloader {
    SystemdBoot,
    Grub,
}

/// Detect the target bootloader from its boot filesystem.
pub fn detect_bootloader(boot_root: &Path) -> Option<Bootloader> {
    if boot_root.join("grub2/grub.cfg").is_file()
        || boot_root.join("grub/grub.cfg").is_file()
        || boot_root.join("grub2/custom.cfg").is_file()
        || boot_root.join("grub/custom.cfg").is_file()
    {
        return Some(Bootloader::Grub);
    }
    if boot_root.join("loader/loader.conf").is_file() || boot_root.join("loader/entries").is_dir() {
        return Some(Bootloader::SystemdBoot);
    }
    None
}

/// Build the command that makes the saved rollback entry the persistent
/// default. `grub2-set-default` and `grub-set-default` accept the same BLS id.
pub fn default_command(bootloader: Bootloader, setter: &str) -> Vec<String> {
    match bootloader {
        Bootloader::SystemdBoot => vec![
            setter.to_string(),
            "set-default".to_string(),
            ROLLBACK_ENTRY.to_string(),
        ],
        Bootloader::Grub => vec![setter.to_string(), ROLLBACK_ENTRY_ID.to_string()],
    }
}

/// Build the command that puts a saved `BootOrder` back. Separate from
/// running it so the restore is testable without NVRAM.
pub fn boot_order_command(boot_order: &str) -> Vec<String> {
    vec![
        "efibootmgr".to_string(),
        "--bootorder".to_string(),
        boot_order.to_string(),
    ]
}

fn command_exists(command: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(command).is_file()))
}

fn run_command(argv: &[String], what: &str) -> Result<()> {
    let status = Command::new(&argv[0])
        .args(&argv[1..])
        .status()
        .with_context(|| format!("running {what}"))?;
    if !status.success() {
        bail!("{what} failed (exit {status})");
    }
    Ok(())
}

fn rollback_entry_exists(boot_root: &Path) -> bool {
    boot_root
        .join("loader/entries")
        .join(ROLLBACK_ENTRY)
        .is_file()
        || [
            boot_root.join("grub2/custom.cfg"),
            boot_root.join("grub/custom.cfg"),
        ]
        .iter()
        .any(|path| {
            fs::read_to_string(path)
                .map(|content| content.contains(ROLLBACK_ENTRY_ID))
                .unwrap_or(false)
        })
}

/// Make the package-host rollback entry the default without deleting the new
/// bootc deployment, and put the firmware boot order back the way the
/// install found it. With `reboot`, the next boot immediately enters it.
pub fn run_undo(state_dir: &Path, boot_root: &Path, dry_run: bool, reboot: bool) -> Result<()> {
    let snapshot = read_snapshot(state_dir)?;
    if !rollback_entry_exists(boot_root) {
        bail!(
            "package rollback entry is missing below {}; re-run the migration or restore the saved entry",
            boot_root.display()
        );
    }
    let bootloader = detect_bootloader(boot_root).ok_or_else(|| {
        anyhow::anyhow!(
            "could not identify the target bootloader below {}",
            boot_root.display()
        )
    })?;

    let setter = match bootloader {
        Bootloader::SystemdBoot => "bootctl",
        Bootloader::Grub => {
            if command_exists("grub2-set-default") {
                "grub2-set-default"
            } else if command_exists("grub-set-default") {
                "grub-set-default"
            } else if dry_run {
                "grub2-set-default"
            } else {
                bail!("neither grub2-set-default nor grub-set-default is available")
            }
        }
    };
    let command = default_command(bootloader, setter);
    if dry_run {
        println!("[DRY RUN] Would execute: {}", command.join(" "));
    } else {
        run_command(&command, "selecting the package-host rollback entry")?;
        println!(
            "Package-host rollback entry is now the default; the bootc deployment was not removed."
        );
    }

    // The loader default alone is only half the chain: the install put its
    // own firmware entry first, so without the saved order the firmware
    // still starts the new deployment's loader and never shows the
    // rollback entry (#461).
    match snapshot
        .nvram
        .as_ref()
        .and_then(|n| n.boot_order.as_deref())
    {
        Some(order) => {
            let command = boot_order_command(order);
            if dry_run {
                println!("[DRY RUN] Would execute: {}", command.join(" "));
            } else {
                run_command(&command, "restoring the pre-install BootOrder")?;
                println!("Pre-install BootOrder restored ({order}).");
            }
        }
        None => eprintln!(
            "Warning: the rollback snapshot has no pre-install BootOrder, so the firmware \
             order is left as the install set it; if the next boot does not offer the \
             package system, pick its entry in the firmware boot menu."
        ),
    }

    if reboot {
        let reboot_command = vec!["systemctl".to_string(), "reboot".to_string()];
        if dry_run {
            println!("[DRY RUN] Would execute: {}", reboot_command.join(" "));
        } else {
            run_command(&reboot_command, "rebooting into the package-host rollback")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn source() -> SourceIdentity {
        SourceIdentity {
            id: "postmarketos".into(),
            version_id: "edge".into(),
            name: "postmarketOS".into(),
            pretty_name: "postmarketOS edge".into(),
        }
    }

    #[test]
    fn source_identity_parser_and_label_are_deterministic() {
        let identity = parse_source_identity(
            "ID=ubuntu\nVERSION_ID=24.04\nNAME='Ubuntu'\nPRETTY_NAME=\"Ubuntu 24.04 LTS\"\n",
        )
        .unwrap();
        assert_eq!(identity.label(), "Ubuntu 24.04 LTS");
        assert_eq!(identity.id, "ubuntu");
    }

    #[test]
    fn bls_parser_handles_devicetree() {
        let content = "title postmarketOS\nversion 6.12\nlinux /vmlinuz\ninitrd /initramfs\ndevicetree /sc8280xp-lenovo-thinkpad-x13s.dtb\noptions arm64.nopauth rw\n";
        let entry = parse_bls_entry("pmos.conf", content).unwrap();
        assert_eq!(entry.title, "postmarketOS");
        assert_eq!(entry.linux, "/vmlinuz");
        assert_eq!(entry.initrds, vec!["/initramfs"]);
        assert_eq!(
            entry.devicetree.as_deref(),
            Some("/sc8280xp-lenovo-thinkpad-x13s.dtb")
        );
        assert_eq!(entry.options, "arm64.nopauth rw");
    }

    #[test]
    fn bls_parser_and_newest_selection_table() {
        let cases = [
            (
                "old.conf",
                10,
                "title Fedora 42\nversion 6.10\nlinux /vmlinuz-6.10\ninitrd /initramfs-6.10.img\noptions root=UUID=old rw\n",
            ),
            (
                "new.conf",
                20,
                "title Fedora 43\nversion 6.11\nlinux /vmlinuz-6.11\ninitrd /initramfs-6.11.img\noptions root=UUID=new rw\n",
            ),
        ];
        let candidates: Vec<_> = cases
            .iter()
            .map(|(filename, mtime, content)| BlsCandidate {
                entry: parse_bls_entry(filename, content).unwrap(),
                modified_unix_secs: *mtime,
            })
            .collect();
        let selected = select_newest_bls(&candidates).unwrap();
        assert_eq!(selected.entry.filename, "new.conf");
        assert_eq!(selected.entry.options, "root=UUID=new rw");
    }

    #[test]
    fn renderers_keep_devicetree_and_old_cmdline() {
        let snapshot = PackageRollbackSnapshot {
            schema_version: SNAPSHOT_SCHEMA,
            source_format: BootEntryFormat::Bls,
            source: source(),
            version: "6.12".into(),
            options: "arm64.nopauth clk_ignore_unused rw".into(),
            linux: SnapshotArtifact {
                original_path: "/vmlinuz".into(),
                stored_name: "kernel-vmlinuz".into(),
            },
            initrds: vec![SnapshotArtifact {
                original_path: "/initramfs".into(),
                stored_name: "initrd-0-initramfs".into(),
            }],
            devicetree: Some(SnapshotArtifact {
                original_path: "/sc8280xp.dtb".into(),
                stored_name: "dtb-sc8280xp.dtb".into(),
            }),
            uses_boot_prefix: false,
            nvram: None,
        };
        let bls = snapshot.render_bls_entry();
        assert!(bls.contains("title Previous system (postmarketOS edge)"));
        assert!(bls.contains("linux /bootc-migrate-rollback/kernel-vmlinuz"));
        assert!(bls.contains("initrd /bootc-migrate-rollback/initrd-0-initramfs"));
        assert!(bls.contains("devicetree /bootc-migrate-rollback/dtb-sc8280xp.dtb"));
        assert!(bls.contains("options arm64.nopauth clk_ignore_unused rw"));
        assert!(bls.contains("sort-key bootc-migrate-rollback"));

        let grub = snapshot.render_grub_menuentry();
        assert!(grub.contains("menuentry 'Previous system (postmarketOS edge)'"));
        assert!(grub.contains("devicetree /bootc-migrate-rollback/dtb-sc8280xp.dtb"));
    }

    #[test]
    fn snapshot_and_stage_round_trip_with_devicetree() {
        let root = tempdir().unwrap();
        let boot = root.path().join("boot");
        let entries = boot.join("loader/entries");
        fs::create_dir_all(&entries).unwrap();
        fs::write(boot.join("vmlinuz"), "pmos kernel").unwrap();
        fs::write(boot.join("initramfs"), "pmos initrd").unwrap();
        fs::write(boot.join("x13s.dtb"), "pmos dtb").unwrap();
        fs::write(
            entries.join("pmos.conf"),
            "title postmarketOS\nversion 6.12\nlinux /vmlinuz\ninitrd /initramfs\ndevicetree /x13s.dtb\noptions arm64.nopauth rw\n",
        )
        .unwrap();
        let state = root.path().join("var/lib/rollback");
        let snapshot = snapshot_source_boot(
            &boot,
            &state,
            source(),
            "arm64.nopauth clk_ignore_unused rw",
            None,
        )
        .unwrap();
        fs::remove_dir_all(&boot).unwrap();
        fs::create_dir_all(&boot).unwrap();
        let staged = stage_rollback(&state, &boot, true).unwrap();

        assert_eq!(fs::read(&staged.kernel_path).unwrap(), b"pmos kernel");
        assert_eq!(fs::read(&staged.initrd_paths[0]).unwrap(), b"pmos initrd");
        assert_eq!(
            fs::read(staged.devicetree_path.as_ref().unwrap()).unwrap(),
            b"pmos dtb"
        );
        let entry = fs::read_to_string(&staged.entry_path).unwrap();
        assert_eq!(entry, snapshot.render_bls_entry());
        assert!(entry.contains("devicetree /bootc-migrate-rollback/dtb-x13s.dtb"));
        assert!(boot.join("loader/entries").join(ROLLBACK_ENTRY).is_file());
    }

    /// A staged entry the undo command cannot name is a rollback that
    /// silently does nothing: systemd-boot is given the BLS *filename*,
    /// GRUB the menuentry's `--id`, so both are taken from the artifact
    /// `stage_rollback` actually wrote rather than restated here.
    #[test]
    fn staged_entry_id_is_the_one_undo_selects() {
        let root = tempdir().unwrap();
        let boot = root.path().join("boot");
        fs::create_dir_all(boot.join("loader/entries")).unwrap();
        fs::write(boot.join("vmlinuz"), "kernel").unwrap();
        fs::write(boot.join("initramfs"), "initrd").unwrap();
        fs::write(
            boot.join("loader/entries/pmos.conf"),
            "title postmarketOS\nversion 6.12\nlinux /vmlinuz\ninitrd /initramfs\noptions rw\n",
        )
        .unwrap();
        let state = root.path().join("var/lib/rollback");
        snapshot_source_boot(&boot, &state, source(), "rw", None).unwrap();

        let bls_boot = root.path().join("target-bls");
        fs::create_dir_all(bls_boot.join("loader/entries")).unwrap();
        let staged = stage_rollback(&state, &bls_boot, true).unwrap();
        let staged_name = staged
            .entry_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap()
            .to_string();
        assert_eq!(detect_bootloader(&bls_boot), Some(Bootloader::SystemdBoot));
        assert_eq!(
            default_command(Bootloader::SystemdBoot, "bootctl").last(),
            Some(&staged_name)
        );
        assert!(rollback_entry_exists(&bls_boot));

        let grub_boot = root.path().join("target-grub");
        fs::create_dir_all(&grub_boot).unwrap();
        let staged = stage_rollback(&state, &grub_boot, false).unwrap();
        let menuentry = fs::read_to_string(&staged.entry_path).unwrap();
        let staged_id = menuentry
            .split("--id ")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .unwrap()
            .to_string();
        assert_eq!(detect_bootloader(&grub_boot), Some(Bootloader::Grub));
        assert_eq!(
            default_command(Bootloader::Grub, "grub2-set-default").last(),
            Some(&staged_id)
        );
        assert!(rollback_entry_exists(&grub_boot));
    }

    /// `undo` restores the firmware order the install replaced, so the
    /// order has to survive the snapshot JSON and reach the command.
    #[test]
    fn pre_install_boot_order_survives_the_snapshot() {
        const EFIBOOTMGR_V: &str = "BootCurrent: 0002\n\
            Timeout: 1 seconds\n\
            BootOrder: 0002,0000,0001\n\
            Boot0000* Fedora\tHD(1,GPT,abc)/File(\\EFI\\fedora\\shimx64.efi)\n\
            Boot0002* Linux Boot Manager\tHD(1,GPT,abc)/File(\\EFI\\systemd\\systemd-bootx64.efi)\n";
        let root = tempdir().unwrap();
        let boot = root.path().join("boot");
        fs::create_dir_all(boot.join("loader/entries")).unwrap();
        fs::write(boot.join("vmlinuz"), "kernel").unwrap();
        fs::write(boot.join("initramfs"), "initrd").unwrap();
        fs::write(
            boot.join("loader/entries/pmos.conf"),
            "title postmarketOS\nversion 6.12\nlinux /vmlinuz\ninitrd /initramfs\noptions rw\n",
        )
        .unwrap();

        let state = root.path().join("var/lib/rollback");
        snapshot_source_boot(&boot, &state, source(), "rw", Some(EFIBOOTMGR_V)).unwrap();
        let nvram = read_snapshot(&state).unwrap().nvram.unwrap();
        assert_eq!(nvram.boot_order.as_deref(), Some("0002,0000,0001"));
        assert_eq!(nvram.boot_current.as_deref(), Some("0002"));
        assert_eq!(
            boot_order_command(nvram.boot_order.as_deref().unwrap()),
            ["efibootmgr", "--bootorder", "0002,0000,0001"]
        );

        // A host whose NVRAM could not be read still gets a rollback entry;
        // it just has no order to put back.
        let without = root.path().join("var/lib/rollback-no-nvram");
        snapshot_source_boot(&boot, &without, source(), "rw", None).unwrap();
        assert!(read_snapshot(&without).unwrap().nvram.is_none());
    }

    /// A snapshot from a newer build describes a boot chain this one cannot
    /// fully read; acting on it would restore the wrong thing.
    #[test]
    fn newer_snapshot_schema_is_refused() {
        let root = tempdir().unwrap();
        let state = root.path().join("state");
        fs::create_dir_all(&state).unwrap();
        let newer = format!(
            "{{\"schema_version\":{},\"source_format\":\"bls\",\"source\":{{\"id\":\"x\",\
             \"version_id\":\"1\",\"name\":\"X\",\"pretty_name\":\"X 1\"}},\"version\":\"6\",\
             \"options\":\"rw\",\"linux\":{{\"original_path\":\"/vmlinuz\",\
             \"stored_name\":\"kernel-vmlinuz\"}},\"initrds\":[],\"uses_boot_prefix\":false}}",
            SNAPSHOT_SCHEMA + 1
        );
        fs::write(state.join(SNAPSHOT_FILE), newer).unwrap();
        let err = format!("{:#}", read_snapshot(&state).unwrap_err());
        assert!(err.contains("schema version"), "{err}");
        assert!(err.contains(&(SNAPSHOT_SCHEMA + 1).to_string()), "{err}");
    }
}
