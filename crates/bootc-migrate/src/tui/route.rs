//! Target route screen — composefs hosts only (#312).
//!
//! On an ostree host the wizard has one route (the OSTree → composefs
//! conversion) and this screen never appears, so that flow keeps exactly the
//! screens the `tui-migrate` E2E driver walks. On a composefs host the
//! selected target is scanned (`bootc_migrate_core::scan`), and the backends
//! the scan says the target can take decide what is shown: a choice only when
//! the target is dual-capable, otherwise a fixed line naming the one route.
//!
//! Copy here follows docs/support-matrix.md: no route is promised. Neither
//! composefs-host route has a green `main` E2E cell, so both say "unknown".

use super::*;

use bootc_migrate_core::scan::Capabilities;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Utah: Bluefin on Fedora Hummingbird. Pre-alpha; the image the
/// Dakota → Utah E2E cells (#302, `just e2e-image-swap`) target.
pub(crate) const UTAH_TESTING: &str = "ghcr.io/projectbluefin/utah:testing";

/// What the wizard knows about the selected target's capabilities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ScanState {
    /// Not scanned yet (or the selection changed since).
    NotRun,
    Running,
    Done(Box<Capabilities>),
    Failed(String),
}

/// The backends the scanned target can be deployed as, in the order they are
/// offered. composefs leads because it is what this wizard ran on composefs
/// hosts before the scan existed, so Enter alone keeps the old behaviour.
///
/// The ostree condition is the one the OstreeInstall engine enforces
/// (`ostree_install.rs`): the target must be ostree-based and ship bootupd,
/// which `bootc install` on the ostree backend requires.
pub(crate) fn viable_backends(caps: &Capabilities) -> Vec<Backend> {
    let mut out = Vec::new();
    if caps.composefs_capable {
        out.push(Backend::Composefs);
    }
    if caps.ostree_capable && caps.bootupd_present {
        out.push(Backend::Ostree);
    }
    out
}

/// What happens to the bootloader on each composefs-host route. Neither
/// engine route takes a bootloader option, so there is exactly one viable
/// loader per route and the screen shows it as a fixed line.
pub(crate) fn route_bootloader(target: Backend) -> &'static str {
    match target {
        Backend::Composefs => "this system's, unchanged (bootc switch adds the new entry)",
        Backend::Ostree => {
            "the target's, via bootupd (shim/GRUB first); the composefs entry stays as rollback"
        }
    }
}

/// One line per route, from docs/support-matrix.md's backend table. Both
/// composefs-host routes are without a green `main` cell, so the only honest
/// word is "unknown".
pub(crate) fn route_e2e_status(target: Backend) -> &'static str {
    match target {
        Backend::Composefs => {
            "unknown: no green main E2E cell for ImageSwap (docs/support-matrix.md)"
        }
        Backend::Ostree => {
            "unknown: no green main E2E cell for OstreeInstall (docs/support-matrix.md)"
        }
    }
}

/// Find the `bootc-rebase` engine: next to this binary first (a release
/// unpacked into one directory), then on `PATH`. The wizard runs the
/// OstreeInstall route through it because `bootc-migrate` has no
/// `--target-backend`.
pub(crate) fn find_bootc_rebase(exe: Option<&Path>, path: Option<&OsStr>) -> Option<PathBuf> {
    let sibling = exe
        .and_then(Path::parent)
        .map(|dir| dir.join("bootc-rebase"));
    let on_path = path
        .map(std::env::split_paths)
        .into_iter()
        .flatten()
        .map(|dir| dir.join("bootc-rebase"));
    sibling
        .into_iter()
        .chain(on_path)
        .find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Shown wherever the OstreeInstall route is selected and the engine is not
/// installed. `bootc-rebase` is not released yet (RELEASING.md), so the hint
/// is the source build.
pub(crate) const BOOTC_REBASE_MISSING: &str = "bootc-rebase not found next to this binary or on PATH. \
     Build it with `cargo build --release -p bootc-rebase` (README, \"bootc-rebase\"), \
     put it next to bootc-migrate or on PATH, then start the wizard again.";

fn yes_no(v: bool) -> (&'static str, Color) {
    if v { ("✓", SUCCESS) } else { ("✗", MUTED) }
}

pub fn render_select_route(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(Span::styled(
            format!(" Step {} · Choose Route ", app.current_step()),
            Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(TEAL))
        .style(Style::default().bg(DARK_BG));

    let label = |s: &'static str| Span::styled(format!("  {s:<12}"), Style::default().fg(MUTED));
    let mut lines: Vec<Line> = vec![
        Line::raw(""),
        Line::from(vec![
            label("Target"),
            Span::styled(app.selected_image(), Style::default().fg(TEXT)),
        ]),
    ];

    match &app.scan {
        ScanState::NotRun | ScanState::Running => {
            lines.push(Line::from(vec![
                label("Scan"),
                Span::styled(
                    format!("{} reading the target image…", app.spinner_char()),
                    Style::default().fg(AMBER),
                ),
            ]));
        }
        ScanState::Failed(err) => {
            lines.push(Line::from(vec![
                label("Scan"),
                Span::styled(format!("failed: {err}"), Style::default().fg(DANGER)),
            ]));
        }
        ScanState::Done(caps) => {
            let mut spans = vec![label("Scan")];
            for (i, (name, v)) in [
                ("composefs", caps.composefs_capable),
                ("ostree", caps.ostree_capable),
                ("bootupd", caps.bootupd_present),
                (
                    "initramfs composefs module",
                    caps.initramfs_has_composefs_module,
                ),
            ]
            .into_iter()
            .enumerate()
            {
                if i > 0 {
                    spans.push(Span::styled(" · ", Style::default().fg(MUTED)));
                }
                let (mark, fg) = yes_no(v);
                spans.push(Span::styled(format!("{name} "), Style::default().fg(TEXT)));
                spans.push(Span::styled(mark, Style::default().fg(fg)));
            }
            lines.push(Line::from(spans));
        }
    }
    lines.push(Line::raw(""));

    if matches!(app.scan, ScanState::Done(_) | ScanState::Failed(_)) {
        let viable = app.viable_backends();
        let backend_value = if viable.len() > 1 {
            // A real choice: the target is dual-capable.
            let mark = |b: Backend| {
                if app.target_backend == b {
                    "●"
                } else {
                    "○"
                }
            };
            Span::styled(
                format!(
                    "[composefs {}] [ostree {}]",
                    mark(Backend::Composefs),
                    mark(Backend::Ostree)
                ),
                Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
            )
        } else {
            let why = match (&app.scan, viable.len()) {
                (ScanState::Failed(_), _) => "scan failed; target capabilities unknown",
                (_, 0) => "the scan found no composefs or ostree+bootupd support; unknown",
                _ => "the only backend this target supports",
            };
            Span::styled(
                format!("{} ({why})", app.target_backend),
                Style::default().fg(TEXT),
            )
        };
        lines.push(Line::from(vec![label("Backend"), backend_value]));
        let strategy = if app.target_backend == Backend::Ostree {
            "OstreeInstall: fresh ostree deployment, /etc and /var carried over"
        } else {
            "ImageSwap: bootc switch to the new image"
        };
        lines.push(Line::from(vec![
            label("Route"),
            Span::styled(strategy, Style::default().fg(TEXT)),
        ]));
        lines.push(Line::from(vec![
            label("Bootloader"),
            Span::styled(
                route_bootloader(app.target_backend),
                Style::default().fg(TEXT),
            ),
        ]));
        lines.push(Line::from(vec![
            label("E2E status"),
            Span::styled(
                route_e2e_status(app.target_backend),
                Style::default().fg(AMBER),
            ),
        ]));
        if app.target_backend == Backend::Composefs
            && let ScanState::Done(caps) = &app.scan
            && caps.composefs_capable
            && !caps.initramfs_has_composefs_module
        {
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "  The target's initramfs ships no composefs dracut module: \
                 whether it boots as a composefs deployment is unknown.",
                Style::default().fg(AMBER),
            )));
        }
        if app.target_backend == Backend::Ostree && app.bootc_rebase.is_none() {
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                format!("  ✗ {BOOTC_REBASE_MISSING}"),
                Style::default().fg(DANGER),
            )));
        }
    }

    let para = Paragraph::new(Text::from(lines))
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(para, area);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(composefs: bool, ostree: bool, bootupd: bool) -> Capabilities {
        Capabilities {
            composefs_capable: composefs,
            ostree_capable: ostree,
            systemd_boot_payload: false,
            bootc_present: true,
            bootupd_present: bootupd,
            desktops: Vec::new(),
            base: None,
            sysusers: Vec::new(),
            fs_verity_required: false,
            root_transient: false,
            etc_transient: false,
            initramfs_has_composefs_module: true,
            filesystem_expectation: None,
        }
    }

    #[test]
    fn viable_backends_follow_the_scan() {
        use Backend::{Composefs, Ostree};
        // ((composefs_capable, ostree_capable, bootupd_present), offered)
        type Case = ((bool, bool, bool), &'static [Backend]);
        let cases: &[Case] = &[
            // Dual-capable (Utah: composefs enabled, ostree-based, bootupd).
            ((true, true, true), &[Composefs, Ostree]),
            // ostree-based but no bootupd: OstreeInstall refuses it.
            ((true, true, false), &[Composefs]),
            ((false, true, true), &[Ostree]),
            ((false, true, false), &[]),
            ((true, false, false), &[Composefs]),
            ((false, false, false), &[]),
        ];
        for ((c, o, b), want) in cases {
            assert_eq!(
                viable_backends(&caps(*c, *o, *b)),
                *want,
                "composefs={c} ostree={o} bootupd={b}"
            );
        }
    }

    fn touch(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn bootc_rebase_is_found_beside_the_binary_before_path() {
        let root = std::env::temp_dir().join(format!("bmc-312-find-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let bin = root.join("bin");
        let path_dir = root.join("path");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&path_dir).unwrap();
        let exe = bin.join("bootc-migrate");
        let path = std::env::join_paths([&path_dir]).unwrap();

        // Nowhere: None, not a panic.
        assert_eq!(find_bootc_rebase(Some(&exe), Some(&path)), None);
        assert_eq!(find_bootc_rebase(None, None), None);

        // Only on PATH.
        touch(&path_dir.join("bootc-rebase"), 0o755);
        assert_eq!(
            find_bootc_rebase(Some(&exe), Some(&path)),
            Some(path_dir.join("bootc-rebase"))
        );

        // A non-executable sibling is skipped.
        touch(&bin.join("bootc-rebase"), 0o644);
        assert_eq!(
            find_bootc_rebase(Some(&exe), Some(&path)),
            Some(path_dir.join("bootc-rebase"))
        );

        // An executable sibling wins over PATH.
        touch(&bin.join("bootc-rebase"), 0o755);
        assert_eq!(
            find_bootc_rebase(Some(&exe), Some(&path)),
            Some(bin.join("bootc-rebase"))
        );

        std::fs::remove_dir_all(&root).unwrap();
    }

    // ── Wizard flow ──────────────────────────────────────────────────────

    fn dual_capable(_: &str) -> Result<Capabilities> {
        Ok(caps(true, true, true))
    }

    fn composefs_only(_: &str) -> Result<Capabilities> {
        Ok(caps(true, false, false))
    }

    fn unreachable_registry(_: &str) -> Result<Capabilities> {
        anyhow::bail!("registry unreachable")
    }

    fn draw(app: &mut App) -> String {
        let backend = ratatui::backend::TestBackend::new(140, 40);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal.draw(|f| render(f, app)).expect("draw");
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    /// An app on a composefs host at the image screen, scanning with `scanner`.
    fn composefs_app(scanner: fn(&str) -> Result<Capabilities>) -> App {
        let mut app = App::new();
        app.screen = Screen::SelectImage;
        app.booted_backend = Some(Backend::Composefs);
        app.booted_image = Some(DAKOTA_STABLE.to_owned());
        app.image_choices = image_choices(app.booted_backend, Some(DAKOTA_STABLE), "Dakota");
        app.image_list_state.select(Some(0));
        app.scanner = scanner;
        app.bootc_rebase = Some(PathBuf::from("/usr/libexec/bootc-rebase"));
        app
    }

    /// Wait for the scan thread; the fakes answer at once.
    fn finish_scan(app: &mut App) {
        for _ in 0..200 {
            app.poll_scan();
            if !matches!(app.scan, ScanState::Running) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("scan did not finish");
    }

    fn key(app: &mut App, k: KeyCode) {
        app.handle_key(k, KeyModifiers::NONE);
    }

    /// The acceptance bar: no new screen, the same step count and the same
    /// six option rows on the conversion path the tui-migrate driver walks.
    #[test]
    fn ostree_host_flow_has_no_route_screen() {
        let mut app = App::new();
        app.screen = Screen::SelectImage;
        app.booted_backend = Some(Backend::Ostree);
        app.scanner = |_| panic!("an ostree host must not scan");
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.screen, Screen::ConfigureOptions);
        assert_eq!(app.option_rows(), CONVERSION_ROWS);
        let text = draw(&mut app);
        assert!(text.contains("Step 4 of 5"), "{text}");
        assert!(text.contains("Step 4 · Configure Options"), "{text}");
        assert!(text.contains("[systemd-boot ●] [grub2 ○]"), "{text}");
        key(&mut app, KeyCode::Char('n'));
        let text = draw(&mut app);
        assert!(text.contains("Step 5 of 5"), "{text}");
        assert!(text.contains("Step 5 · Review & Run"), "{text}");
        let args = app.build_command_args();
        assert!(!args.contains(&"--target-backend".to_owned()), "{args:?}");
        assert!(args.contains(&"--bootloader".to_owned()), "{args:?}");
        key(&mut app, KeyCode::Char('b'));
        key(&mut app, KeyCode::Char('b'));
        assert_eq!(app.screen, Screen::SelectImage);
    }

    #[test]
    fn utah_is_offered_on_composefs_hosts_only() {
        let cfs = image_choices(Some(Backend::Composefs), Some(DAKOTA_STABLE), "Dakota");
        let utah = cfs
            .iter()
            .find(|c| c.image == UTAH_TESTING)
            .expect("utah row");
        assert!(utah.note.contains("unknown"), "honest note: {utah:?}");
        let ostree = image_choices(Some(Backend::Ostree), None, "Bluefin");
        assert!(ostree.iter().all(|c| c.image != UTAH_TESTING));
        let on_utah = image_choices(Some(Backend::Composefs), Some(UTAH_TESTING), "Utah");
        assert_eq!(
            on_utah.iter().filter(|c| c.image == UTAH_TESTING).count(),
            1
        );
    }

    #[test]
    fn dual_capable_target_offers_a_backend_choice_and_runs_bootc_rebase() {
        let mut app = composefs_app(dual_capable);
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.screen, Screen::SelectRoute);
        finish_scan(&mut app);
        assert_eq!(
            app.target_backend,
            Backend::Composefs,
            "default keeps the swap"
        );
        let text = draw(&mut app);
        assert!(text.contains("Step 4 of 6"), "{text}");
        assert!(text.contains("[composefs ●] [ostree ○]"), "{text}");
        assert!(text.contains("ImageSwap"), "{text}");

        key(&mut app, KeyCode::Right);
        assert_eq!(app.target_backend, Backend::Ostree);
        let text = draw(&mut app);
        assert!(text.contains("[composefs ○] [ostree ●]"), "{text}");
        assert!(text.contains("OstreeInstall"), "{text}");
        assert!(text.contains("bootupd"), "fixed bootloader line: {text}");
        assert!(text.contains("unknown"), "no promise: {text}");

        key(&mut app, KeyCode::Enter);
        assert_eq!(app.screen, Screen::ConfigureOptions);
        assert_eq!(app.option_rows(), COMPOSEFS_HOST_ROWS);
        let text = draw(&mut app);
        assert!(text.contains("Step 5 of 6"), "{text}");
        assert!(!text.contains("systemd-boot ●"), "no dead radio: {text}");

        key(&mut app, KeyCode::Char('n'));
        assert_eq!(app.screen, Screen::Review);
        assert_eq!(
            app.build_command_args(),
            [
                "/usr/libexec/bootc-rebase",
                "--target-image",
                &app.selected_image(),
                "--target-backend",
                "ostree",
                "--dry-run",
            ]
        );
        let text = draw(&mut app);
        assert!(text.contains("Step 6 of 6"), "{text}");
        assert!(
            text.contains("$ /usr/libexec/bootc-rebase --target-image"),
            "exact command on the review screen: {text}"
        );

        // Back from options returns to the route screen without rescanning.
        key(&mut app, KeyCode::Char('b'));
        key(&mut app, KeyCode::Char('b'));
        assert_eq!(app.screen, Screen::SelectRoute);
        assert!(app.scan_rx.is_none(), "same image, no second scan");
    }

    #[test]
    fn single_capable_target_shows_a_fixed_line() {
        let mut app = composefs_app(composefs_only);
        key(&mut app, KeyCode::Enter);
        finish_scan(&mut app);
        key(&mut app, KeyCode::Right);
        assert_eq!(
            app.target_backend,
            Backend::Composefs,
            "nothing to switch to"
        );
        let text = draw(&mut app);
        assert!(!text.contains('●'), "no radio: {text}");
        assert!(
            text.contains("the only backend this target supports"),
            "{text}"
        );
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Char('n'));
        let args = app.build_command_args();
        assert!(!args.contains(&"--target-backend".to_owned()), "{args:?}");
    }

    #[test]
    fn failed_scan_says_unknown_and_can_be_retried() {
        let mut app = composefs_app(unreachable_registry);
        key(&mut app, KeyCode::Enter);
        finish_scan(&mut app);
        assert!(matches!(app.scan, ScanState::Failed(_)));
        assert_eq!(app.target_backend, Backend::Composefs);
        let text = draw(&mut app);
        assert!(text.contains("registry unreachable"), "{text}");
        assert!(text.contains("capabilities unknown"), "{text}");
        app.scanner = dual_capable;
        key(&mut app, KeyCode::Char('r'));
        finish_scan(&mut app);
        assert_eq!(app.viable_backends().len(), 2);
    }

    #[test]
    fn route_screen_waits_for_the_scan() {
        let mut app = composefs_app(dual_capable);
        app.screen = Screen::SelectRoute;
        app.scan = ScanState::Running;
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.screen, Screen::SelectRoute);
    }

    #[test]
    fn missing_bootc_rebase_shows_a_hint_and_does_not_run() {
        let mut app = composefs_app(dual_capable);
        app.bootc_rebase = None;
        key(&mut app, KeyCode::Enter);
        finish_scan(&mut app);
        key(&mut app, KeyCode::Right);
        let text = draw(&mut app);
        assert!(text.contains("bootc-rebase not found"), "{text}");
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Char('n'));
        assert_eq!(app.screen, Screen::Review);
        assert_eq!(app.build_command_args()[0], "bootc-rebase");
        let text = draw(&mut app);
        assert!(
            text.contains("cargo build --release -p bootc-rebase"),
            "{text}"
        );
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.screen, Screen::Review, "nothing to run");
        assert!(app.rx.is_none());
    }

    #[test]
    fn ostree_install_phases_follow_bootc_rebase_output() {
        let mut app = composefs_app(dual_capable);
        app.target_backend = Backend::Ostree;
        app.phases = ostree_install_phases();
        for (i, line) in [
            "Route: composefs -> ostree via OstreeInstall (implemented)",
            "=== Pull: ghcr.io/projectbluefin/utah:testing ===",
            "[deploy] bootc install to-existing-root",
            "=== /etc: 3-way merge into the new deployment ===",
            "=== Bootloader: restoring the composefs rollback entry ===",
        ]
        .into_iter()
        .enumerate()
        {
            app.update_phases_from_line(line);
            assert_eq!(app.phases[i].status, PhaseStatus::Running, "{line}");
            assert!(
                app.phases[..i]
                    .iter()
                    .all(|p| p.status == PhaseStatus::Done),
                "{line}"
            );
        }
    }
}
