//! Desktop-environment detection for a re-base target image and for the
//! running host (issue #68).
//!
//! [`crate::de_migrate`] owns the *mechanics* of moving a DE's per-user
//! config around a cross-DE re-base; it cannot decide on its own whether a
//! re-base is cross-DE at all. This module answers that: which desktop does
//! the image we are about to switch to ship, and which one is this system
//! running now?
//!
//! Both questions are answered from the same evidence — session `.desktop`
//! files, well-known session binaries, and a display manager's configured
//! default session — collected from a filesystem root. For the host that
//! root is `/`; for a target image it is a directory the registry streamer
//! (`crate::registry::extract_paths_into_dir`) unpacked the relevant paths
//! into, so no `podman pull` and no full image download is needed.
//!
//! Parsing is separated from I/O throughout: [`detect_desktop`] is a total
//! function over [`DesktopEvidence`] and does no filesystem or network
//! access, so the decision logic is exhaustively unit-testable.

use crate::de_migrate::{self, DesktopEnvironment};
use anyhow::{Context, Result};
use serde::Serialize;
use std::fmt;
use std::fs;
use std::path::Path;

/// What was found on a filesystem root that points at a desktop environment.
/// Deliberately raw: mapping evidence to a verdict is [`detect_desktop`]'s
/// job, so tests can construct any combination without touching a disk.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DesktopEvidence {
    /// File names (not paths) found under `/usr/share/xsessions` and
    /// `/usr/share/wayland-sessions`, e.g. `gnome.desktop`, `plasmax11.desktop`.
    pub session_files: Vec<String>,
    /// Which of [`BINARY_MARKERS`]' paths exist on the root.
    pub session_binaries: Vec<String>,
    /// The session a display manager is configured to start by default, as
    /// written in its config (e.g. SDDM's `[Autologin] Session=plasma.desktop`).
    pub default_session: Option<String>,
}

/// The verdict for one filesystem root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "desktop")]
pub enum DesktopDetection {
    /// Exactly one known desktop, or several with a display manager naming
    /// one of them as the default session.
    Single(DesktopEnvironment),
    /// Several known desktops and nothing to break the tie. Sorted and
    /// deduplicated — a combined image, not a detection failure.
    Multiple(Vec<DesktopEnvironment>),
    /// No known desktop signal at all (a headless/server image, or a desktop
    /// this module does not know yet).
    Unknown,
}

impl DesktopDetection {
    /// The single desktop this root ships, if the verdict is unambiguous.
    /// `Multiple`/`Unknown` yield `None` — callers that mutate user state
    /// must not guess.
    pub fn single(&self) -> Option<DesktopEnvironment> {
        match self {
            DesktopDetection::Single(de) => Some(*de),
            _ => None,
        }
    }
}

impl fmt::Display for DesktopDetection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DesktopDetection::Single(de) => write!(f, "{de}"),
            DesktopDetection::Multiple(des) => {
                let names: Vec<String> = des.iter().map(|d| d.to_string()).collect();
                write!(f, "multiple ({})", names.join(", "))
            }
            DesktopDetection::Unknown => f.write_str("unknown"),
        }
    }
}

/// Substrings of a session file's name (minus `.desktop`, lowercased) that
/// identify a desktop. Matched as substrings because distributions ship
/// variants of the same session: `gnome-xorg.desktop`, `plasmax11.desktop`,
/// `plasmawayland.desktop`, `xfce.desktop` vs `xfce-wayland.desktop`.
/// First match wins, so more specific markers must come first.
const SESSION_MARKERS: &[(&str, DesktopEnvironment)] = &[
    ("gnome", DesktopEnvironment::Gnome),
    ("plasma", DesktopEnvironment::Kde),
    ("kde", DesktopEnvironment::Kde),
    ("cosmic", DesktopEnvironment::Cosmic),
    ("niri", DesktopEnvironment::Niri),
    ("xfce", DesktopEnvironment::Xfce),
];

/// Root-relative paths (no leading `/`, so the same list addresses both a
/// live `/` and an unpacked image tree) whose presence identifies a desktop.
/// These are the session's own compositor/shell binary — the thing that
/// cannot be absent if the image really ships that desktop, unlike the
/// session `.desktop` file, which a stripped image may drop.
///
/// `usr/bin` only: bootc images are usr-merged, so `/bin` is a symlink into
/// it and probing both would just double the registry extraction work.
const BINARY_MARKERS: &[(&str, DesktopEnvironment)] = &[
    ("usr/bin/gnome-shell", DesktopEnvironment::Gnome),
    ("usr/bin/plasmashell", DesktopEnvironment::Kde),
    ("usr/bin/cosmic-comp", DesktopEnvironment::Cosmic),
    ("usr/bin/niri", DesktopEnvironment::Niri),
    ("usr/bin/xfce4-session", DesktopEnvironment::Xfce),
];

/// Directories holding session `.desktop` files, root-relative.
const SESSION_DIRS: &[&str] = &["usr/share/xsessions", "usr/share/wayland-sessions"];

/// Display-manager config files, root-relative. Single files only — an
/// `*.conf.d` drop-in directory is handled by [`DISPLAY_MANAGER_CONFIG_DIRS`].
const DISPLAY_MANAGER_CONFIG_FILES: &[&str] = &[
    "etc/sddm.conf",
    "etc/lightdm/lightdm.conf",
    "etc/gdm/custom.conf",
];

/// Display-manager drop-in directories, root-relative. Every `*.conf` in
/// them is parsed; later files do not override earlier ones here, because
/// the first *recognized* default-session key wins (see
/// [`collect_evidence_from_root`]) and drop-ins in practice each set it once.
const DISPLAY_MANAGER_CONFIG_DIRS: &[&str] = &[
    "etc/sddm.conf.d",
    "usr/lib/sddm/sddm.conf.d",
    "usr/share/sddm/sddm.conf.d",
    "etc/lightdm/lightdm.conf.d",
];

/// INI keys (compared lowercased) whose value names the session a display
/// manager starts by default: SDDM's `Session` (under `[Autologin]`),
/// LightDM's `user-session` (under `[Seat:*]`/`[SeatDefaults]`), and the
/// `DefaultSession` some greeters and downstream GDM configs use. The
/// section name is not matched — it varies across display managers and
/// versions, while these key names do not collide with anything else.
const DEFAULT_SESSION_KEYS: &[&str] = &["session", "user-session", "defaultsession"];

/// Every root-relative path DE detection needs out of a target image — the
/// argument for [`crate::registry::extract_paths_into_dir`].
fn evidence_paths() -> Vec<&'static str> {
    let mut paths: Vec<&'static str> = Vec::new();
    paths.extend_from_slice(SESSION_DIRS);
    paths.extend(BINARY_MARKERS.iter().map(|(p, _)| *p));
    paths.extend_from_slice(DISPLAY_MANAGER_CONFIG_FILES);
    paths.extend_from_slice(DISPLAY_MANAGER_CONFIG_DIRS);
    paths
}

/// Map a session name — a `.desktop` file name or a display manager's
/// configured session value — to a desktop. `None` for a session this
/// module does not recognize.
pub fn classify_session_name(name: &str) -> Option<DesktopEnvironment> {
    let stem = name
        .trim()
        .trim_end_matches(".desktop")
        .to_ascii_lowercase();
    if stem.is_empty() {
        return None;
    }
    SESSION_MARKERS
        .iter()
        .find(|(marker, _)| stem.contains(marker))
        .map(|(_, de)| *de)
}

/// Map a root-relative binary path to the desktop it identifies.
pub fn classify_binary_path(path: &str) -> Option<DesktopEnvironment> {
    let path = path.trim_start_matches('/');
    BINARY_MARKERS
        .iter()
        .find(|(marker, _)| *marker == path)
        .map(|(_, de)| *de)
}

/// Extract the default session a display-manager config names, if any.
///
/// Parsed with `tini` rather than line-matched, per REVIEW.md: these are INI
/// files and the value can be quoted or padded. An unparseable config is an
/// error the caller reports — silently treating it as "no default session"
/// would turn a typo in `sddm.conf` into a wrong DE verdict.
pub fn parse_display_manager_default_session(content: &str) -> Result<Option<String>> {
    let ini = tini::Ini::from_string(content)
        .map_err(|e| anyhow::anyhow!("display-manager config is not valid INI: {e}"))?;
    for (_section_name, section) in ini.iter() {
        for (key, value) in section.iter() {
            if DEFAULT_SESSION_KEYS.contains(&key.to_ascii_lowercase().as_str()) {
                let value = value.trim();
                if !value.is_empty() {
                    return Ok(Some(value.to_string()));
                }
            }
        }
    }
    Ok(None)
}

/// Decide which desktop a root ships from the evidence gathered off it.
///
/// A display manager's configured default session is authoritative when it
/// names a desktop we recognize: an image can ship several sessions while
/// booting exactly one of them, and that one is what the user's config
/// belongs to. Otherwise the union of session files and session binaries
/// decides, and more than one candidate is reported as [`Multiple`] rather
/// than resolved by guessing.
///
/// Total and side-effect free — no I/O, no failure mode.
///
/// [`Multiple`]: DesktopDetection::Multiple
pub fn detect_desktop(evidence: &DesktopEvidence) -> DesktopDetection {
    if let Some(session) = evidence.default_session.as_deref()
        && let Some(de) = classify_session_name(session)
    {
        return DesktopDetection::Single(de);
    }

    let mut candidates: Vec<DesktopEnvironment> = evidence
        .session_files
        .iter()
        .filter_map(|f| classify_session_name(f))
        .chain(
            evidence
                .session_binaries
                .iter()
                .filter_map(|b| classify_binary_path(b)),
        )
        .collect();
    candidates.sort();
    candidates.dedup();

    match candidates.len() {
        0 => DesktopDetection::Unknown,
        1 => DesktopDetection::Single(candidates[0]),
        _ => DesktopDetection::Multiple(candidates),
    }
}

/// Collect DE evidence off a filesystem root — `/` for the running host, or
/// a directory a target image's relevant paths were unpacked into. Read-only.
///
/// A missing session directory or display-manager config is expected (most
/// images ship only some of them) and contributes nothing; any other I/O
/// error propagates with the path that caused it.
pub fn collect_evidence_from_root(root: &Path) -> Result<DesktopEvidence> {
    let mut evidence = DesktopEvidence::default();

    for dir in SESSION_DIRS {
        let path = root.join(dir);
        let entries = match fs::read_dir(&path) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(e).with_context(|| format!("reading session dir {}", path.display()));
            }
        };
        for entry in entries {
            let entry = entry
                .with_context(|| format!("reading session dir entry in {}", path.display()))?;
            if let Some(name) = entry.file_name().to_str() {
                evidence.session_files.push(name.to_string());
            }
        }
    }
    evidence.session_files.sort();

    for (binary, _) in BINARY_MARKERS {
        if root.join(binary).exists() {
            evidence.session_binaries.push((*binary).to_string());
        }
    }

    for config in display_manager_config_files(root)? {
        let content = match fs::read_to_string(&config) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("reading display-manager config {}", config.display())
                });
            }
        };
        let session = parse_display_manager_default_session(&content)
            .with_context(|| format!("parsing display-manager config {}", config.display()))?;
        if session.is_some() {
            evidence.default_session = session;
            break;
        }
    }

    Ok(evidence)
}

/// Every display-manager config file that exists under `root`: the
/// well-known single files, then each `*.conf` in the drop-in directories
/// (sorted, so the same root always yields the same order).
fn display_manager_config_files(root: &Path) -> Result<Vec<std::path::PathBuf>> {
    let mut files: Vec<std::path::PathBuf> = DISPLAY_MANAGER_CONFIG_FILES
        .iter()
        .map(|f| root.join(f))
        .filter(|p| p.is_file())
        .collect();

    for dir in DISPLAY_MANAGER_CONFIG_DIRS {
        let path = root.join(dir);
        let entries = match fs::read_dir(&path) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("reading display-manager dir {}", path.display()));
            }
        };
        let mut drop_ins: Vec<std::path::PathBuf> = Vec::new();
        for entry in entries {
            let entry = entry
                .with_context(|| format!("reading display-manager dir entry {}", path.display()))?;
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some("conf") {
                drop_ins.push(p);
            }
        }
        drop_ins.sort();
        files.extend(drop_ins);
    }
    Ok(files)
}

/// Detect the desktop this system is running from its own filesystem.
pub fn detect_host_desktop() -> Result<DesktopDetection> {
    let evidence = collect_evidence_from_root(Path::new("/"))
        .context("collecting desktop evidence from this host")?;
    Ok(detect_desktop(&evidence))
}

/// Detect the desktop `image_ref` ships, streaming only the paths that carry
/// the evidence out of the registry (peak disk ≈ one layer, no `podman pull`).
pub fn detect_image_desktop(image_ref: &str) -> Result<DesktopDetection> {
    let scratch = tempfile::Builder::new()
        .prefix("bootc-migrate-de-")
        .tempdir_in(DE_EVIDENCE_SCRATCH_DIR)
        .context("failed to create scratch dir for desktop-environment detection")?;
    crate::registry::extract_paths_into_dir(image_ref, &evidence_paths(), scratch.path())
        .with_context(|| format!("streaming desktop evidence from {image_ref}"))?;
    let evidence = collect_evidence_from_root(scratch.path())
        .with_context(|| format!("collecting desktop evidence from {image_ref}"))?;
    Ok(detect_desktop(&evidence))
}

/// Same disk-backed scratch space the layer streamer itself uses — the
/// unpacked evidence is tiny, but it must not land on `/tmp`'s tmpfs
/// alongside the layer blob it is extracted from.
const DE_EVIDENCE_SCRATCH_DIR: &str = "/var/tmp";

/// Whether a re-base from `from` to `to` needs the DE stash/restore step —
/// i.e. both sides are unambiguous and they disagree. Returns the pair to
/// hand to [`crate::de_migrate::stash`]/[`crate::de_migrate::restore`].
pub fn cross_desktop_pair(
    from: &DesktopDetection,
    to: &DesktopDetection,
) -> Option<(DesktopEnvironment, DesktopEnvironment)> {
    let from = from.single()?;
    let to = to.single()?;
    (from != to).then_some((from, to))
}

/// Re-export so callers that only need the name↔enum mapping don't have to
/// reach into [`crate::de_migrate`] as well.
pub use de_migrate::parse_desktop_environment;

#[cfg(test)]
mod tests {
    use super::*;
    use DesktopEnvironment::{Cosmic, Gnome, Kde, Niri, Xfce};

    fn evidence(sessions: &[&str], binaries: &[&str], default: Option<&str>) -> DesktopEvidence {
        DesktopEvidence {
            session_files: sessions.iter().map(|s| s.to_string()).collect(),
            session_binaries: binaries.iter().map(|s| s.to_string()).collect(),
            default_session: default.map(str::to_string),
        }
    }

    #[test]
    fn classify_session_name_table() {
        let cases: &[(&str, Option<DesktopEnvironment>)] = &[
            ("gnome.desktop", Some(Gnome)),
            ("gnome-xorg.desktop", Some(Gnome)),
            ("gnome-wayland.desktop", Some(Gnome)),
            ("plasma.desktop", Some(Kde)),
            ("plasmax11.desktop", Some(Kde)),
            ("plasmawayland.desktop", Some(Kde)),
            ("cosmic.desktop", Some(Cosmic)),
            ("niri.desktop", Some(Niri)),
            ("xfce.desktop", Some(Xfce)),
            ("Xfce Session.desktop", Some(Xfce)),
            // Not a desktop this module knows — must not be guessed at.
            ("sway.desktop", None),
            ("i3.desktop", None),
            ("", None),
            (".desktop", None),
        ];
        for (input, expected) in cases {
            assert_eq!(
                classify_session_name(input),
                *expected,
                "classifying session {input:?}"
            );
        }
    }

    #[test]
    fn classify_binary_path_table() {
        let cases: &[(&str, Option<DesktopEnvironment>)] = &[
            ("usr/bin/gnome-shell", Some(Gnome)),
            ("/usr/bin/gnome-shell", Some(Gnome)),
            ("usr/bin/plasmashell", Some(Kde)),
            ("usr/bin/cosmic-comp", Some(Cosmic)),
            ("usr/bin/niri", Some(Niri)),
            ("usr/bin/xfce4-session", Some(Xfce)),
            // A prefix or suffix of a marker must not match.
            ("usr/bin/gnome-shell-extension-tool", None),
            ("usr/bin/nirid", None),
            ("usr/bin/bash", None),
        ];
        for (input, expected) in cases {
            assert_eq!(
                classify_binary_path(input),
                *expected,
                "classifying binary {input:?}"
            );
        }
    }

    #[test]
    fn detect_desktop_table() {
        let cases: &[(&str, DesktopEvidence, DesktopDetection)] = &[
            (
                "session file alone",
                evidence(&["gnome.desktop", "gnome-xorg.desktop"], &[], None),
                DesktopDetection::Single(Gnome),
            ),
            (
                "binary alone (session files stripped from the image)",
                evidence(&[], &["usr/bin/plasmashell"], None),
                DesktopDetection::Single(Kde),
            ),
            (
                "session file and binary agree",
                evidence(&["cosmic.desktop"], &["usr/bin/cosmic-comp"], None),
                DesktopDetection::Single(Cosmic),
            ),
            (
                "two desktops shipped, no default session",
                evidence(
                    &["gnome.desktop", "plasma.desktop"],
                    &["usr/bin/gnome-shell", "usr/bin/plasmashell"],
                    None,
                ),
                DesktopDetection::Multiple(vec![Gnome, Kde]),
            ),
            (
                "default session breaks the tie",
                evidence(
                    &["gnome.desktop", "plasma.desktop"],
                    &["usr/bin/gnome-shell", "usr/bin/plasmashell"],
                    Some("plasma.desktop"),
                ),
                DesktopDetection::Single(Kde),
            ),
            (
                "default session overrides a lone contradicting session file",
                evidence(&["gnome.desktop"], &[], Some("plasmax11.desktop")),
                DesktopDetection::Single(Kde),
            ),
            (
                "unrecognized default session falls back to the evidence",
                evidence(&["gnome.desktop"], &[], Some("sway.desktop")),
                DesktopDetection::Single(Gnome),
            ),
            (
                "headless image",
                evidence(&[], &[], None),
                DesktopDetection::Unknown,
            ),
            (
                "only unknown desktops",
                evidence(&["sway.desktop", "i3.desktop"], &[], None),
                DesktopDetection::Unknown,
            ),
            (
                "three desktops are reported sorted and deduplicated",
                evidence(
                    &["niri.desktop", "xfce.desktop", "niri.desktop"],
                    &["usr/bin/gnome-shell"],
                    None,
                ),
                DesktopDetection::Multiple(vec![Gnome, Niri, Xfce]),
            ),
        ];
        for (name, input, expected) in cases {
            assert_eq!(detect_desktop(input), *expected, "case: {name}");
        }
    }

    #[test]
    fn parse_display_manager_default_session_table() {
        let cases: &[(&str, &str, Option<&str>)] = &[
            (
                "sddm autologin",
                "[Autologin]\nUser=alice\nSession=plasma.desktop\n",
                Some("plasma.desktop"),
            ),
            (
                "lightdm seat",
                "[Seat:*]\ngreeter-session=slick-greeter\nuser-session=xfce\n",
                Some("xfce"),
            ),
            (
                "gdm-style DefaultSession",
                "[daemon]\nWaylandEnable=true\nDefaultSession=gnome.desktop\n",
                Some("gnome.desktop"),
            ),
            (
                "no session key at all",
                "[General]\nDisplayServer=wayland\n",
                None,
            ),
            (
                "empty value is not a session",
                "[Autologin]\nSession=\n",
                None,
            ),
            ("empty file", "", None),
        ];
        for (name, content, expected) in cases {
            assert_eq!(
                parse_display_manager_default_session(content).unwrap(),
                expected.map(str::to_string),
                "case: {name}"
            );
        }
    }

    #[test]
    fn parse_display_manager_default_session_rejects_malformed_ini() {
        // A stray section header with no closing bracket must surface as an
        // error, not silently become "no default session".
        let err = parse_display_manager_default_session("[Autologin\nSession=plasma.desktop\n")
            .unwrap_err();
        assert!(
            err.to_string().contains("not valid INI"),
            "unexpected error: {err}"
        );
    }

    fn write_file(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn collect_evidence_from_root_reads_sessions_binaries_and_default() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_file(&root.join("usr/share/xsessions/plasmax11.desktop"), "x");
        write_file(
            &root.join("usr/share/wayland-sessions/plasmawayland.desktop"),
            "x",
        );
        write_file(&root.join("usr/bin/plasmashell"), "elf");
        write_file(&root.join("usr/bin/gnome-shell"), "elf");
        write_file(
            &root.join("etc/sddm.conf.d/10-autologin.conf"),
            "[Autologin]\nSession=plasmax11.desktop\n",
        );

        let evidence = collect_evidence_from_root(root).unwrap();

        assert_eq!(
            evidence.session_files,
            vec![
                "plasmawayland.desktop".to_string(),
                "plasmax11.desktop".to_string()
            ]
        );
        assert_eq!(
            evidence.session_binaries,
            vec![
                "usr/bin/gnome-shell".to_string(),
                "usr/bin/plasmashell".to_string()
            ]
        );
        assert_eq!(
            evidence.default_session.as_deref(),
            Some("plasmax11.desktop")
        );
        // Both shells present, but the display manager names Plasma.
        assert_eq!(detect_desktop(&evidence), DesktopDetection::Single(Kde));
    }

    #[test]
    fn collect_evidence_from_empty_root_is_unknown_not_error() {
        let tmp = tempfile::tempdir().unwrap();
        let evidence = collect_evidence_from_root(tmp.path()).unwrap();
        assert_eq!(evidence, DesktopEvidence::default());
        assert_eq!(detect_desktop(&evidence), DesktopDetection::Unknown);
    }

    #[test]
    fn evidence_paths_cover_every_marker() {
        let paths = evidence_paths();
        for (binary, _) in BINARY_MARKERS {
            assert!(
                paths.contains(binary),
                "{binary} must be streamed out of the image"
            );
        }
        for dir in SESSION_DIRS {
            assert!(paths.contains(dir), "{dir} must be streamed");
        }
        assert!(
            paths.iter().all(|p| !p.starts_with('/')),
            "registry paths are archive-relative: {paths:?}"
        );
    }

    #[test]
    fn cross_desktop_pair_table() {
        /// (case name, host verdict, target verdict, expected stash pair)
        type Case = (
            &'static str,
            DesktopDetection,
            DesktopDetection,
            Option<(DesktopEnvironment, DesktopEnvironment)>,
        );
        let cases: &[Case] = &[
            (
                "gnome to kde is cross-DE",
                DesktopDetection::Single(Gnome),
                DesktopDetection::Single(Kde),
                Some((Gnome, Kde)),
            ),
            (
                "same desktop is not",
                DesktopDetection::Single(Gnome),
                DesktopDetection::Single(Gnome),
                None,
            ),
            (
                "ambiguous target must not be guessed at",
                DesktopDetection::Single(Gnome),
                DesktopDetection::Multiple(vec![Gnome, Kde]),
                None,
            ),
            (
                "unknown host means no stash",
                DesktopDetection::Unknown,
                DesktopDetection::Single(Kde),
                None,
            ),
        ];
        for (name, from, to, expected) in cases {
            assert_eq!(cross_desktop_pair(from, to), *expected, "case: {name}");
        }
    }

    #[test]
    fn detection_renders_for_humans() {
        assert_eq!(DesktopDetection::Single(Kde).to_string(), "kde");
        assert_eq!(
            DesktopDetection::Multiple(vec![Gnome, Kde]).to_string(),
            "multiple (gnome, kde)"
        );
        assert_eq!(DesktopDetection::Unknown.to_string(), "unknown");
    }
}
