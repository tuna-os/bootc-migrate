//! Target-image capability scan (issue #24).
//!
//! Answers "what can this image be re-based to, on this machine" **before
//! anything is staged**, from a handful of probe files that registry
//! streaming (`crate::registry`) can pull without a full image pull (peak
//! disk ≈ one layer).
//!
//! This module is the pure half: parsers for each probe file and the
//! assembly into [`Capabilities`]. Fetching is a seam — callers hand in
//! [`ProbeFiles`] however they obtained them (registry streaming, a mounted
//! image, fixtures in tests). Routing consumes the result to auto-fill
//! `--target-backend` and refuse impossible routes with evidence instead of
//! mid-phase failures.

use serde::Serialize;
use std::path::Path;

/// Identity of a base image, from `/usr/lib/os-release`, plus the
/// package-manager family its `/usr/bin` reveals (bootc-migrate#256).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BaseInfo {
    pub id: String,
    pub id_like: Option<String>,
    pub version_id: Option<String>,
    /// The package manager the image ships, when a known one is found.
    /// `os-release` alone misclassifies real pairs: Bluefin LTS declares
    /// `ID=centos ID_LIKE="rhel fedora"` and Dakota `ID=bluefin-dakota
    /// ID_LIKE="org.gnome.os"`, which share no name yet are both dnf-managed
    /// Fedora-lineage images with the same `/etc` conventions. The package
    /// manager is the second signal [`lineage`] uses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pkg_family: Option<PkgFamily>,
}

/// The package-manager family an image is built with. The *frontend*, not
/// the archive format: Fedora and openSUSE both use rpm, and their `/etc`
/// (repositories, PAM, service defaults) still has nothing in common.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PkgFamily {
    /// dnf / dnf5 / microdnf / yum / rpm-ostree: Fedora, RHEL, CentOS and
    /// their derivatives.
    Dnf,
    /// zypper: openSUSE and SUSE.
    Zypp,
    /// apt / dpkg: Debian and Ubuntu.
    Apt,
    /// pacman: Arch.
    Pacman,
    /// apk: Alpine.
    Apk,
}

impl PkgFamily {
    /// The name printed in gate messages.
    pub fn label(self) -> &'static str {
        match self {
            PkgFamily::Dnf => "dnf",
            PkgFamily::Zypp => "zypper",
            PkgFamily::Apt => "apt",
            PkgFamily::Pacman => "pacman",
            PkgFamily::Apk => "apk",
        }
    }
}

/// `usr/bin` names that identify a [`PkgFamily`]. Any one of them is enough;
/// a distribution ships at least one, and image builders keep the frontend
/// even when they strip the rest.
pub const PKG_FAMILY_BINARIES: &[(&str, PkgFamily)] = &[
    ("dnf", PkgFamily::Dnf),
    ("dnf5", PkgFamily::Dnf),
    ("dnf-3", PkgFamily::Dnf),
    ("microdnf", PkgFamily::Dnf),
    ("yum", PkgFamily::Dnf),
    ("rpm-ostree", PkgFamily::Dnf),
    ("zypper", PkgFamily::Zypp),
    ("apt", PkgFamily::Apt),
    ("apt-get", PkgFamily::Apt),
    ("dpkg", PkgFamily::Apt),
    ("pacman", PkgFamily::Pacman),
    ("apk", PkgFamily::Apk),
];

/// The package-manager family of the image rooted at `root`, from the
/// binaries listed in [`PKG_FAMILY_BINARIES`]. A symlink counts even when
/// dangling (`dnf -> dnf5` inside a partial registry extraction), so the
/// check is on the directory entry, not on a resolved target.
pub fn pkg_family_from_root(root: &Path) -> Option<PkgFamily> {
    let bin = root.join("usr/bin");
    PKG_FAMILY_BINARIES
        .iter()
        .find(|(name, _)| std::fs::symlink_metadata(bin.join(name)).is_ok())
        .map(|(_, family)| *family)
}

/// One `/usr/lib/sysusers.d/*.conf` allocation. Only `u`/`g` lines with an
/// explicit numeric id matter for cross-base planning (issue #67); dynamic
/// (`-`) allocations are recorded without an id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SysusersEntry {
    /// `'u'` (user, possibly with paired group) or `'g'` (group).
    pub kind: char,
    pub name: String,
    pub id: Option<u32>,
}

/// Whether the target's `prepare-root.conf` enables the composefs root, and
/// under which mode. ostree's `[composefs] enabled` accepts `true`/`yes`/`1`
/// (plain enable) and `maybe` (signed-only: composefs is used only when the
/// image carries a valid signature, which in practice means fs-verity
/// sealing is required — see `docs/filesystem-support.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ComposefsMode {
    Disabled,
    Enabled,
    /// `enabled = maybe`: signed-only — composefs requires a valid (i.e.
    /// fs-verity-sealed) signature to activate.
    SignedOnly,
}

impl ComposefsMode {
    pub fn is_enabled(self) -> bool {
        !matches!(self, ComposefsMode::Disabled)
    }

    /// Whether this mode requires fs-verity sealing (tightens Phase 3's
    /// policy — see issue #24's capability-scan acceptance criteria).
    pub fn requires_verity(self) -> bool {
        matches!(self, ComposefsMode::SignedOnly)
    }
}

/// The `[root]`/`[etc]` `transient = true|false` settings from
/// `prepare-root.conf`. Transient means the section is composed fresh from
/// the image's own defaults every boot rather than persisted — affects
/// whether a user's `/etc` edits actually survive a re-base (issue #24).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct PrepareRootInfo {
    pub composefs: Option<ComposefsMode>,
    pub root_transient: bool,
    pub etc_transient: bool,
}

/// The probe files the scan reads. Every field is optional — a missing file
/// is a signal (e.g. no prepare-root.conf ⇒ not an ostree/bootc image), not
/// an error.
#[derive(Debug, Clone, Default)]
pub struct ProbeFiles {
    /// `/usr/lib/os-release` content.
    pub os_release: Option<String>,
    /// `/usr/lib/ostree/prepare-root.conf` content.
    pub prepare_root: Option<String>,
    /// Contents of each `/usr/lib/sysusers.d/*.conf`.
    pub sysusers: Vec<String>,
    /// File names under `/usr/share/xsessions` + `/usr/share/wayland-sessions`.
    pub session_files: Vec<String>,
    /// `/usr/lib/systemd/boot/efi/systemd-bootx64.efi` present.
    pub has_systemd_boot_payload: bool,
    /// `/usr/bin/bootc` (or `/usr/lib/bootc/`) present.
    pub has_bootc: bool,
    /// `/usr/bin/bootupctl` present together with `/usr/lib/bootupd/updates`:
    /// the image can install a bootloader through bootupd, which bootc's
    /// ostree backend requires (#260).
    pub has_bootupd: bool,
    /// Any `/usr/lib/dracut/modules.d/*composefs*` directory name present —
    /// signals the image's initramfs already integrates composefs support,
    /// so it likely doesn't need regenerating for that reason.
    pub dracut_module_dir_names: Vec<String>,
    /// Content of a `/usr/lib/bootc/install/*.toml` file, if present (bootc
    /// install config — may declare a `root-fs` filesystem expectation).
    pub bootc_install_config: Option<String>,
    /// The package-manager family found under `usr/bin` (#256).
    pub pkg_family: Option<PkgFamily>,
}

/// What the target image supports, assembled from [`ProbeFiles`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Capabilities {
    /// prepare-root.conf enables the composefs root (`enabled = true|yes|maybe`).
    pub composefs_capable: bool,
    /// The image is ostree-based at all (prepare-root.conf present).
    pub ostree_capable: bool,
    /// Ships the systemd-boot EFI payload (bootloader migration possible
    /// even when the *source* OS lacks the binaries — the phase-5 property).
    pub systemd_boot_payload: bool,
    /// Ships bootc (required for switch-based strategies).
    pub bootc_present: bool,
    /// Ships bootupd (required by `bootc install` on the ostree backend,
    /// the composefs → ostree route's engine; #260).
    pub bootupd_present: bool,
    /// Desktop environments inventoried from session files (`gnome`, `kde`, …).
    pub desktops: Vec<String>,
    /// Base identity for cross-base gating (#67).
    pub base: Option<BaseInfo>,
    /// Statically-allocated sysusers ids, input to the remap planner (#67).
    pub sysusers: Vec<SysusersEntry>,
    /// `enabled = verity`: fs-verity is required, tightening Phase 3's policy.
    pub fs_verity_required: bool,
    /// `[root] transient = true`: the target composes its root fresh every
    /// boot rather than persisting it.
    pub root_transient: bool,
    /// `[etc] transient = true`: the target's `/etc` is composed fresh every
    /// boot — a user's live `/etc` edits will NOT survive on this target
    /// unless the migration's own merge captures them into the deployment
    /// (as `mergetc`/native `bootc switch` merge both already do); this flag
    /// is informational context for that merge, not a gate.
    pub etc_transient: bool,
    /// Whether the image's initramfs already ships a composefs dracut
    /// module (`/usr/lib/dracut/modules.d/*composefs*`) — if not, an
    /// initramfs regeneration may be needed for a composefs boot.
    pub initramfs_has_composefs_module: bool,
    /// The filesystem `bootc install` expects for the root, parsed from
    /// `/usr/lib/bootc/install/*.toml`'s `root-fs` key, when present.
    pub filesystem_expectation: Option<String>,
}

impl Capabilities {
    /// The machine-readable form (`bootc-rebase scan --json`).
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("Capabilities serialization cannot fail")
    }
}

/// Strip surrounding single or double quotes, os-release style.
fn unquote(v: &str) -> &str {
    let v = v.trim();
    v.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| v.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(v)
}

/// Parse the identity fields out of os-release content.
pub fn parse_base_info(content: &str) -> Option<BaseInfo> {
    let mut id = None;
    let mut id_like = None;
    let mut version_id = None;
    for line in content.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("ID=") {
            id = Some(unquote(v).to_string());
        } else if let Some(v) = line.strip_prefix("ID_LIKE=") {
            id_like = Some(unquote(v).to_string());
        } else if let Some(v) = line.strip_prefix("VERSION_ID=") {
            version_id = Some(unquote(v).to_string());
        }
    }
    Some(BaseInfo {
        id: id?,
        id_like,
        version_id,
        pkg_family: None,
    })
}

/// Whether prepare-root.conf enables the composefs root. ostree accepts
/// `true`/`yes`/`1` and the signed-only mode `maybe` — all of them mean the
/// initrd can set up a composefs root.
pub fn prepare_root_enables_composefs(content: &str) -> bool {
    parse_prepare_root_info(content)
        .composefs
        .is_some_and(ComposefsMode::is_enabled)
}

/// Parse `prepare-root.conf`'s `[composefs] enabled`, `[root] transient`,
/// and `[etc] transient` keys (issue #24). Unlike
/// [`prepare_root_enables_composefs`], this walks every section instead of
/// stopping at the first `[composefs] enabled` match, so `root`/`etc`
/// transience is captured regardless of section order.
pub fn parse_prepare_root_info(content: &str) -> PrepareRootInfo {
    let mut info = PrepareRootInfo::default();
    let mut section = "";
    for line in content.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            section = match name.to_ascii_lowercase().as_str() {
                "composefs" => "composefs",
                "root" => "root",
                "etc" => "etc",
                _ => "",
            };
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let k = k.trim().to_ascii_lowercase();
        let v = unquote(v).to_ascii_lowercase();
        match (section, k.as_str()) {
            ("composefs", "enabled") if info.composefs.is_none() => {
                info.composefs = Some(match v.as_str() {
                    "maybe" => ComposefsMode::SignedOnly,
                    "true" | "yes" | "1" => ComposefsMode::Enabled,
                    _ => ComposefsMode::Disabled,
                });
            }
            ("root", "transient") => {
                info.root_transient = matches!(v.as_str(), "true" | "yes" | "1")
            }
            ("etc", "transient") => info.etc_transient = matches!(v.as_str(), "true" | "yes" | "1"),
            _ => {}
        }
    }
    info
}

/// Parse sysusers.d content: `u name id …` / `g name id …`. Ranges, `-`,
/// and `uid:gid` forms keep only the leading uid; malformed lines are
/// skipped (the scan reports, it must not fail).
pub fn parse_sysusers(content: &str) -> Vec<SysusersEntry> {
    content
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let mut f = line.split_whitespace();
            let kind = match f.next()? {
                "u" | "u!" => 'u',
                "g" => 'g',
                _ => return None, // m/r lines carry no allocation
            };
            let name = f.next()?.to_string();
            let id = f.next().and_then(|tok| {
                let lead = tok.split(&[':', '-'][..]).next().unwrap_or("");
                lead.parse().ok()
            });
            Some(SysusersEntry { kind, name, id })
        })
        .collect()
}

/// Map session-file names to desktop identifiers. Unknown names are kept
/// verbatim (minus extension) so nothing is silently dropped.
pub fn desktops_from_sessions(session_files: &[String]) -> Vec<String> {
    let mut out: Vec<String> = session_files
        .iter()
        .filter_map(|f| {
            let stem = f
                .trim_end_matches(".desktop")
                .to_ascii_lowercase()
                .to_string();
            if stem.is_empty() {
                return None;
            }
            Some(if stem.contains("gnome") {
                "gnome".to_string()
            } else if stem.contains("plasma") {
                "kde".to_string()
            } else if stem.contains("xfce") {
                "xfce".to_string()
            } else if stem.contains("cinnamon") {
                "cinnamon".to_string()
            } else if stem.contains("cosmic") {
                "cosmic".to_string()
            } else if stem.contains("sway") {
                "sway".to_string()
            } else {
                stem
            })
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Assemble the capability report from probe files.
pub fn assemble(probe: &ProbeFiles) -> Capabilities {
    let prepare_root_info = probe
        .prepare_root
        .as_deref()
        .map(parse_prepare_root_info)
        .unwrap_or_default();
    Capabilities {
        composefs_capable: prepare_root_info
            .composefs
            .is_some_and(ComposefsMode::is_enabled),
        ostree_capable: probe.prepare_root.is_some(),
        systemd_boot_payload: probe.has_systemd_boot_payload,
        bootc_present: probe.has_bootc,
        bootupd_present: probe.has_bootupd,
        desktops: desktops_from_sessions(&probe.session_files),
        base: probe
            .os_release
            .as_deref()
            .and_then(parse_base_info)
            .map(|base| BaseInfo {
                pkg_family: probe.pkg_family,
                ..base
            }),
        sysusers: probe
            .sysusers
            .iter()
            .flat_map(|c| parse_sysusers(c))
            .collect(),
        fs_verity_required: prepare_root_info
            .composefs
            .is_some_and(ComposefsMode::requires_verity),
        root_transient: prepare_root_info.root_transient,
        etc_transient: prepare_root_info.etc_transient,
        initramfs_has_composefs_module: probe
            .dracut_module_dir_names
            .iter()
            .any(|n| n.contains("composefs")),
        filesystem_expectation: probe
            .bootc_install_config
            .as_deref()
            .and_then(parse_bootc_install_filesystem),
    }
}

/// Extract the `root-fs` value from a bootc install config TOML, if present.
/// The key can appear top-level or nested under an `[install.filesystem...]`
/// section header — either way this looks for a bare `root-fs = "value"`
/// line, since bootc's own config schema has moved between both shapes
/// across releases and this is advisory context, not a gate.
fn parse_bootc_install_filesystem(content: &str) -> Option<String> {
    for line in content.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("root-fs") else {
            continue;
        };
        let Some(rest) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let value = rest.trim().trim_matches('"').trim_matches('\'');
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

/// Why [`is_compatible`] returned `false`, in user-facing terms — empty when
/// compatible. Kept as a separate function (rather than folding extra
/// advisory-only signals like `bootc_present` into the compatibility gate)
/// so the YES/NO verdict doesn't change behavior for images this scan
/// already reported as usable.
pub fn compatibility_issues(caps: &Capabilities) -> Vec<String> {
    let mut issues = Vec::new();
    if !caps.ostree_capable && !caps.composefs_capable {
        issues.push(
            "no usr/lib/ostree/prepare-root.conf found — not an ostree/bootc image".to_string(),
        );
    }
    issues
}

/// Whether the scanned image is usable as a re-base target at all.
pub fn is_compatible(caps: &Capabilities) -> bool {
    compatibility_issues(caps).is_empty()
}

/// Cross-base gate (#67): true when the target's base family differs from
/// the host's. Same `ID` is same-base; otherwise membership of either side's
/// `ID_LIKE` chain still counts as same family (fedora ↔ "ID_LIKE=fedora").
pub fn is_cross_base(host: &BaseInfo, target: &BaseInfo) -> bool {
    if host.id == target.id {
        return false;
    }
    let like_contains = |like: &Option<String>, id: &str| {
        like.as_deref()
            .map(|l| l.split_whitespace().any(|w| w == id))
            .unwrap_or(false)
    };
    !(like_contains(&host.id_like, &target.id) || like_contains(&target.id_like, &host.id))
}

/// How two bases relate, at the granularity the composefs conversion route
/// cares about (bootc-migrate#256).
///
/// [`is_cross_base`] answers "do these two images disagree on identity at
/// all?" — Bluefin and Dakota do (different `ID`, diverging `wheel` gid),
/// and the OstreeDeploy route rightly plans a remap for them. But both
/// declare `ID_LIKE=fedora`: they are the same *family*, their `/etc`
/// defaults share a vendor lineage, and the same-lineage 3-way merge is the
/// right merge for them. Fedora → openSUSE shares nothing, and there the
/// same merge quietly carries one family's `/etc` onto the other.
///
/// `ID_LIKE` is one signal, not the verdict. Downstream images relabel
/// themselves freely (Bluefin LTS is `centos`, Dakota is
/// `ID_LIKE="org.gnome.os"`), so the package-manager family the image
/// ships ([`BaseInfo::pkg_family`]) is the second: two dnf-managed images
/// share Fedora's `/etc` conventions whatever their `ID_LIKE` says, and a
/// dnf image and a zypper image never do, whatever theirs say.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Lineage {
    /// Identical `ID`.
    SameBase,
    /// Different `ID`, but a shared family: one side's `ID_LIKE` names the
    /// other's `ID`, the two `ID_LIKE` lists intersect, or both ship the
    /// same package-manager family.
    SameFamily,
    /// No identity overlap and a different package-manager family on each
    /// side: positively cross-family.
    CrossFamily,
    /// No identity overlap and no package-manager evidence on at least one
    /// side. Not proven either way; callers warn and keep the standard
    /// merge rather than refuse or invert the policy on a guess.
    Unknown,
}

/// Classify the relation between `host` and `target` — see [`Lineage`].
pub fn lineage(host: &BaseInfo, target: &BaseInfo) -> Lineage {
    if host.id == target.id {
        return Lineage::SameBase;
    }
    let words = |like: &Option<String>| -> Vec<String> {
        like.as_deref()
            .map(|l| l.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default()
    };
    let host_like = words(&host.id_like);
    let target_like = words(&target.id_like);
    let related = host_like.contains(&target.id)
        || target_like.contains(&host.id)
        || host_like.iter().any(|w| target_like.contains(w));
    if related {
        return Lineage::SameFamily;
    }
    match (host.pkg_family, target.pkg_family) {
        (Some(h), Some(t)) if h == t => Lineage::SameFamily,
        (Some(_), Some(_)) => Lineage::CrossFamily,
        _ => Lineage::Unknown,
    }
}

/// Read this host's own base identity from `/etc/os-release` (falling back
/// to `/usr/lib/os-release`, the same precedence os-release(5) specifies),
/// for cross-base gating (#67) against a target image's scanned identity.
/// `None` if neither file is readable or parseable — callers treat that as
/// "can't establish identity, don't gate."
pub fn read_host_base_info() -> Option<BaseInfo> {
    read_base_info_from(
        Path::new("/etc/os-release"),
        Path::new("/usr/lib/os-release"),
    )
    .map(|base| BaseInfo {
        pkg_family: pkg_family_from_root(Path::new("/")),
        ..base
    })
}

/// Read a mounted image's base identity from its root: `usr/lib/os-release`
/// first (the vendor copy, always present in a bootc image), then
/// `etc/os-release`. Registry-independent — this is what Phase 4 uses to
/// decide the cross-family policy authoritatively (#256).
pub fn read_base_info_from_root(root: &Path) -> Option<BaseInfo> {
    read_base_info_from(
        &root.join("usr/lib/os-release"),
        &root.join("etc/os-release"),
    )
    .map(|base| BaseInfo {
        pkg_family: pkg_family_from_root(root),
        ..base
    })
}

/// Testable core of [`read_host_base_info`]: try `primary`, falling back to
/// `secondary`, parsing whichever is readable first.
fn read_base_info_from(primary: &Path, secondary: &Path) -> Option<BaseInfo> {
    let content = std::fs::read_to_string(primary)
        .or_else(|_| std::fs::read_to_string(secondary))
        .ok()?;
    parse_base_info(&content)
}

/// Fetch probe files from an image ref via registry streaming.
pub fn fetch_probe_files(image_ref: &str) -> anyhow::Result<ProbeFiles> {
    crate::registry::fetch_probe_files_via_registry(image_ref)
}

/// Scan a target image via registry streaming and return its [`Capabilities`].
pub fn scan_target_image(image_ref: &str) -> anyhow::Result<Capabilities> {
    let probe = fetch_probe_files(image_ref)?;
    Ok(assemble(&probe))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_base_info_from_prefers_primary() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("etc-os-release");
        let secondary = dir.path().join("usr-os-release");
        std::fs::write(&primary, "ID=dakota\n").unwrap();
        std::fs::write(&secondary, "ID=fallback\n").unwrap();
        let b = read_base_info_from(&primary, &secondary).unwrap();
        assert_eq!(b.id, "dakota");
    }

    #[test]
    fn read_base_info_from_falls_back_when_primary_missing() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("does-not-exist");
        let secondary = dir.path().join("usr-os-release");
        std::fs::write(&secondary, "ID=fallback\n").unwrap();
        let b = read_base_info_from(&primary, &secondary).unwrap();
        assert_eq!(b.id, "fallback");
    }

    #[test]
    fn read_base_info_from_none_when_neither_exists() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("nope1");
        let secondary = dir.path().join("nope2");
        assert!(read_base_info_from(&primary, &secondary).is_none());
    }

    #[test]
    fn base_info_parses_quoted_and_unquoted() {
        let b = parse_base_info("NAME=\"Fedora Linux\"\nID=fedora\nVERSION_ID=44\n").unwrap();
        assert_eq!(b.id, "fedora");
        assert_eq!(b.version_id.as_deref(), Some("44"));
        assert_eq!(b.id_like, None);

        let b = parse_base_info("ID='centos'\nID_LIKE=\"rhel fedora\"\n").unwrap();
        assert_eq!(b.id, "centos");
        assert_eq!(b.id_like.as_deref(), Some("rhel fedora"));
    }

    #[test]
    fn prepare_root_variants() {
        assert!(prepare_root_enables_composefs(
            "[composefs]\nenabled = true\n"
        ));
        assert!(prepare_root_enables_composefs(
            "[etc]\ntransient = false\n[composefs]\nenabled = maybe\n"
        ));
        assert!(!prepare_root_enables_composefs(
            "[composefs]\nenabled = false\n"
        ));
        // `enabled` under a different section must not count.
        assert!(!prepare_root_enables_composefs(
            "[root]\nenabled = true\n[composefs]\n"
        ));
        assert!(!prepare_root_enables_composefs(""));
    }

    #[test]
    fn prepare_root_info_plain_enabled() {
        let info = parse_prepare_root_info("[composefs]\nenabled = true\n");
        assert_eq!(info.composefs, Some(ComposefsMode::Enabled));
        assert!(!info.composefs.unwrap().requires_verity());
        assert!(!info.root_transient);
        assert!(!info.etc_transient);
    }

    #[test]
    fn prepare_root_info_signed_only_requires_verity() {
        let info = parse_prepare_root_info("[composefs]\nenabled = maybe\n");
        assert_eq!(info.composefs, Some(ComposefsMode::SignedOnly));
        assert!(info.composefs.unwrap().requires_verity());
        assert!(info.composefs.unwrap().is_enabled());
    }

    #[test]
    fn prepare_root_info_disabled_does_not_require_verity() {
        let info = parse_prepare_root_info("[composefs]\nenabled = false\n");
        assert_eq!(info.composefs, Some(ComposefsMode::Disabled));
        assert!(!info.composefs.unwrap().is_enabled());
        assert!(!info.composefs.unwrap().requires_verity());
    }

    #[test]
    fn prepare_root_info_captures_transient_root_and_etc_regardless_of_order() {
        let info = parse_prepare_root_info(
            "[etc]\ntransient = true\n[composefs]\nenabled = true\n[root]\ntransient = yes\n",
        );
        assert_eq!(info.composefs, Some(ComposefsMode::Enabled));
        assert!(info.root_transient);
        assert!(info.etc_transient);
    }

    #[test]
    fn prepare_root_info_transient_false_is_not_transient() {
        let info = parse_prepare_root_info("[root]\ntransient = false\n[etc]\ntransient = 0\n");
        assert!(!info.root_transient);
        assert!(!info.etc_transient);
    }

    #[test]
    fn prepare_root_info_transient_outside_its_section_is_ignored() {
        // A `transient` key under [composefs] (or any other section) must
        // not be mistaken for [root]/[etc] transience.
        let info = parse_prepare_root_info("[composefs]\ntransient = true\nenabled = true\n");
        assert!(!info.root_transient);
        assert!(!info.etc_transient);
    }

    #[test]
    fn prepare_root_info_empty_content_is_all_defaults() {
        let info = parse_prepare_root_info("");
        assert_eq!(info.composefs, None);
        assert!(!info.root_transient);
        assert!(!info.etc_transient);
    }

    #[test]
    fn assemble_wires_fs_verity_and_transient_flags() {
        let probe = ProbeFiles {
            prepare_root: Some(
                "[composefs]\nenabled = maybe\n[root]\ntransient = true\n[etc]\ntransient = true\n"
                    .to_string(),
            ),
            ..Default::default()
        };
        let caps = assemble(&probe);
        assert!(caps.composefs_capable);
        assert!(caps.fs_verity_required);
        assert!(caps.root_transient);
        assert!(caps.etc_transient);
    }

    #[test]
    fn sysusers_lines_parse_with_id_forms() {
        let entries = parse_sysusers(
            "# comment\n\
             u dnsmasq 983 \"Dnsmasq DHCP\" /var/lib/dnsmasq -\n\
             u! locked 984 - -\n\
             g render 105 - -\n\
             u dynamic - \"no static id\"\n\
             u paired 60:61 - -\n\
             m dnsmasq render\n",
        );
        assert_eq!(entries.len(), 5);
        assert_eq!(
            (entries[0].kind, entries[0].name.as_str(), entries[0].id),
            ('u', "dnsmasq", Some(983))
        );
        assert_eq!(entries[1].id, Some(984));
        assert_eq!((entries[2].kind, entries[2].id), ('g', Some(105)));
        assert_eq!(entries[3].id, None);
        assert_eq!(entries[4].id, Some(60));
    }

    #[test]
    fn desktops_map_and_dedupe() {
        let d = desktops_from_sessions(&[
            "gnome.desktop".into(),
            "gnome-xorg.desktop".into(),
            "plasma.desktop".into(),
            "plasmawayland.desktop".into(),
            "weirdwm.desktop".into(),
        ]);
        assert_eq!(d, vec!["gnome", "kde", "weirdwm"]);
    }

    #[test]
    fn assemble_full_probe() {
        let probe = ProbeFiles {
            os_release: Some("ID=fedora\nVERSION_ID=44\n".into()),
            prepare_root: Some("[composefs]\nenabled = true\n".into()),
            sysusers: vec!["u dnsmasq 983 - -\n".into()],
            session_files: vec!["gnome.desktop".into()],
            has_systemd_boot_payload: true,
            has_bootc: true,
            has_bootupd: true,
            ..Default::default()
        };
        let caps = assemble(&probe);
        assert!(caps.composefs_capable);
        assert!(caps.ostree_capable);
        assert!(caps.systemd_boot_payload);
        assert!(caps.bootc_present);
        assert!(caps.bootupd_present);
        assert_eq!(caps.desktops, vec!["gnome"]);
        assert_eq!(caps.base.as_ref().unwrap().id, "fedora");
        assert_eq!(caps.sysusers.len(), 1);
        let json = caps.to_json();
        assert!(json.contains("\"composefs_capable\": true"));
    }

    #[test]
    fn assemble_non_ostree_image() {
        let caps = assemble(&ProbeFiles::default());
        assert!(!caps.composefs_capable);
        assert!(!caps.ostree_capable);
        assert!(caps.base.is_none());
    }

    #[test]
    fn assemble_detects_composefs_dracut_module() {
        let probe = ProbeFiles {
            dracut_module_dir_names: vec!["90bootc".into(), "37composefs".into()],
            ..Default::default()
        };
        let caps = assemble(&probe);
        assert!(caps.initramfs_has_composefs_module);

        let probe = ProbeFiles {
            dracut_module_dir_names: vec!["90bootc".into()],
            ..Default::default()
        };
        let caps = assemble(&probe);
        assert!(!caps.initramfs_has_composefs_module);
    }

    #[test]
    fn parse_bootc_install_filesystem_bare_key() {
        assert_eq!(
            parse_bootc_install_filesystem("root-fs = \"xfs\"\n"),
            Some("xfs".to_string())
        );
        assert_eq!(
            parse_bootc_install_filesystem("[install.filesystem.root]\nroot-fs = 'btrfs'\n"),
            Some("btrfs".to_string())
        );
    }

    #[test]
    fn parse_bootc_install_filesystem_missing_key_is_none() {
        assert_eq!(
            parse_bootc_install_filesystem("[install]\nkargs = [\"quiet\"]\n"),
            None
        );
        assert_eq!(parse_bootc_install_filesystem(""), None);
    }

    #[test]
    fn assemble_wires_filesystem_expectation_from_probe() {
        let probe = ProbeFiles {
            bootc_install_config: Some("root-fs = \"xfs\"\n".to_string()),
            ..Default::default()
        };
        let caps = assemble(&probe);
        assert_eq!(caps.filesystem_expectation.as_deref(), Some("xfs"));
    }

    #[test]
    fn compatibility_issues_empty_when_ostree_or_composefs_capable() {
        let probe = ProbeFiles {
            prepare_root: Some("[composefs]\nenabled = true\n".to_string()),
            ..Default::default()
        };
        let caps = assemble(&probe);
        assert!(compatibility_issues(&caps).is_empty());
        assert!(is_compatible(&caps));
    }

    #[test]
    fn compatibility_issues_flags_non_ostree_image() {
        let caps = assemble(&ProbeFiles::default());
        let issues = compatibility_issues(&caps);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("prepare-root.conf"));
        assert!(!is_compatible(&caps));
    }

    #[test]
    fn cross_base_respects_id_like() {
        let fedora = BaseInfo {
            id: "fedora".into(),
            id_like: None,
            version_id: None,
            pkg_family: None,
        };
        let dakota = BaseInfo {
            id: "dakota".into(),
            id_like: Some("fedora".into()),
            version_id: None,
            pkg_family: None,
        };
        let centos = BaseInfo {
            id: "centos".into(),
            id_like: Some("rhel".into()),
            version_id: None,
            pkg_family: None,
        };
        assert!(!is_cross_base(&fedora, &fedora));
        assert!(!is_cross_base(&fedora, &dakota));
        assert!(!is_cross_base(&dakota, &fedora));
        assert!(is_cross_base(&fedora, &centos));
        assert!(is_cross_base(&dakota, &centos));
    }

    /// The composefs route keys on family, not bare identity: Bluefin and
    /// Dakota are cross-base to `is_cross_base` (and correctly so for the
    /// OstreeDeploy remap) yet same-family here, so the MVP path keeps its
    /// same-lineage merge; Fedora and openSUSE are the pair that must not.
    /// `ID_LIKE` decides when it can; the package-manager family decides
    /// the rest (Bluefin LTS is `centos`, Dakota `ID_LIKE=org.gnome.os`).
    #[test]
    fn lineage_table() {
        use PkgFamily::*;
        let base = |id: &str, like: Option<&str>, pkg: Option<PkgFamily>| BaseInfo {
            id: id.into(),
            id_like: like.map(Into::into),
            version_id: None,
            pkg_family: pkg,
        };
        let cases = [
            (
                "fedora",
                None,
                None,
                "fedora",
                None,
                None,
                Lineage::SameBase,
            ),
            // Same ID, even with different ID_LIKE noise.
            (
                "fedora",
                None,
                None,
                "fedora",
                Some("rhel"),
                None,
                Lineage::SameBase,
            ),
            // One side's ID_LIKE names the other's ID (either direction).
            (
                "fedora",
                None,
                None,
                "dakota",
                Some("fedora"),
                None,
                Lineage::SameFamily,
            ),
            (
                "dakota",
                Some("fedora"),
                None,
                "fedora",
                None,
                None,
                Lineage::SameFamily,
            ),
            // Both derive from the same parent: the ID_LIKE lists intersect.
            (
                "bluefin",
                Some("fedora"),
                None,
                "dakota",
                Some("fedora"),
                None,
                Lineage::SameFamily,
            ),
            (
                "centos",
                Some("rhel fedora"),
                None,
                "dakota",
                Some("fedora"),
                None,
                Lineage::SameFamily,
            ),
            // ID_LIKE contradicts nothing but proves nothing: the package
            // manager decides. The real Bluefin LTS -> Dakota pair.
            (
                "centos",
                Some("rhel fedora"),
                Some(Dnf),
                "bluefin-dakota",
                Some("org.gnome.os"),
                Some(Dnf),
                Lineage::SameFamily,
            ),
            // Same archive format, different family: rpm is not a lineage.
            (
                "fedora",
                None,
                Some(Dnf),
                "opensuse-tumbleweed",
                Some("opensuse suse"),
                Some(Zypp),
                Lineage::CrossFamily,
            ),
            (
                "bluefin",
                Some("fedora"),
                Some(Dnf),
                "opensuse-tumbleweed",
                Some("opensuse suse"),
                Some(Zypp),
                Lineage::CrossFamily,
            ),
            (
                "fedora",
                None,
                Some(Dnf),
                "debian",
                None,
                Some(Apt),
                Lineage::CrossFamily,
            ),
            (
                "ubuntu",
                Some("debian"),
                Some(Apt),
                "arch",
                None,
                Some(Pacman),
                Lineage::CrossFamily,
            ),
            // ID_LIKE overlap wins over a package-manager mismatch.
            (
                "fedora",
                None,
                Some(Dnf),
                "dakota",
                Some("fedora"),
                None,
                Lineage::SameFamily,
            ),
            // No overlap and no package-manager evidence on one side:
            // neither proven nor refuted.
            (
                "fedora",
                None,
                Some(Dnf),
                "debian",
                None,
                None,
                Lineage::Unknown,
            ),
            ("fedora", None, None, "debian", None, None, Lineage::Unknown),
            (
                "centos",
                Some("rhel fedora"),
                Some(Dnf),
                "bluefin-dakota",
                Some("org.gnome.os"),
                None,
                Lineage::Unknown,
            ),
        ];
        for (h, hl, hp, t, tl, tp, want) in cases {
            assert_eq!(
                lineage(&base(h, hl, hp), &base(t, tl, tp)),
                want,
                "{h} ({hl:?}, {hp:?}) -> {t} ({tl:?}, {tp:?})"
            );
        }
    }

    /// The package-manager fingerprint is read off directory entries, so a
    /// dangling symlink (a partial registry extraction) still counts.
    #[test]
    fn pkg_family_from_root_table() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert_eq!(pkg_family_from_root(root), None);
        std::fs::create_dir_all(root.join("usr/bin")).unwrap();
        assert_eq!(pkg_family_from_root(root), None);
        std::os::unix::fs::symlink("dnf5", root.join("usr/bin/dnf")).unwrap();
        assert_eq!(pkg_family_from_root(root), Some(PkgFamily::Dnf));
        std::fs::remove_file(root.join("usr/bin/dnf")).unwrap();
        std::fs::write(root.join("usr/bin/zypper"), b"").unwrap();
        assert_eq!(pkg_family_from_root(root), Some(PkgFamily::Zypp));
    }

    #[test]
    fn assemble_attaches_pkg_family_to_base() {
        let probe = ProbeFiles {
            os_release: Some("ID=fedora\n".into()),
            pkg_family: Some(PkgFamily::Dnf),
            ..Default::default()
        };
        let caps = assemble(&probe);
        assert_eq!(caps.base.as_ref().unwrap().pkg_family, Some(PkgFamily::Dnf));
        assert!(caps.to_json().contains("\"pkg_family\": \"dnf\""));
    }
}
