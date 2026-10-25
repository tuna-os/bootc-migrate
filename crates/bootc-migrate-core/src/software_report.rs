//! Software carried by a package-managed host but absent from a target image.
//!
//! Package inventory is kept separate from I/O: package-manager databases are
//! read into [`InstalledPackage`] values, then [`build`] performs the comparison
//! and classification. Registry scanning supplies [`TargetInventory`] from the
//! package database extracted out of the image.

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::source_host::{self, HostKind, PackageDb};

pub const STATE_DIR: &str = "/var/lib/bootc-rebase";
pub const JSON_REPORT_FILE: &str = "software-report.json";
pub const MARKDOWN_REPORT_FILE: &str = "software-report.md";

const RPM_PACKAGE_MARKER: &str = "@@BOOTC-MIGRATE-PACKAGE@@";
const RPM_QUERY_FORMAT: &str = "@@BOOTC-MIGRATE-PACKAGE@@%{NAME}\n[%{FILENAMES}\n]";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledPackage {
    pub name: String,
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TargetInventory {
    pub packages: BTreeSet<String>,
    pub system_units: BTreeSet<String>,
    pub package_db: Option<PackageDb>,
    pub complete: bool,
    pub warning: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostInventory {
    pub packages: Vec<InstalledPackage>,
    pub repositories: Vec<Repository>,
    pub enabled_services: Vec<String>,
    pub enabled_services_complete: bool,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageKind {
    DesktopApplication,
    CliTool,
    SystemService,
    KernelModule,
    Other,
}

impl PackageKind {
    fn heading(self) -> &'static str {
        match self {
            Self::DesktopApplication => "Desktop applications",
            Self::CliTool => "Command-line tools",
            Self::SystemService => "System services",
            Self::KernelModule => "Kernel modules (DKMS/akmods)",
            Self::Other => "Other packages",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplacementKind {
    Flatpak,
    Homebrew,
    ToolboxOrDistrobox,
    DerivedImage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MissingPackage {
    pub name: String,
    pub kind: PackageKind,
    pub replacement_kind: ReplacementKind,
    pub replacement: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Repository {
    pub name: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MissingService {
    pub unit: String,
    pub source_package: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SoftwareReport {
    pub target_image: String,
    pub target_package_db: Option<String>,
    pub comparison_complete: bool,
    pub missing_packages: Vec<MissingPackage>,
    pub repositories: Vec<Repository>,
    pub missing_enabled_services: Vec<MissingService>,
    pub warnings: Vec<String>,
}

impl SoftwareReport {
    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)? + "\n")
    }

    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "# Software report\n");
        let _ = writeln!(out, "Target image: `{}`\n", self.target_image);
        let _ = writeln!(
            out,
            "Comparison: **{}**\n",
            if self.comparison_complete {
                "complete"
            } else {
                "incomplete — see warnings"
            }
        );

        for kind in [
            PackageKind::DesktopApplication,
            PackageKind::CliTool,
            PackageKind::SystemService,
            PackageKind::KernelModule,
            PackageKind::Other,
        ] {
            let mut packages = self
                .missing_packages
                .iter()
                .filter(|package| package.kind == kind)
                .peekable();
            if packages.peek().is_none() {
                continue;
            }
            let _ = writeln!(out, "## {}\n", kind.heading());
            for package in packages {
                let _ = writeln!(out, "- `{}` — {}", package.name, package.replacement);
            }
            out.push('\n');
        }

        let mut derived = String::new();
        for package in self
            .missing_packages
            .iter()
            .filter(|package| package.replacement_kind == ReplacementKind::DerivedImage)
        {
            if !derived.is_empty() {
                derived.push(' ');
            }
            derived.push_str(&package.name);
        }
        if !derived.is_empty() {
            let _ = writeln!(out, "### Derived-image `Containerfile`\n");
            let _ = writeln!(out, "```Dockerfile");
            let _ = writeln!(out, "FROM {}", self.target_image);
            let packages = derived;
            match self.target_package_db.as_deref() {
                Some("rpm") => {
                    let _ = writeln!(out, "RUN dnf -y install {packages} && dnf clean all");
                }
                Some("dpkg") => {
                    let _ = writeln!(
                        out,
                        "RUN apt-get update && apt-get install -y {packages} && rm -rf /var/lib/apt/lists/*"
                    );
                }
                Some("pacman") => {
                    let _ = writeln!(
                        out,
                        "RUN pacman -Syu --noconfirm {packages} && pacman -Scc --noconfirm"
                    );
                }
                _ => {
                    let _ = writeln!(out, "# Install: {packages}");
                }
            }
            let _ = writeln!(out, "```\n");
        }

        if !self.repositories.is_empty() {
            let _ = writeln!(out, "## Third-party package repositories\n");
            let _ = writeln!(
                out,
                "These host repository definitions do not carry into the image:\n"
            );
            for repository in &self.repositories {
                let _ = writeln!(out, "- `{}` — {}", repository.name, repository.source);
            }
            out.push('\n');
        }

        if !self.missing_enabled_services.is_empty() {
            let _ = writeln!(out, "## Enabled services absent from the image\n");
            for service in &self.missing_enabled_services {
                if let Some(package) = &service.source_package {
                    let _ = writeln!(out, "- `{}` (from `{package}`)", service.unit);
                } else {
                    let _ = writeln!(out, "- `{}`", service.unit);
                }
            }
            out.push('\n');
        }

        if self
            .missing_packages
            .iter()
            .any(|package| package.kind == PackageKind::KernelModule)
        {
            let _ = writeln!(
                out,
                "Kernel-module packages need a target-compatible build. Cross-check these against the hardware report before rebooting (bootc-migrate#369).\n"
            );
        }

        if !self.warnings.is_empty() {
            let _ = writeln!(out, "## Warnings\n");
            for warning in &self.warnings {
                let _ = writeln!(out, "- {warning}");
            }
            out.push('\n');
        }
        out
    }
}

/// Build the report without reading the machine. An incomplete target inventory
/// suppresses absence claims: a gap in the registry scan is not evidence that a
/// package or service is missing.
pub fn build(host: HostInventory, target: &TargetInventory, target_image: &str) -> SoftwareReport {
    let mut warnings = host.warnings;
    if let Some(warning) = &target.warning {
        warnings.push(warning.clone());
    }

    let missing_packages = if target.complete {
        host.packages
            .iter()
            .filter(|package| !target.packages.contains(&package.name))
            .map(classify)
            .collect()
    } else {
        warnings.push(
            "Missing-package and missing-service comparisons were suppressed because the target inventory is incomplete."
                .to_string(),
        );
        Vec::new()
    };

    let missing_enabled_services = if target.complete && host.enabled_services_complete {
        let unit_owners: BTreeMap<&str, &str> =
            host.packages
                .iter()
                .flat_map(|package| {
                    package.files.iter().filter_map(|path| {
                        unit_name(path).map(|unit| (unit, package.name.as_str()))
                    })
                })
                .collect();
        host.enabled_services
            .iter()
            .filter(|unit| !target.system_units.contains(*unit))
            .map(|unit| MissingService {
                unit: unit.clone(),
                source_package: unit_owners
                    .get(unit.as_str())
                    .map(|package| (*package).to_string()),
            })
            .collect()
    } else {
        Vec::new()
    };

    SoftwareReport {
        target_image: target_image.to_string(),
        target_package_db: target.package_db.map(|package_db| package_db.to_string()),
        comparison_complete: target.complete && host.enabled_services_complete,
        missing_packages,
        repositories: host.repositories,
        missing_enabled_services,
        warnings,
    }
}

fn classify(package: &InstalledPackage) -> MissingPackage {
    let kind = package_kind(package);
    let (replacement_kind, replacement) = match kind {
        PackageKind::DesktopApplication => flatpak_id(package)
            .map(|id| {
                (
                    ReplacementKind::Flatpak,
                    format!("install from Flathub: `flatpak install flathub {id}`"),
                )
            })
            .unwrap_or_else(|| {
                (
                    ReplacementKind::ToolboxOrDistrobox,
                    "install in a toolbox/distrobox container (no matching Flathub ID found)"
                        .to_string(),
                )
            }),
        PackageKind::CliTool => brew_formula(&package.name)
            .map(|formula| {
                (
                    ReplacementKind::Homebrew,
                    format!("install with Homebrew: `brew install {formula}`"),
                )
            })
            .unwrap_or_else(|| {
                (
                    ReplacementKind::ToolboxOrDistrobox,
                    "install in a toolbox/distrobox container".to_string(),
                )
            }),
        PackageKind::SystemService | PackageKind::KernelModule => (
            ReplacementKind::DerivedImage,
            "add to a derived image (see the Containerfile snippet below)".to_string(),
        ),
        PackageKind::Other => (
            ReplacementKind::ToolboxOrDistrobox,
            "install in a toolbox/distrobox container, or add it to a derived image if the host needs it at boot"
                .to_string(),
        ),
    };
    MissingPackage {
        name: package.name.clone(),
        kind,
        replacement_kind,
        replacement,
    }
}

fn package_kind(package: &InstalledPackage) -> PackageKind {
    if package.name.contains("akmod")
        || package.name.contains("dkms")
        || package.name.starts_with("kmod-")
        || package.files.iter().any(|path| {
            path.starts_with("/usr/lib/modules/")
                || path.starts_with("/lib/modules/")
                || path.starts_with("/var/lib/dkms/")
        })
    {
        PackageKind::KernelModule
    } else if package
        .files
        .iter()
        .any(|path| desktop_file_id(path).is_some())
    {
        PackageKind::DesktopApplication
    } else if package.files.iter().any(|path| unit_name(path).is_some()) {
        PackageKind::SystemService
    } else if package.files.iter().any(|path| {
        ["/usr/bin/", "/usr/sbin/", "/bin/", "/sbin/"]
            .iter()
            .any(|prefix| path.starts_with(prefix) && path.len() > prefix.len())
    }) {
        PackageKind::CliTool
    } else {
        PackageKind::Other
    }
}

fn desktop_file_id(path: &str) -> Option<&str> {
    let filename = path
        .strip_prefix("/usr/share/applications/")?
        .strip_suffix(".desktop")?;
    (!filename.is_empty()).then_some(filename)
}

fn flatpak_id(package: &InstalledPackage) -> Option<&'static str> {
    const KNOWN: &[(&str, &str)] = &[
        ("baobab", "org.gnome.baobab"),
        ("cheese", "org.gnome.Cheese"),
        ("discord", "com.discordapp.Discord"),
        ("evince", "org.gnome.Evince"),
        ("file-roller", "org.gnome.FileRoller"),
        ("firefox", "org.mozilla.firefox"),
        ("gimp", "org.gimp.GIMP"),
        ("gnome-calculator", "org.gnome.Calculator"),
        ("gnome-calendar", "org.gnome.Calendar"),
        ("gnome-clocks", "org.gnome.Clocks"),
        ("gnome-maps", "org.gnome.Maps"),
        ("gnome-text-editor", "org.gnome.TextEditor"),
        ("inkscape", "org.inkscape.Inkscape"),
        ("libreoffice", "org.libreoffice.LibreOffice"),
        ("loupe", "org.gnome.Loupe"),
        ("obs-studio", "com.obsproject.Studio"),
        ("rhythmbox", "org.gnome.Rhythmbox3"),
        ("snapshot", "org.gnome.Snapshot"),
        ("spotify-client", "com.spotify.Client"),
        ("steam", "com.valvesoftware.Steam"),
        ("thunderbird", "org.mozilla.Thunderbird"),
        ("vlc", "org.videolan.VLC"),
    ];
    KNOWN
        .iter()
        .find_map(|(name, id)| (*name == package.name).then_some(*id))
}

fn brew_formula(package: &str) -> Option<&'static str> {
    const FORMULAE: &[(&str, &str)] = &[
        ("bat", "bat"),
        ("btop", "btop"),
        ("curl", "curl"),
        ("eza", "eza"),
        ("fd-find", "fd"),
        ("fzf", "fzf"),
        ("git", "git"),
        ("git-lfs", "git-lfs"),
        ("htop", "htop"),
        ("jq", "jq"),
        ("neovim", "neovim"),
        ("ripgrep", "ripgrep"),
        ("tmux", "tmux"),
        ("tree", "tree"),
        ("vim", "vim"),
        ("wget", "wget"),
        ("zoxide", "zoxide"),
        ("zsh", "zsh"),
    ];
    FORMULAE
        .iter()
        .find_map(|(name, formula)| (*name == package).then_some(*formula))
}

fn unit_name(path: &str) -> Option<&str> {
    const DIRECTORIES: &[&str] = &[
        "/usr/lib/systemd/system/",
        "/lib/systemd/system/",
        "/etc/systemd/system/",
    ];
    let filename = DIRECTORIES
        .iter()
        .find_map(|directory| path.strip_prefix(directory))?;
    (!filename.contains('/') && filename.ends_with(".service")).then_some(filename)
}

/// Read the live package host and compare it with inventory already extracted
/// from the target image. Returns `None` when the current root is not a package
/// host; scans of existing bootc deployments retain their capability-only form.
pub fn gather_live(target: &TargetInventory, target_image: &str) -> Result<Option<SoftwareReport>> {
    let root = Path::new("/");
    let HostKind::Package(package_db) = source_host::detect_host_kind(root) else {
        return Ok(None);
    };
    let host = gather_host(root, package_db)?;
    Ok(Some(build(host, target, target_image)))
}

fn gather_host(root: &Path, package_db: PackageDb) -> Result<HostInventory> {
    let packages = read_installed_packages(root, package_db)
        .with_context(|| format!("reading the host's {package_db} package inventory"))?;
    let repositories = read_repositories(root)?;
    let (enabled_services, enabled_services_complete, warnings) = match read_enabled_services() {
        Ok(services) => (services, true, Vec::new()),
        Err(error) => (
            Vec::new(),
            false,
            vec![format!(
                "Enabled services could not be inventoried: {error:#}. No service-absence claims were made."
            )],
        ),
    };
    Ok(HostInventory {
        packages,
        repositories,
        enabled_services,
        enabled_services_complete,
        warnings,
    })
}

pub(crate) fn target_inventory_from_root(root: &Path, layers_skipped: usize) -> TargetInventory {
    let package_db =
        if root.join("usr/lib/sysimage/rpm").exists() || root.join("var/lib/rpm").exists() {
            Some(PackageDb::Rpm)
        } else if root.join("var/lib/dpkg/status").exists() {
            Some(PackageDb::Dpkg)
        } else if root.join("var/lib/pacman/local").exists() {
            Some(PackageDb::Pacman)
        } else {
            None
        };

    let Some(package_db) = package_db else {
        return TargetInventory {
            warning: Some(
                "The target image exposes no rpm, dpkg, or pacman database; its package inventory is unknown."
                    .to_string(),
            ),
            ..Default::default()
        };
    };
    if layers_skipped != 0 {
        return TargetInventory {
            package_db: Some(package_db),
            warning: Some(format!(
                "{layers_skipped} target image layer(s) could not be read; its package inventory is incomplete."
            )),
            ..Default::default()
        };
    }

    match read_installed_packages(root, package_db).and_then(|installed| {
        let system_units = system_units_from_root(root)?;
        Ok((installed, system_units))
    }) {
        Ok((installed, mut system_units)) => {
            let packages = installed
                .iter()
                .map(|package| package.name.clone())
                .collect();
            system_units.extend(installed.iter().flat_map(|package| {
                package
                    .files
                    .iter()
                    .filter_map(|path| unit_name(path).map(str::to_string))
            }));
            TargetInventory {
                packages,
                system_units,
                package_db: Some(package_db),
                complete: true,
                warning: None,
            }
        }
        Err(error) => TargetInventory {
            package_db: Some(package_db),
            warning: Some(format!(
                "The target {package_db} database could not be read: {error:#}."
            )),
            ..Default::default()
        },
    }
}

fn system_units_from_root(root: &Path) -> Result<BTreeSet<String>> {
    let mut units = BTreeSet::new();
    for directory in [
        "usr/lib/systemd/system",
        "lib/systemd/system",
        "etc/systemd/system",
    ] {
        let directory = root.join(directory);
        if !directory.is_dir() {
            continue;
        }
        for entry in
            fs::read_dir(&directory).with_context(|| format!("reading {}", directory.display()))?
        {
            let entry = entry?;
            if let Some(name) = entry.file_name().to_str()
                && name.ends_with(".service")
            {
                units.insert(name.to_string());
            }
        }
    }
    Ok(units)
}

fn read_installed_packages(root: &Path, package_db: PackageDb) -> Result<Vec<InstalledPackage>> {
    let mut packages = match package_db {
        PackageDb::Rpm => read_rpm_packages(root),
        PackageDb::Dpkg => read_dpkg_packages(root),
        PackageDb::Pacman => read_pacman_packages(root),
    }?;
    packages.sort_unstable_by(|left, right| left.name.cmp(&right.name));

    let mut merged: Vec<InstalledPackage> = Vec::with_capacity(packages.len());
    for mut package in packages {
        if let Some(previous) = merged
            .last_mut()
            .filter(|previous| previous.name == package.name)
        {
            previous.files.append(&mut package.files);
        } else {
            merged.push(package);
        }
    }
    for package in &mut merged {
        package.files.sort_unstable();
        package.files.dedup();
    }
    Ok(merged)
}

fn read_rpm_packages(root: &Path) -> Result<Vec<InstalledPackage>> {
    let database = if root.join("usr/lib/sysimage/rpm").exists() {
        "/usr/lib/sysimage/rpm"
    } else {
        "/var/lib/rpm"
    };
    let output = Command::new("rpm")
        .arg("--root")
        .arg(root)
        .args(["--dbpath", database, "-qa", "--qf", RPM_QUERY_FORMAT])
        .output()
        .context("executing rpm query")?;
    if !output.status.success() {
        bail!(
            "rpm query failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    parse_rpm_query(&String::from_utf8(output.stdout).context("rpm emitted non-UTF-8 output")?)
}

fn parse_rpm_query(output: &str) -> Result<Vec<InstalledPackage>> {
    let mut packages = Vec::new();
    let mut current: Option<InstalledPackage> = None;
    for line in output.lines() {
        if let Some(name) = line.strip_prefix(RPM_PACKAGE_MARKER) {
            if let Some(package) = current.take() {
                packages.push(package);
            }
            if name.is_empty() {
                bail!("rpm emitted a package without a name");
            }
            current = Some(InstalledPackage {
                name: name.to_string(),
                files: Vec::new(),
            });
        } else if !line.is_empty() {
            let package = current
                .as_mut()
                .ok_or_else(|| anyhow!("rpm emitted a file before its package name"))?;
            package.files.push(line.to_string());
        }
    }
    if let Some(package) = current {
        packages.push(package);
    }
    packages.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(packages)
}

fn read_dpkg_packages(root: &Path) -> Result<Vec<InstalledPackage>> {
    let status_path = root.join("var/lib/dpkg/status");
    let status = fs::read_to_string(&status_path)
        .with_context(|| format!("reading {}", status_path.display()))?;
    let mut packages: BTreeMap<String, InstalledPackage> = parse_dpkg_status(&status)
        .into_iter()
        .map(|name| {
            (
                name.clone(),
                InstalledPackage {
                    name,
                    files: Vec::new(),
                },
            )
        })
        .collect();

    let info = root.join("var/lib/dpkg/info");
    if info.is_dir() {
        for entry in fs::read_dir(&info).with_context(|| format!("reading {}", info.display()))? {
            let entry = entry?;
            let Some(filename) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let Some(stem) = filename.strip_suffix(".list") else {
                continue;
            };
            let package_name = stem.split(':').next().unwrap_or(stem);
            let Some(package) = packages.get_mut(package_name) else {
                continue;
            };
            let contents = fs::read_to_string(entry.path())
                .with_context(|| format!("reading {}", entry.path().display()))?;
            package.files.extend(
                contents
                    .lines()
                    .filter(|line| line.starts_with('/'))
                    .map(str::to_string),
            );
        }
    }
    Ok(packages.into_values().collect())
}

fn parse_dpkg_status(status: &str) -> Vec<String> {
    let mut names = Vec::new();
    for paragraph in status.split("\n\n") {
        let mut name = None;
        let mut installed = false;
        for line in paragraph.lines() {
            if let Some(value) = line.strip_prefix("Package:") {
                name = Some(value.trim().split(':').next().unwrap_or_default());
            } else if let Some(value) = line.strip_prefix("Status:") {
                installed = value.trim() == "install ok installed";
            }
        }
        if installed && let Some(name) = name.filter(|name| !name.is_empty()) {
            names.push(name.to_string());
        }
    }
    names.sort();
    names
}

fn read_pacman_packages(root: &Path) -> Result<Vec<InstalledPackage>> {
    let local = root.join("var/lib/pacman/local");
    let mut packages = Vec::new();
    for entry in fs::read_dir(&local).with_context(|| format!("reading {}", local.display()))? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let desc_path = entry.path().join("desc");
        let desc = fs::read_to_string(&desc_path)
            .with_context(|| format!("reading {}", desc_path.display()))?;
        let Some(name) = pacman_section_value(&desc, "%NAME%") else {
            continue;
        };
        let files_path = entry.path().join("files");
        let files = if files_path.exists() {
            let contents = fs::read_to_string(&files_path)
                .with_context(|| format!("reading {}", files_path.display()))?;
            pacman_files(&contents)
        } else {
            Vec::new()
        };
        packages.push(InstalledPackage { name, files });
    }
    packages.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(packages)
}

fn pacman_section_value(contents: &str, section: &str) -> Option<String> {
    let mut lines = contents.lines();
    while let Some(line) = lines.next() {
        if line == section {
            return lines
                .next()
                .filter(|value| !value.is_empty())
                .map(str::to_string);
        }
    }
    None
}

fn pacman_files(contents: &str) -> Vec<String> {
    let Some((_, files)) = contents.split_once("%FILES%\n") else {
        return Vec::new();
    };
    files
        .lines()
        .take_while(|line| !line.starts_with('%'))
        .filter(|line| !line.is_empty() && !line.ends_with('/'))
        .map(|line| format!("/{line}"))
        .collect()
}

fn read_enabled_services() -> Result<Vec<String>> {
    let output = Command::new("systemctl")
        .args([
            "list-unit-files",
            "--type=service",
            "--no-legend",
            "--no-pager",
        ])
        .output()
        .context("executing systemctl list-unit-files")?;
    if !output.status.success() {
        bail!(
            "systemctl list-unit-files failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let mut services: Vec<String> = String::from_utf8(output.stdout)
        .context("systemctl emitted non-UTF-8 output")?
        .lines()
        .filter_map(|line| {
            let mut columns = line.split_whitespace();
            let unit = columns.next()?;
            let state = columns.next()?;
            matches!(state, "enabled" | "enabled-runtime").then(|| unit.to_string())
        })
        .collect();
    services.sort();
    services.dedup();
    Ok(services)
}

fn read_repositories(root: &Path) -> Result<Vec<Repository>> {
    let os_ids = host_os_ids(root)?;
    let mut repositories = Vec::new();
    read_yum_repositories(root, &os_ids, &mut repositories)?;
    read_apt_repositories(root, &os_ids, &mut repositories)?;
    read_pacman_repositories(root, &mut repositories)?;
    repositories.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.source.cmp(&right.source))
    });
    repositories.dedup();
    Ok(repositories)
}

fn host_os_ids(root: &Path) -> Result<BTreeSet<String>> {
    let mut contents = None;
    for path in ["etc/os-release", "usr/lib/os-release"] {
        let path = root.join(path);
        if path.exists() {
            contents = Some(
                fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?,
            );
            break;
        }
    }
    let mut ids = BTreeSet::new();
    if let Some(contents) = contents {
        for (key, value) in contents.lines().filter_map(|line| line.split_once('=')) {
            if !matches!(key, "ID" | "ID_LIKE") {
                continue;
            }
            ids.extend(
                value
                    .trim()
                    .trim_matches(['\"', '\''])
                    .split_whitespace()
                    .map(str::to_lowercase),
            );
        }
    }
    Ok(ids)
}

fn is_vendor_yum_repository(os_ids: &BTreeSet<String>, repository_id: &str) -> bool {
    (os_ids.contains("fedora")
        && (matches!(
            repository_id,
            "fedora" | "fedora-cisco-openh264" | "rawhide"
        ) || repository_id.starts_with("updates")
            || repository_id.starts_with("rawhide-")))
        || (os_ids
            .iter()
            .any(|id| matches!(id.as_str(), "rhel" | "centos" | "almalinux" | "rocky"))
            && (matches!(
                repository_id,
                "baseos" | "appstream" | "extras" | "crb" | "highavailability" | "resilientstorage"
            ) || repository_id.starts_with("rhel-")
                || repository_id.starts_with("codeready-builder-for-rhel-")))
}

fn is_vendor_apt_repository(os_ids: &BTreeSet<String>, repository: &str) -> bool {
    (os_ids.contains("debian")
        && (repository.contains("deb.debian.org/debian")
            || repository.contains("security.debian.org/debian-security")))
        || (os_ids.contains("ubuntu")
            && (repository.contains("archive.ubuntu.com/ubuntu")
                || repository.contains("security.ubuntu.com/ubuntu")
                || repository.contains("ports.ubuntu.com/ubuntu-ports")))
}

fn sorted_files(directory: &Path) -> Result<Vec<PathBuf>> {
    if !directory.is_dir() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    for entry in fs::read_dir(directory)
        .with_context(|| format!("reading repository directory {}", directory.display()))?
    {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            files.push(entry.path());
        }
    }
    files.sort();
    Ok(files)
}

fn display_source(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .ok()
        .map(|path| format!("/{}", path.display()))
        .unwrap_or_else(|| path.display().to_string())
}

fn read_yum_repositories(
    root: &Path,
    os_ids: &BTreeSet<String>,
    repositories: &mut Vec<Repository>,
) -> Result<()> {
    for path in sorted_files(&root.join("etc/yum.repos.d"))? {
        if path.extension().and_then(|extension| extension.to_str()) != Some("repo") {
            continue;
        }
        let contents = fs::read_to_string(&path)
            .with_context(|| format!("reading repository file {}", path.display()))?;
        let source = display_source(root, &path);
        let mut section: Option<(String, String, bool)> = None;
        let finish = |section: Option<(String, String, bool)>,
                      repositories: &mut Vec<Repository>| {
            if let Some((id, name, enabled)) = section
                && enabled
                && !is_vendor_yum_repository(os_ids, &id)
            {
                repositories.push(Repository {
                    name: if name.is_empty() {
                        id
                    } else {
                        format!("{name} ({id})")
                    },
                    source: source.clone(),
                });
            }
        };
        for line in contents.lines().map(str::trim) {
            if line.starts_with('[') && line.ends_with(']') {
                finish(section.take(), repositories);
                let id = line[1..line.len() - 1].trim().to_string();
                section = Some((id, String::new(), true));
            } else if let Some((_, name, enabled)) = section.as_mut()
                && let Some((key, value)) = line.split_once('=')
            {
                match key.trim() {
                    "name" => *name = value.trim().to_string(),
                    "enabled" => *enabled = !matches!(value.trim(), "0" | "false" | "no"),
                    _ => {}
                }
            }
        }
        finish(section, repositories);
    }
    Ok(())
}

fn redact_uri_credentials(value: &str) -> String {
    let mut redacted = String::with_capacity(value.len());
    for token in value.split_whitespace() {
        if !redacted.is_empty() {
            redacted.push(' ');
        }
        let Some((scheme, rest)) = token.split_once("://") else {
            redacted.push_str(token);
            continue;
        };
        let authority_end = rest.find('/').unwrap_or(rest.len());
        let (authority, suffix) = rest.split_at(authority_end);
        let Some((_, host)) = authority.rsplit_once('@') else {
            redacted.push_str(token);
            continue;
        };
        let _ = write!(redacted, "{scheme}://<redacted>@{host}{suffix}");
    }
    redacted
}

fn read_apt_repositories(
    root: &Path,
    os_ids: &BTreeSet<String>,
    repositories: &mut Vec<Repository>,
) -> Result<()> {
    for path in sorted_files(&root.join("etc/apt/sources.list.d"))? {
        let contents = fs::read_to_string(&path)
            .with_context(|| format!("reading repository file {}", path.display()))?;
        let source = display_source(root, &path);
        let filename = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("apt source");
        if path.extension().and_then(|extension| extension.to_str()) == Some("sources") {
            for stanza in contents.split("\n\n") {
                let disabled = stanza
                    .lines()
                    .any(|line| line.trim().eq_ignore_ascii_case("Enabled: no"));
                if disabled {
                    continue;
                }
                let uris = stanza
                    .lines()
                    .find_map(|line| line.strip_prefix("URIs:").map(str::trim));
                let suites = stanza
                    .lines()
                    .find_map(|line| line.strip_prefix("Suites:").map(str::trim));
                for uri in uris.into_iter().flat_map(str::split_whitespace) {
                    if is_vendor_apt_repository(os_ids, uri) {
                        continue;
                    }
                    let uri = redact_uri_credentials(uri);
                    let name = suites
                        .filter(|suite| !suite.is_empty())
                        .map(|suite| format!("{filename}: {uri} {suite}"))
                        .unwrap_or_else(|| format!("{filename}: {uri}"));
                    repositories.push(Repository {
                        name,
                        source: source.clone(),
                    });
                }
            }
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("list") {
            for line in contents.lines().map(str::trim).filter(|line| {
                !line.is_empty()
                    && !line.starts_with('#')
                    && line.starts_with("deb")
                    && !is_vendor_apt_repository(os_ids, line)
            }) {
                let line = redact_uri_credentials(line);
                repositories.push(Repository {
                    name: format!("{filename}: {line}"),
                    source: source.clone(),
                });
            }
        }
    }
    Ok(())
}

fn read_pacman_repositories(root: &Path, repositories: &mut Vec<Repository>) -> Result<()> {
    let path = root.join("etc/pacman.conf");
    if !path.is_file() {
        return Ok(());
    }
    let contents = fs::read_to_string(&path)
        .with_context(|| format!("reading repository file {}", path.display()))?;
    let source = display_source(root, &path);
    for line in contents.lines().map(str::trim) {
        if let Some(section) = line
            .strip_prefix('[')
            .and_then(|line| line.strip_suffix(']'))
            && !matches!(
                section,
                "options"
                    | "core"
                    | "extra"
                    | "multilib"
                    | "community"
                    | "testing"
                    | "core-testing"
                    | "extra-testing"
                    | "multilib-testing"
            )
        {
            repositories.push(Repository {
                name: section.to_string(),
                source: source.clone(),
            });
        }
    }
    Ok(())
}

fn write_private(path: &Path, contents: &str) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    file.write_all(contents.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("setting permissions on {}", path.display()))
}

pub fn write_reports(report: &SoftwareReport, directory: &Path) -> Result<(PathBuf, PathBuf)> {
    fs::create_dir_all(directory)
        .with_context(|| format!("creating software report directory {}", directory.display()))?;
    let json_path = directory.join(JSON_REPORT_FILE);
    let markdown_path = directory.join(MARKDOWN_REPORT_FILE);
    write_private(&json_path, &report.to_json()?)?;
    write_private(&markdown_path, &report.to_markdown())?;
    Ok((json_path, markdown_path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(name: &str, files: &[&str]) -> InstalledPackage {
        InstalledPackage {
            name: name.to_string(),
            files: files.iter().map(|file| (*file).to_string()).collect(),
        }
    }

    #[test]
    fn classifier_table() {
        let cases = [
            (
                package(
                    "gnome-calculator",
                    &[
                        "/usr/share/applications/org.gnome.Calculator.desktop",
                        "/usr/lib/systemd/system/gnome-calculator-search-provider.service",
                    ],
                ),
                PackageKind::DesktopApplication,
                ReplacementKind::Flatpak,
                "flatpak install flathub org.gnome.Calculator",
            ),
            (
                package("ripgrep", &["/usr/bin/rg"]),
                PackageKind::CliTool,
                ReplacementKind::Homebrew,
                "brew install ripgrep",
            ),
            (
                package("my-cli", &["/usr/bin/my-cli"]),
                PackageKind::CliTool,
                ReplacementKind::ToolboxOrDistrobox,
                "toolbox/distrobox",
            ),
            (
                package(
                    "postgresql-server",
                    &["/usr/lib/systemd/system/postgresql.service"],
                ),
                PackageKind::SystemService,
                ReplacementKind::DerivedImage,
                "derived image",
            ),
            (
                package("akmod-nvidia", &["/usr/src/akmods/nvidia.spec"]),
                PackageKind::KernelModule,
                ReplacementKind::DerivedImage,
                "derived image",
            ),
            (
                package("libexample", &["/usr/lib64/libexample.so.1"]),
                PackageKind::Other,
                ReplacementKind::ToolboxOrDistrobox,
                "toolbox/distrobox",
            ),
        ];

        for (package, kind, replacement_kind, replacement_text) in cases {
            let classified = classify(&package);
            assert_eq!(classified.kind, kind, "{} kind", package.name);
            assert_eq!(
                classified.replacement_kind, replacement_kind,
                "{} replacement kind",
                package.name
            );
            assert!(
                classified.replacement.contains(replacement_text),
                "{} replacement was {}",
                package.name,
                classified.replacement
            );
        }
    }

    #[test]
    fn package_and_desktop_report_includes_suggestions() {
        let host = HostInventory {
            packages: vec![
                package("bash", &["/usr/bin/bash"]),
                package("ripgrep", &["/usr/bin/rg"]),
                package(
                    "gnome-calculator",
                    &["/usr/share/applications/org.gnome.Calculator.desktop"],
                ),
                package(
                    "postgresql-server",
                    &["/usr/lib/systemd/system/postgresql.service"],
                ),
            ],
            repositories: vec![Repository {
                name: "copr:example/tools".to_string(),
                source: "/etc/yum.repos.d/_copr_example.repo".to_string(),
            }],
            enabled_services: vec!["postgresql.service".to_string()],
            enabled_services_complete: true,
            warnings: Vec::new(),
        };
        let target = TargetInventory {
            packages: BTreeSet::from(["bash".to_string()]),
            system_units: BTreeSet::new(),
            package_db: Some(PackageDb::Rpm),
            complete: true,
            warning: None,
        };

        let report = build(host, &target, "quay.io/fedora/fedora-bootc:44");
        assert_eq!(
            report
                .missing_packages
                .iter()
                .map(|package| package.name.as_str())
                .collect::<Vec<_>>(),
            ["ripgrep", "gnome-calculator", "postgresql-server"]
        );
        assert_eq!(
            report.missing_enabled_services,
            [MissingService {
                unit: "postgresql.service".to_string(),
                source_package: Some("postgresql-server".to_string()),
            }]
        );
        let markdown = report.to_markdown();
        assert!(markdown.contains("flatpak install flathub org.gnome.Calculator"));
        assert!(markdown.contains("brew install ripgrep"));
        assert!(markdown.contains("RUN dnf -y install postgresql-server"));
        assert!(markdown.contains("copr:example/tools"));
    }

    #[test]
    fn dpkg_fixture_end_to_end_lists_cli_and_desktop_replacements() {
        fn write_dpkg_root(root: &Path, packages: &[(&str, &[&str])]) {
            let info = root.join("var/lib/dpkg/info");
            fs::create_dir_all(&info).unwrap();
            let mut status = String::new();
            for (name, files) in packages {
                let _ = writeln!(
                    status,
                    "Package: {name}\nStatus: install ok installed\nArchitecture: amd64\n"
                );
                fs::write(info.join(format!("{name}.list")), files.join("\n")).unwrap();
            }
            fs::write(root.join("var/lib/dpkg/status"), status).unwrap();
        }

        let host_root = tempfile::tempdir().unwrap();
        write_dpkg_root(
            host_root.path(),
            &[
                ("bash", &["/usr/bin/bash"]),
                ("ripgrep", &["/usr/bin/rg"]),
                (
                    "gnome-calculator",
                    &["/usr/share/applications/org.gnome.Calculator.desktop"],
                ),
            ],
        );
        let target_root = tempfile::tempdir().unwrap();
        write_dpkg_root(target_root.path(), &[("bash", &["/usr/bin/bash"])]);

        let host = gather_host(host_root.path(), PackageDb::Dpkg).unwrap();
        let target = target_inventory_from_root(target_root.path(), 0);
        let report = build(host, &target, "example.invalid/debian-bootc:latest");

        let markdown = report.to_markdown();
        assert_eq!(
            report
                .missing_packages
                .iter()
                .map(|package| package.name.as_str())
                .collect::<Vec<_>>(),
            ["gnome-calculator", "ripgrep"]
        );
        assert!(markdown.contains("flatpak install flathub org.gnome.Calculator"));
        assert!(markdown.contains("brew install ripgrep"));
    }

    #[test]
    fn incomplete_target_never_claims_software_is_missing() {
        let host = HostInventory {
            packages: vec![package("ripgrep", &["/usr/bin/rg"])],
            repositories: Vec::new(),
            enabled_services: vec!["example.service".to_string()],
            enabled_services_complete: true,
            warnings: Vec::new(),
        };
        let target = TargetInventory {
            warning: Some("one layer was skipped".to_string()),
            ..Default::default()
        };
        let report = build(host, &target, "example.invalid/image:latest");
        assert!(!report.comparison_complete);
        assert!(report.missing_packages.is_empty());
        assert!(report.missing_enabled_services.is_empty());
        assert_eq!(
            report.warnings,
            [
                "one layer was skipped",
                "Missing-package and missing-service comparisons were suppressed because the target inventory is incomplete."
            ]
        );
    }

    #[test]
    fn package_database_parsers() {
        let rpm = parse_rpm_query(
            "@@BOOTC-MIGRATE-PACKAGE@@app\n/usr/bin/app\n/usr/share/applications/org.example.App.desktop\n@@BOOTC-MIGRATE-PACKAGE@@lib\n/usr/lib64/lib.so\n",
        )
        .unwrap();
        assert_eq!(
            rpm,
            [
                package(
                    "app",
                    &[
                        "/usr/bin/app",
                        "/usr/share/applications/org.example.App.desktop"
                    ]
                ),
                package("lib", &["/usr/lib64/lib.so"]),
            ]
        );
        assert_eq!(
            parse_dpkg_status(
                "Package: installed\nStatus: install ok installed\n\nPackage: removed\nStatus: deinstall ok config-files\n"
            ),
            ["installed"]
        );
        assert_eq!(
            pacman_files("%FILES%\nusr/bin/tool\nusr/share/doc/tool/\n\n"),
            ["/usr/bin/tool"]
        );
    }

    #[test]
    fn target_inventory_reads_dpkg_files_and_unowned_units() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("var/lib/dpkg/info")).unwrap();
        fs::write(
            root.path().join("var/lib/dpkg/status"),
            "Package: app\nStatus: install ok installed\n",
        )
        .unwrap();
        fs::write(
            root.path().join("var/lib/dpkg/info/app.list"),
            "/usr/bin/app\n/usr/lib/systemd/system/app.service\n",
        )
        .unwrap();
        fs::create_dir_all(root.path().join("usr/lib/systemd/system")).unwrap();
        fs::write(
            root.path().join("usr/lib/systemd/system/generated.service"),
            "[Service]\nExecStart=/usr/bin/true\n",
        )
        .unwrap();

        let inventory = target_inventory_from_root(root.path(), 0);
        assert!(inventory.complete);
        assert_eq!(inventory.packages, BTreeSet::from(["app".to_string()]));
        assert_eq!(
            inventory.system_units,
            BTreeSet::from(["app.service".to_string(), "generated.service".to_string(),])
        );
    }

    #[test]
    fn repository_parsers_name_enabled_sources() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("etc/yum.repos.d")).unwrap();
        fs::write(root.path().join("etc/os-release"), "ID=debian\n").unwrap();
        fs::write(
            root.path().join("etc/yum.repos.d/example.repo"),
            "[enabled]\nname=Example tools\nenabled=1\n[disabled]\nname=Old\nenabled=0\n",
        )
        .unwrap();
        fs::create_dir_all(root.path().join("etc/apt/sources.list.d")).unwrap();
        fs::write(
            root.path().join("etc/apt/sources.list.d/vendor.list"),
            "# disabled\ndeb https://user:secret@packages.example.test stable main\n",
        )
        .unwrap();
        fs::write(
            root.path().join("etc/apt/sources.list.d/debian.sources"),
            "Types: deb\nURIs: http://deb.debian.org/debian\nSuites: stable\n\nTypes: deb\nURIs: https://disabled.example.test/debian\nSuites: stable\nEnabled: no\n",
        )
        .unwrap();
        fs::write(
            root.path().join("etc/pacman.conf"),
            "[options]\n[core]\n[company]\nServer = https://packages.example.test/$arch\n",
        )
        .unwrap();

        assert_eq!(
            read_repositories(root.path()).unwrap(),
            [
                Repository {
                    name: "Example tools (enabled)".to_string(),
                    source: "/etc/yum.repos.d/example.repo".to_string(),
                },
                Repository {
                    name: "company".to_string(),
                    source: "/etc/pacman.conf".to_string(),
                },
                Repository {
                    name: "vendor.list: deb https://<redacted>@packages.example.test stable main"
                        .to_string(),
                    source: "/etc/apt/sources.list.d/vendor.list".to_string(),
                },
            ]
        );
    }

    #[test]
    fn derived_image_snippet_matches_target_package_database() {
        let cases = [
            ("rpm", "RUN dnf -y install daemon"),
            ("dpkg", "RUN apt-get update && apt-get install -y daemon"),
            ("pacman", "RUN pacman -Syu --noconfirm daemon"),
        ];
        for (package_db, expected) in cases {
            let report = SoftwareReport {
                target_image: "example.invalid/image:latest".to_string(),
                target_package_db: Some(package_db.to_string()),
                comparison_complete: true,
                missing_packages: vec![MissingPackage {
                    name: "daemon".to_string(),
                    kind: PackageKind::SystemService,
                    replacement_kind: ReplacementKind::DerivedImage,
                    replacement: "derived image".to_string(),
                }],
                repositories: Vec::new(),
                missing_enabled_services: Vec::new(),
                warnings: Vec::new(),
            };
            assert!(
                report.to_markdown().contains(expected),
                "{package_db} snippet did not contain {expected}"
            );
        }
    }

    #[test]
    fn report_files_are_json_and_markdown_twins() {
        let directory = tempfile::tempdir().unwrap();
        let report = SoftwareReport {
            target_package_db: Some("rpm".to_string()),
            target_image: "example.invalid/image:latest".to_string(),
            comparison_complete: true,
            missing_packages: Vec::new(),
            repositories: Vec::new(),
            missing_enabled_services: Vec::new(),
            warnings: Vec::new(),
        };
        let (json, markdown) = write_reports(&report, directory.path()).unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&json).unwrap()).unwrap();
        assert_eq!(value["target_image"], "example.invalid/image:latest");
        assert!(
            fs::read_to_string(&markdown)
                .unwrap()
                .starts_with("# Software report\n")
        );
        assert_eq!(
            fs::metadata(json).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(markdown).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
