//! Interactive TUI wizard for bootc-migrate.
//!
//! Entry point: [`run_tui`].  Invoke as `sudo bootc-migrate tui`
//! or automatically when `--target-image` is omitted.

use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Wrap,
    },
};
use std::{
    fmt,
    io::{BufRead, BufReader},
    process::Stdio,
    sync::mpsc,
    time::{Duration, Instant},
};
use tui_big_text::{BigText, PixelSize};

mod preflight;
mod welcome;

use self::preflight::{PreflightTuiState, render_preflight};
use self::welcome::render_welcome;

use self::complete::render_complete;
use self::failed::render_failed;
use self::review::render_review;
use self::running::render_running;

use self::configure_options::render_configure_options;
use self::select_image::render_select_image;

use bootc_migrate_core::rebase_plan::Backend;

mod configure_options;
mod select_image;

mod complete;
mod failed;
mod review;
mod running;

// ─── Colour palette ───────────────────────────────────────────────────────────
//
// Every foreground here clears WCAG AA (4.5:1) against BOTH backgrounds, and
// `contrast_tests` below asserts it so a future tweak cannot quietly undo it.
// The exception is `BORDER`, which only ever draws chrome and is held to the
// 3:1 non-text minimum instead.
//
// The distinction that was missing before is `MUTED` vs `BORDER`. A single
// "subtle" colour was doing both jobs, and because a border may be dim while
// text may not, it ended up set for the border and dragged the text down with
// it: 2.7:1 for the values, unselected rows and hints — below even the
// large-text floor, which is what made the panels unreadable on a dark
// terminal.
const TEAL: Color = Color::Rgb(0, 190, 190);
const AMBER: Color = Color::Rgb(230, 170, 20);
const DARK_BG: Color = Color::Rgb(18, 20, 24);
const SURFACE: Color = Color::Rgb(30, 34, 42);
/// De-emphasised **text**: values, unselected rows, hints. Readable (>=4.5:1).
const MUTED: Color = Color::Rgb(140, 149, 162);
/// **Chrome only** — inactive borders and separators. Never put text in this.
const BORDER: Color = Color::Rgb(100, 110, 124);
/// The unfilled portion of a gauge, so a bar reads as a bar at 0%.
const TRACK: Color = Color::Rgb(58, 64, 78);
const SUCCESS: Color = Color::Rgb(90, 210, 110);
/// Red **text** on a dark panel.
const DANGER: Color = Color::Rgb(235, 105, 105);
/// Red **fill** behind light text (the error banner). Separate from `DANGER`
/// for the same reason `BORDER` is separate from `MUTED`: a red bright enough
/// to read as text on a dark panel is too bright to sit behind white.
const DANGER_BG: Color = Color::Rgb(160, 32, 32);
const TEXT: Color = Color::Rgb(222, 226, 234);

// ─── Spinner ──────────────────────────────────────────────────────────────────
const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

// ─── Target image choices ─────────────────────────────────────────────────────

/// The composefs-backed image this tool migrates to.
const DAKOTA_STABLE: &str = "ghcr.io/projectbluefin/dakota:stable";

/// One row on the target-image screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImageChoice {
    /// What this target is.
    pub label: String,
    /// The image reference, or empty for the custom row.
    pub image: String,
    /// Why it is being offered — the detected fact that produced it.
    pub note: String,
    /// Whether this row takes a typed reference.
    pub custom: bool,
}

/// Build the target list from what preflight actually found.
///
/// This replaced three hard-coded rows — "Dakota stable (default)", "(from
/// LTS/XFS)" and "(from Aurora)" — which all pointed at the *same* image and
/// differed only in a hint matched against the detected OS. The source was
/// already detected, so the choice was asking the user to identify a system
/// the tool had identified, and every answer led to the same place.
///
/// Now the detected facts produce the list, and the first row is the answer:
/// on ostree there is one target, and on composefs the useful default is the
/// image already booted, so a swap starts from the user's own reference
/// instead of a blank field.
pub(crate) fn image_choices(
    backend: Option<Backend>,
    booted_image: Option<&str>,
    detected_os: &str,
) -> Vec<ImageChoice> {
    let mut choices = Vec::new();

    match backend {
        // Already composefs: this is an image swap, so the image in hand is
        // the most useful starting point — usually the user wants a different
        // tag of what they are running, which is one edit away rather than a
        // reference typed from memory.
        Some(Backend::Composefs) => {
            if let Some(current) = booted_image {
                choices.push(ImageChoice {
                    label: "Current image (edit the tag to swap)".to_owned(),
                    image: current.to_owned(),
                    note: "currently booted".to_owned(),
                    custom: false,
                });
            }
            if booted_image != Some(DAKOTA_STABLE) {
                choices.push(ImageChoice {
                    label: "Dakota stable".to_owned(),
                    image: DAKOTA_STABLE.to_owned(),
                    note: "the default".to_owned(),
                    custom: false,
                });
            }
        }
        // ostree, or preflight could not tell: the conversion has exactly one
        // target today, so say so and name what it was matched against rather
        // than offering the same image three times.
        _ => {
            // The detected source is named in the header, so this only has to
            // say why the row is here.
            let note = if booted_image.is_some() || detected_os != "Unknown OS" {
                "recommended for this system".to_owned()
            } else {
                "the composefs-backed default".to_owned()
            };
            choices.push(ImageChoice {
                label: "Dakota stable".to_owned(),
                image: DAKOTA_STABLE.to_owned(),
                note,
                custom: false,
            });
        }
    }

    choices.push(ImageChoice {
        label: "Custom…".to_owned(),
        image: String::new(),
        note: "any bootc image".to_owned(),
        custom: true,
    });
    choices
}

// ─── Source OS detection ──────────────────────────────────────────────────────

/// Detect the currently running OS from /etc/os-release.
fn detect_source_os() -> String {
    let os_release = std::fs::read_to_string("/etc/os-release")
        .or_else(|_| std::fs::read_to_string("/usr/lib/os-release"))
        .unwrap_or_default();
    let mut name = String::new();
    let mut variant = String::new();
    for line in os_release.lines() {
        if let Some(val) = line.strip_prefix("NAME=") {
            name = val.trim_matches('"').to_string();
        } else if let Some(val) = line.strip_prefix("VARIANT_ID=") {
            variant = val.trim_matches('"').to_string();
        }
    }
    if name.is_empty() {
        return "Unknown OS".to_string();
    }
    if variant.is_empty() {
        name
    } else {
        format!("{} ({})", name, variant)
    }
}

// ─── Phase information ────────────────────────────────────────────────────────
#[derive(Debug, Clone, PartialEq, Eq)]
enum PhaseStatus {
    Pending,
    Running,
    Done,
    Failed,
    Skipped,
}

#[derive(Debug, Clone)]
struct PhaseInfo {
    label: &'static str,
    status: PhaseStatus,
}

fn default_phases() -> Vec<PhaseInfo> {
    vec![
        PhaseInfo {
            label: "Phase 0 · Preflight",
            status: PhaseStatus::Pending,
        },
        PhaseInfo {
            label: "Phase 1 · OSTree import",
            status: PhaseStatus::Pending,
        },
        PhaseInfo {
            label: "Phase 2 · OCI pull",
            status: PhaseStatus::Pending,
        },
        PhaseInfo {
            label: "Phase 3 · EROFS seal",
            status: PhaseStatus::Pending,
        },
        PhaseInfo {
            label: "Phase 4 · Stage deployment",
            status: PhaseStatus::Pending,
        },
        PhaseInfo {
            label: "Phase 5 · Bootloader",
            status: PhaseStatus::Pending,
        },
    ]
}

// ─── Log line colours ─────────────────────────────────────────────────────────
#[derive(Debug, Clone)]
enum LogKind {
    Header,
    Phase,
    Error,
    Success,
    Normal,
}

#[derive(Debug, Clone)]
struct LogLine {
    text: String,
    kind: LogKind,
}

impl LogLine {
    fn classify(raw: &str) -> Self {
        let kind = if raw.starts_with("===") {
            LogKind::Header
        } else if raw.starts_with("[phase")
            || raw.starts_with("[phase2]")
            || raw.starts_with("[phase4]")
            || raw.starts_with("[phase5]")
        {
            LogKind::Phase
        } else if raw.to_lowercase().contains("error")
            || raw.to_lowercase().contains("failed")
            || raw.to_lowercase().contains("fatal")
        {
            LogKind::Error
        } else if raw.contains("✓")
            || raw.contains("COMPLETED")
            || raw.contains("success")
            || raw.starts_with("Reclaimed")
        {
            LogKind::Success
        } else {
            LogKind::Normal
        };
        Self {
            text: raw.to_owned(),
            kind,
        }
    }
}

// ─── Migration process messages ───────────────────────────────────────────────
#[derive(Debug)]
enum MigMsg {
    Line(String),
    Done(bool), // true = success
}

// ─── Wizard screen ────────────────────────────────────────────────────────────
#[derive(Debug, Clone, PartialEq, Eq)]
enum Screen {
    Welcome,
    Preflight,
    SelectImage,
    ConfigureOptions,
    Review,
    Running,
    Complete,
    Failed,
}

// ─── Bootloader choice ────────────────────────────────────────────────────────
#[derive(Debug, Clone, PartialEq, Eq)]
enum Bootloader {
    SystemdBoot,
    Grub2,
}

// ─── App state ────────────────────────────────────────────────────────────────
/// Top-level application state for the TUI wizard.
pub struct App {
    screen: Screen,

    // Preflight
    preflight_state: Option<PreflightTuiState>,
    detected_os: String,
    /// Target rows, rebuilt from the preflight report so the list reflects
    /// this system rather than a fixed menu.
    image_choices: Vec<ImageChoice>,
    /// What preflight found booted, shown in the header so each row's note can
    /// stay short enough not to be truncated.
    booted_backend: Option<Backend>,
    booted_image: Option<String>,

    // SelectImage
    image_list_state: ListState,
    custom_image: String,
    custom_image_editing: bool,

    // ConfigureOptions
    opt_dry_run: bool,
    opt_skip_import: bool,
    opt_bootloader: Bootloader,
    opt_skip_preflight: bool,
    opt_force: bool,
    options_cursor: usize,

    // Running
    phases: Vec<PhaseInfo>,
    log_lines: Vec<LogLine>,
    log_scroll: usize,
    spinner_tick: usize,
    last_tick: Instant,
    rx: Option<mpsc::Receiver<MigMsg>>,
    migration_done: bool,
    migration_success: bool,

    // Quit confirmation dialog
    show_quit_dialog: bool,
    quit_dialog_yes: bool,

    // Terminal size for scrollbar
    term_height: u16,
}

impl fmt::Debug for App {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("App")
            .field("screen", &self.screen)
            .field("opt_dry_run", &self.opt_dry_run)
            .finish_non_exhaustive()
    }
}

impl App {
    fn new() -> Self {
        let mut image_list_state = ListState::default();
        image_list_state.select(Some(0));
        let detected_os = detect_source_os();
        // Preflight has not run yet; this is replaced the moment it does.
        let image_choices = image_choices(None, None, &detected_os);
        Self {
            screen: Screen::Welcome,
            preflight_state: None,
            detected_os,
            image_choices,
            booted_backend: None,
            booted_image: None,
            image_list_state,
            custom_image: String::new(),
            custom_image_editing: false,
            opt_dry_run: true,
            opt_skip_import: false,
            opt_bootloader: Bootloader::SystemdBoot,
            opt_skip_preflight: false,
            opt_force: false,
            options_cursor: 0,
            phases: default_phases(),
            log_lines: Vec::new(),
            log_scroll: 0,
            spinner_tick: 0,
            last_tick: Instant::now(),
            rx: None,
            migration_done: false,
            migration_success: false,
            show_quit_dialog: false,
            quit_dialog_yes: false,
            term_height: 40,
        }
    }

    fn selected_image(&self) -> String {
        let idx = self.image_list_state.selected().unwrap_or(0);
        match self.image_choices.get(idx) {
            Some(c) if c.custom => self.custom_image.clone(),
            Some(c) => c.image.clone(),
            None => String::new(),
        }
    }

    fn is_custom_selected(&self) -> bool {
        let idx = self.image_list_state.selected().unwrap_or(0);
        self.image_choices.get(idx).is_some_and(|c| c.custom)
    }

    /// Rebuild the target list from a fresh preflight report, keeping the
    /// cursor on the first row — the one the detection picked.
    fn refresh_image_choices(&mut self, report: &bootc_migrate_core::preflight::PreflightReport) {
        self.booted_backend = report.booted_backend;
        self.booted_image = report.booted_image.clone();
        self.image_choices = image_choices(
            report.booted_backend,
            report.booted_image.as_deref(),
            &self.detected_os,
        );
        self.image_list_state.select(Some(0));
    }

    fn build_command_args(&self) -> Vec<String> {
        let mut args: Vec<String> = Vec::new();
        let exe =
            std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("bootc-migrate"));
        args.push(exe.display().to_string());
        args.push("--target-image".to_owned());
        args.push(self.selected_image());
        if self.opt_dry_run {
            args.push("--dry-run".to_owned());
        }
        if self.opt_skip_import {
            args.push("--skip-import".to_owned());
        }
        match self.opt_bootloader {
            Bootloader::SystemdBoot => {
                args.push("--bootloader".to_owned());
                args.push("systemd-boot".to_owned());
            }
            Bootloader::Grub2 => {
                args.push("--bootloader".to_owned());
                args.push("grub2".to_owned());
            }
        }
        if self.opt_skip_preflight {
            args.push("--skip-preflight".to_owned());
        }
        if self.opt_force {
            args.push("--force".to_owned());
        }
        args
    }

    fn command_display(&self) -> String {
        self.build_command_args().join(" ")
    }

    /// Spawn the migration binary in a background thread, piping stdout/stderr
    /// through an mpsc channel as [`MigMsg`] values.
    fn start_migration(&mut self) {
        let args = self.build_command_args();
        // args[0] is the executable path; args[1..] are the arguments.
        let exe = args[0].clone();
        let rest: Vec<String> = args[1..].to_vec();

        let (tx, rx) = mpsc::channel::<MigMsg>();
        self.rx = Some(rx);
        self.phases = default_phases();
        self.log_lines.clear();
        self.log_scroll = 0;
        self.migration_done = false;
        self.migration_success = false;

        std::thread::spawn(move || {
            let result = std::process::Command::new(&exe)
                .args(&rest)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn();

            let mut child = match result {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(MigMsg::Line(format!("ERROR: failed to spawn: {e}")));
                    let _ = tx.send(MigMsg::Done(false));
                    return;
                }
            };

            // Merge stdout + stderr by reading stdout first (common pattern),
            // then stderr.  We use two threads to avoid deadlock.
            let stdout = child.stdout.take().expect("stdout piped");
            let stderr = child.stderr.take().expect("stderr piped");

            let tx2 = tx.clone();
            let stdout_thread = std::thread::spawn(move || {
                let reader = BufReader::new(stdout);
                for line in reader.lines().map_while(Result::ok) {
                    if tx2.send(MigMsg::Line(line)).is_err() {
                        break;
                    }
                }
            });

            let tx3 = tx.clone();
            let stderr_thread = std::thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines().map_while(Result::ok) {
                    if tx3.send(MigMsg::Line(line)).is_err() {
                        break;
                    }
                }
            });

            let _ = stdout_thread.join();
            let _ = stderr_thread.join();

            let success = child.wait().map(|s| s.success()).unwrap_or(false);
            let _ = tx.send(MigMsg::Done(success));
        });
    }

    /// Parse a log line to update phase statuses.
    fn update_phases_from_line(&mut self, line: &str) {
        if line.contains("Phase 0") || line.contains("=== Phase 0") {
            self.set_phase_running(0);
        } else if line.contains("=== Phase 1: Skipped") {
            self.set_phase_done(0);
            self.set_phase_skipped(1);
        } else if line.contains("=== Phase 1") {
            self.set_phase_done(0);
            self.set_phase_running(1);
        } else if line.contains("[phase2]") {
            self.set_phase_done(1);
            self.set_phase_running(2);
        } else if line.contains("=== Phase 3") {
            self.set_phase_done(2);
            self.set_phase_running(3);
        } else if line.contains("=== Phase 4") || line.contains("[phase4]") {
            self.set_phase_done(3);
            self.set_phase_running(4);
        } else if line.contains("[phase5]") {
            self.set_phase_done(4);
            self.set_phase_running(5);
        } else if line.contains("=== MIGRATION COMPLETED") {
            for p in &mut self.phases {
                if p.status == PhaseStatus::Running {
                    p.status = PhaseStatus::Done;
                }
                if p.status == PhaseStatus::Pending {
                    p.status = PhaseStatus::Done;
                }
            }
        }
    }

    fn set_phase_running(&mut self, idx: usize) {
        for (i, p) in self.phases.iter_mut().enumerate() {
            if i < idx && p.status == PhaseStatus::Pending {
                p.status = PhaseStatus::Done;
            }
            if i < idx && p.status == PhaseStatus::Running {
                p.status = PhaseStatus::Done;
            }
        }
        if let Some(p) = self.phases.get_mut(idx)
            && p.status == PhaseStatus::Pending
        {
            p.status = PhaseStatus::Running;
        }
    }

    fn set_phase_done(&mut self, idx: usize) {
        if let Some(p) = self.phases.get_mut(idx)
            && (p.status == PhaseStatus::Running || p.status == PhaseStatus::Pending)
        {
            p.status = PhaseStatus::Done;
        }
    }

    fn set_phase_skipped(&mut self, idx: usize) {
        if let Some(p) = self.phases.get_mut(idx) {
            p.status = PhaseStatus::Skipped;
        }
    }

    fn mark_phases_failed(&mut self) {
        for p in &mut self.phases {
            if p.status == PhaseStatus::Running {
                p.status = PhaseStatus::Failed;
            }
        }
    }

    /// Drain available messages from the migration channel without blocking.
    fn drain_migration_channel(&mut self) {
        let msgs: Vec<MigMsg> = {
            if let Some(ref rx) = self.rx {
                let mut v = Vec::new();
                while let Ok(m) = rx.try_recv() {
                    v.push(m);
                }
                v
            } else {
                Vec::new()
            }
        };

        for msg in msgs {
            match msg {
                MigMsg::Line(line) => {
                    self.update_phases_from_line(&line);
                    self.log_lines.push(LogLine::classify(&line));
                    // Auto-scroll to bottom
                    if !self.log_lines.is_empty() {
                        self.log_scroll = self.log_lines.len().saturating_sub(1);
                    }
                }
                MigMsg::Done(success) => {
                    self.migration_done = true;
                    self.migration_success = success;
                    if success {
                        for p in &mut self.phases {
                            if p.status == PhaseStatus::Running || p.status == PhaseStatus::Pending
                            {
                                p.status = PhaseStatus::Done;
                            }
                        }
                    } else {
                        self.mark_phases_failed();
                    }
                }
            }
        }
    }

    fn advance_spinner(&mut self) {
        if self.last_tick.elapsed() >= Duration::from_millis(80) {
            self.spinner_tick = (self.spinner_tick + 1) % SPINNER.len();
            self.last_tick = Instant::now();
        }
    }

    fn spinner_char(&self) -> char {
        SPINNER[self.spinner_tick]
    }

    // ── Navigation helpers ────────────────────────────────────────────────────

    fn next_screen(&mut self) {
        self.screen = match self.screen {
            Screen::Welcome => {
                // Run preflight checks and show results.
                match bootc_migrate_core::preflight::run_preflight_checks() {
                    Ok(report) => {
                        self.preflight_state = Some(PreflightTuiState::from_report(&report));
                        self.refresh_image_choices(&report);
                    }
                    Err(_) => {
                        // If preflight fails (e.g. not root), show a minimal state.
                        self.preflight_state = None;
                    }
                }
                Screen::Preflight
            }
            Screen::Preflight => Screen::SelectImage,
            Screen::SelectImage => Screen::ConfigureOptions,
            Screen::ConfigureOptions => Screen::Review,
            Screen::Review => {
                self.start_migration();
                Screen::Running
            }
            Screen::Running => {
                if self.migration_success {
                    Screen::Complete
                } else {
                    Screen::Failed
                }
            }
            Screen::Complete | Screen::Failed => Screen::Welcome,
        };
    }

    fn prev_screen(&mut self) {
        self.screen = match self.screen {
            Screen::Welcome => Screen::Welcome,
            Screen::Preflight => Screen::Welcome,
            Screen::SelectImage => Screen::Preflight,
            Screen::ConfigureOptions => Screen::SelectImage,
            Screen::Review => Screen::ConfigureOptions,
            Screen::Running | Screen::Complete | Screen::Failed => Screen::Running,
        };
    }

    fn total_wizard_steps() -> usize {
        5 // Welcome, Preflight, Image, Options, Review
    }

    fn current_step(&self) -> usize {
        match self.screen {
            Screen::Welcome => 1,
            Screen::Preflight => 2,
            Screen::SelectImage => 3,
            Screen::ConfigureOptions => 4,
            Screen::Review => 5,
            Screen::Running | Screen::Complete | Screen::Failed => 5,
        }
    }

    // ── Key handling per screen ───────────────────────────────────────────────

    fn handle_key(&mut self, key: KeyCode, modifiers: KeyModifiers) -> bool {
        // Quit dialog overrides everything
        if self.show_quit_dialog {
            return self.handle_quit_dialog_key(key);
        }

        // Ctrl-C / Ctrl-Q always opens quit confirmation
        if modifiers.contains(KeyModifiers::CONTROL)
            && (key == KeyCode::Char('c') || key == KeyCode::Char('q'))
        {
            self.show_quit_dialog = true;
            return false;
        }

        match &self.screen {
            Screen::Welcome => self.handle_welcome_key(key),
            Screen::Preflight => self.handle_preflight_key(key),
            Screen::SelectImage => self.handle_select_image_key(key),
            Screen::ConfigureOptions => self.handle_options_key(key),
            Screen::Review => self.handle_review_key(key),
            Screen::Running => self.handle_running_key(key),
            Screen::Complete | Screen::Failed => self.handle_end_key(key),
        }
    }

    fn handle_quit_dialog_key(&mut self, key: KeyCode) -> bool {
        match key {
            KeyCode::Left | KeyCode::Char('h') => {
                self.quit_dialog_yes = true;
                false
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.quit_dialog_yes = false;
                false
            }
            KeyCode::Enter => {
                if self.quit_dialog_yes {
                    true // signal exit
                } else {
                    self.show_quit_dialog = false;
                    false
                }
            }
            KeyCode::Esc => {
                self.show_quit_dialog = false;
                false
            }
            _ => false,
        }
    }

    fn handle_welcome_key(&mut self, key: KeyCode) -> bool {
        match key {
            KeyCode::Enter | KeyCode::Char('n') => self.next_screen(),
            KeyCode::Char('q') | KeyCode::Esc => self.show_quit_dialog = true,
            _ => {}
        }
        false
    }

    fn handle_preflight_key(&mut self, key: KeyCode) -> bool {
        match key {
            KeyCode::Enter | KeyCode::Char('n') => self.next_screen(),
            KeyCode::Backspace | KeyCode::Esc | KeyCode::Char('b') => self.prev_screen(),
            KeyCode::Char('q') => self.show_quit_dialog = true,
            KeyCode::Char('r') => {
                // Re-run preflight
                if let Ok(report) = bootc_migrate_core::preflight::run_preflight_checks() {
                    self.preflight_state = Some(PreflightTuiState::from_report(&report));
                    self.refresh_image_choices(&report);
                }
            }
            _ => {}
        }
        false
    }

    fn handle_select_image_key(&mut self, key: KeyCode) -> bool {
        if self.custom_image_editing {
            match key {
                KeyCode::Enter | KeyCode::Esc => {
                    self.custom_image_editing = false;
                }
                KeyCode::Backspace => {
                    self.custom_image.pop();
                }
                KeyCode::Char(c) => {
                    self.custom_image.push(c);
                }
                _ => {}
            }
            return false;
        }

        match key {
            KeyCode::Up | KeyCode::Char('k') => {
                let cur = self.image_list_state.selected().unwrap_or(0);
                if cur > 0 {
                    self.image_list_state.select(Some(cur - 1));
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let cur = self.image_list_state.selected().unwrap_or(0);
                if cur + 1 < self.image_choices.len() {
                    self.image_list_state.select(Some(cur + 1));
                }
            }
            KeyCode::Enter => {
                if self.is_custom_selected() && self.custom_image.is_empty() {
                    self.custom_image_editing = true;
                } else {
                    self.next_screen();
                }
            }
            KeyCode::Tab => {
                if self.is_custom_selected() {
                    self.custom_image_editing = true;
                }
            }
            KeyCode::Char('e') => {
                if self.is_custom_selected() {
                    self.custom_image_editing = true;
                }
            }
            KeyCode::Backspace | KeyCode::Esc | KeyCode::Char('b') => self.prev_screen(),
            KeyCode::Char('q') => self.show_quit_dialog = true,
            _ => {}
        }
        false
    }

    fn handle_options_key(&mut self, key: KeyCode) -> bool {
        // 5 options: dry_run(0), skip_import(1), bootloader(2), skip_preflight(3), force(4)
        const NUM_OPTIONS: usize = 5;
        match key {
            KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => {
                if self.options_cursor > 0 {
                    self.options_cursor -= 1;
                }
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                if self.options_cursor < NUM_OPTIONS - 1 {
                    self.options_cursor += 1;
                }
            }
            KeyCode::Char(' ') | KeyCode::Enter => {
                self.toggle_option(self.options_cursor);
                if key == KeyCode::Enter && self.options_cursor == NUM_OPTIONS - 1 {
                    self.next_screen();
                }
            }
            KeyCode::Right | KeyCode::Char('l') => {
                if self.options_cursor == 2 {
                    self.opt_bootloader = Bootloader::Grub2;
                }
            }
            KeyCode::Left | KeyCode::Char('h') => {
                if self.options_cursor == 2 {
                    self.opt_bootloader = Bootloader::SystemdBoot;
                }
            }
            KeyCode::Char('n') => self.next_screen(),
            KeyCode::Backspace | KeyCode::Esc | KeyCode::Char('b') => self.prev_screen(),
            KeyCode::Char('q') => self.show_quit_dialog = true,
            _ => {}
        }
        false
    }

    fn toggle_option(&mut self, idx: usize) {
        match idx {
            0 => self.opt_dry_run = !self.opt_dry_run,
            1 => self.opt_skip_import = !self.opt_skip_import,
            2 => {
                self.opt_bootloader = match self.opt_bootloader {
                    Bootloader::SystemdBoot => Bootloader::Grub2,
                    Bootloader::Grub2 => Bootloader::SystemdBoot,
                };
            }
            3 => self.opt_skip_preflight = !self.opt_skip_preflight,
            4 => self.opt_force = !self.opt_force,
            _ => {}
        }
    }

    fn handle_review_key(&mut self, key: KeyCode) -> bool {
        match key {
            KeyCode::Enter | KeyCode::Char('r') => self.next_screen(),
            KeyCode::Backspace | KeyCode::Esc | KeyCode::Char('b') => self.prev_screen(),
            KeyCode::Char('q') => self.show_quit_dialog = true,
            _ => {}
        }
        false
    }

    fn handle_running_key(&mut self, key: KeyCode) -> bool {
        match key {
            KeyCode::Up | KeyCode::Char('k') => {
                if self.log_scroll > 0 {
                    self.log_scroll -= 1;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.log_scroll = self
                    .log_scroll
                    .saturating_add(1)
                    .min(self.log_lines.len().saturating_sub(1));
            }
            KeyCode::PageUp => {
                self.log_scroll = self.log_scroll.saturating_sub(10);
            }
            KeyCode::PageDown => {
                self.log_scroll = self
                    .log_scroll
                    .saturating_add(10)
                    .min(self.log_lines.len().saturating_sub(1));
            }
            KeyCode::Enter => {
                if self.migration_done {
                    self.next_screen();
                }
            }
            KeyCode::Char('q') | KeyCode::Esc => {
                self.show_quit_dialog = true;
            }
            _ => {}
        }
        false
    }

    fn handle_end_key(&mut self, key: KeyCode) -> bool {
        match key {
            KeyCode::Char('q') | KeyCode::Enter | KeyCode::Esc => {
                return true;
            }
            _ => {}
        }
        false
    }
}

// ─── Rendering ────────────────────────────────────────────────────────────────

fn render(f: &mut ratatui::Frame, app: &mut App) {
    let area = f.area();
    app.term_height = area.height;

    // Base background
    f.render_widget(Block::default().style(Style::default().bg(DARK_BG)), area);

    // Vertical split: title bar / content / status bar
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(3),
        ])
        .split(area);

    render_title(f, app, chunks[0]);
    render_screen(f, app, chunks[1]);
    render_statusbar(f, app, chunks[2]);

    if app.show_quit_dialog {
        render_quit_dialog(f, area);
    }
}

fn render_title(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let is_dry = app.opt_dry_run && app.screen != Screen::Welcome;
    let mode_tag = if is_dry {
        Span::styled(
            " ⚠ DRY-RUN ",
            Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
        )
    } else if app.screen == Screen::Welcome {
        Span::raw("")
    } else {
        Span::styled(
            " ⚠ LIVE MIGRATION ",
            Style::default().fg(DANGER).add_modifier(Modifier::BOLD),
        )
    };

    let step_str = match app.screen {
        Screen::Welcome
        | Screen::Preflight
        | Screen::SelectImage
        | Screen::ConfigureOptions
        | Screen::Review => {
            format!(
                "  Step {} of {}",
                app.current_step(),
                App::total_wizard_steps()
            )
        }
        Screen::Running => "  Migration running…".to_owned(),
        Screen::Complete => "  Migration complete".to_owned(),
        Screen::Failed => "  Migration failed".to_owned(),
    };

    let title_line = Line::from(vec![
        Span::styled(
            " 🚀 bootc-migrate",
            Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
        ),
        Span::styled(step_str, Style::default().fg(MUTED)),
        Span::raw("  "),
        mode_tag,
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(TEAL))
        .style(Style::default().bg(SURFACE));

    let para = Paragraph::new(title_line)
        .block(block)
        .alignment(Alignment::Left);
    f.render_widget(para, area);
}

fn render_statusbar(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let hints: &[(&str, &str)] = match app.screen {
        Screen::Welcome => &[("Enter", "Next"), ("q", "Quit")],
        Screen::Preflight => &[
            ("Enter", "Continue"),
            ("r", "Re-check"),
            ("b", "Back"),
            ("q", "Quit"),
        ],
        Screen::SelectImage => &[
            ("↑↓", "Move"),
            ("Enter", "Select / Next"),
            ("e / Tab", "Edit custom"),
            ("b", "Back"),
            ("q", "Quit"),
        ],
        Screen::ConfigureOptions => &[
            ("↑↓", "Move"),
            ("Space", "Toggle"),
            ("←→", "Bootloader"),
            ("n", "Next"),
            ("b", "Back"),
            ("q", "Quit"),
        ],
        Screen::Review => &[("Enter / r", "RUN"), ("b", "Back"), ("q", "Quit")],
        Screen::Running => &[
            ("↑↓ / PgUp/Dn", "Scroll log"),
            ("Enter", "Continue (when done)"),
            ("q", "Quit"),
        ],
        Screen::Complete | Screen::Failed => &[("q / Enter", "Exit")],
    };

    let mut spans: Vec<Span> = Vec::new();
    for (i, (key, desc)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ", Style::default()));
        }
        spans.push(Span::styled(
            format!("[{key}]"),
            Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(format!(" {desc}"), Style::default().fg(TEXT)));
    }

    let bar = Paragraph::new(Line::from(spans))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(BORDER))
                .style(Style::default().bg(SURFACE)),
        )
        .alignment(Alignment::Left);
    f.render_widget(bar, area);
}

fn render_screen(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    match app.screen.clone() {
        Screen::Welcome => render_welcome(f, area),
        Screen::Preflight => render_preflight(f, app, area),
        Screen::SelectImage => render_select_image(f, app, area),
        Screen::ConfigureOptions => render_configure_options(f, app, area),
        Screen::Review => render_review(f, app, area),
        Screen::Running => render_running(f, app, area),
        Screen::Complete => render_complete(f, area),
        Screen::Failed => render_failed(f, app, area),
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

// ── Quit dialog ───────────────────────────────────────────────────────────────

fn render_quit_dialog(f: &mut ratatui::Frame, area: Rect) {
    let popup = centered_rect(40, 35, area);
    f.render_widget(Clear, popup);

    let block = Block::default()
        .title(Span::styled(
            " Quit? ",
            Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(AMBER))
        .style(Style::default().bg(SURFACE));

    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Length(3), // buttons
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner);

    let msg = Paragraph::new(vec![
        Line::from(Span::styled(
            "  Are you sure you want to quit?",
            Style::default().fg(TEXT),
        )),
        Line::from(Span::styled(
            "  Migration will be abandoned.",
            Style::default().fg(AMBER),
        )),
    ]);
    f.render_widget(msg, chunks[1]);

    // Button row
    let btn_row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(14),
            Constraint::Length(2),
            Constraint::Length(16),
            Constraint::Min(0),
        ])
        .split(chunks[3]);

    let quit_btn = Paragraph::new(Line::from(Span::styled(
        "    Quit    ",
        Style::default()
            .fg(Color::Rgb(255, 240, 240))
            .bg(DANGER_BG)
            .add_modifier(Modifier::BOLD),
    )))
    .alignment(Alignment::Center);
    f.render_widget(quit_btn, btn_row[1]);

    let stay_btn = Paragraph::new(Line::from(Span::styled(
        "  Keep going  ",
        Style::default()
            .fg(TEXT)
            .bg(SURFACE)
            .add_modifier(Modifier::BOLD),
    )))
    .alignment(Alignment::Center);
    f.render_widget(stay_btn, btn_row[3]);

    let hint = Paragraph::new(Line::from(Span::styled(
        "  ← / → to select, Enter to confirm, Esc to cancel",
        Style::default().fg(MUTED),
    )));
    f.render_widget(hint, chunks[4]);
}

// ─── Main event loop ──────────────────────────────────────────────────────────

/// Entry point for the interactive TUI wizard.
pub fn run_tui() -> Result<()> {
    // Setup terminal
    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen).context("enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create terminal")?;

    let mut app = App::new();
    let result = event_loop(&mut terminal, &mut app);

    // Restore terminal
    disable_raw_mode().context("disable raw mode")?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen).context("leave alternate screen")?;
    terminal.show_cursor().context("show cursor")?;

    result
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    loop {
        // Drain migration channel first
        if app.screen == Screen::Running {
            app.drain_migration_channel();
            app.advance_spinner();

            // Auto-advance when done
            if app.migration_done && !app.show_quit_dialog {
                // Let user press Enter themselves; we just stop draining
            }
        }

        terminal.draw(|f| render(f, app))?;

        // Poll with short timeout to animate spinner and drain channel
        if event::poll(Duration::from_millis(50))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            let should_quit = app.handle_key(key.code, key.modifiers);
            if should_quit {
                break;
            }
        }
    }
    Ok(())
}

// The tests drive the same `App` state machine and `render` function the
// live event loop uses, with keys fed to `handle_key` directly and frames
// drawn into a `TestBackend` buffer. Only the raw-terminal plumbing in
// `run_tui`/`event_loop` (raw mode, alternate screen, crossterm polling)
// is outside their reach — that is what the tui-migrate E2E cell drives
// on a real VM (tests/tui-e2e-driver.py).
#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    /// Render `app` once into a fresh TestBackend and return the buffer as
    /// one newline-joined string for content assertions.
    fn draw_to_text(app: &mut App) -> String {
        let backend = TestBackend::new(100, 40);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal.draw(|f| render(f, app)).expect("draw");
        let buf = terminal.backend().buffer();
        let area = buf.area;
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    /// A fresh App without running detect/preflight side effects beyond
    /// `App::new`'s os-release read (harmless on any host).
    fn app_on(screen: Screen) -> App {
        let mut app = App::new();
        app.screen = screen;
        app
    }

    #[test]
    fn log_line_classification() {
        type KindPred = fn(&LogKind) -> bool;
        let cases: &[(&str, KindPred)] = &[
            ("=== Phase 3: Seal ===", |k| matches!(k, LogKind::Header)),
            ("[phase2] pulling layer", |k| matches!(k, LogKind::Phase)),
            ("something failed badly", |k| matches!(k, LogKind::Error)),
            ("ERROR: no space", |k| matches!(k, LogKind::Error)),
            ("✓ store sealed", |k| matches!(k, LogKind::Success)),
            ("plain progress line", |k| matches!(k, LogKind::Normal)),
        ];
        for (raw, pred) in cases {
            let line = LogLine::classify(raw);
            assert!(pred(&line.kind), "unexpected kind for {raw:?}");
        }
    }

    #[test]
    fn phases_follow_migration_log_lines() {
        let mut app = app_on(Screen::Running);
        app.update_phases_from_line("=== Phase 0: Preflight ===");
        assert_eq!(app.phases[0].status, PhaseStatus::Running);
        app.update_phases_from_line("=== Phase 1: OSTree import ===");
        assert_eq!(app.phases[0].status, PhaseStatus::Done);
        assert_eq!(app.phases[1].status, PhaseStatus::Running);
        app.update_phases_from_line("[phase2] GET blob");
        assert_eq!(app.phases[1].status, PhaseStatus::Done);
        assert_eq!(app.phases[2].status, PhaseStatus::Running);
        app.update_phases_from_line("=== MIGRATION COMPLETED ===");
        assert!(app.phases.iter().all(|p| p.status == PhaseStatus::Done));
    }

    #[test]
    fn skipped_import_phase_is_marked_skipped() {
        let mut app = app_on(Screen::Running);
        app.update_phases_from_line("=== Phase 1: Skipped (--skip-import) ===");
        assert_eq!(app.phases[0].status, PhaseStatus::Done);
        assert_eq!(app.phases[1].status, PhaseStatus::Skipped);
    }

    #[test]
    fn command_args_reflect_configured_options() {
        let mut app = App::new();
        // Defaults: first preset, dry-run on, systemd-boot.
        let args = app.build_command_args();
        assert!(args.contains(&"--target-image".to_string()));
        assert!(args.contains(&app.image_choices[0].image.clone()));
        assert!(args.contains(&"--dry-run".to_string()));
        assert!(args.contains(&"systemd-boot".to_string()));
        assert!(!args.contains(&"--force".to_string()));

        app.opt_dry_run = false;
        app.opt_skip_import = true;
        app.opt_force = true;
        app.opt_skip_preflight = true;
        app.opt_bootloader = Bootloader::Grub2;
        let args = app.build_command_args();
        assert!(!args.contains(&"--dry-run".to_string()));
        assert!(args.contains(&"--skip-import".to_string()));
        assert!(args.contains(&"--force".to_string()));
        assert!(args.contains(&"--skip-preflight".to_string()));
        assert!(args.contains(&"grub2".to_string()));
    }

    #[test]
    fn custom_image_entry_via_keys() {
        let mut app = app_on(Screen::SelectImage);
        // Move to the last row ("Custom…").
        for _ in 0..app.image_choices.len() {
            app.handle_key(KeyCode::Down, KeyModifiers::NONE);
        }
        assert!(app.is_custom_selected());
        // Enter with an empty custom image starts editing instead of advancing.
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.custom_image_editing);
        assert_eq!(app.screen, Screen::SelectImage);
        for c in "quay.io/x/y:z".chars() {
            app.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        app.handle_key(KeyCode::Backspace, KeyModifiers::NONE);
        app.handle_key(KeyCode::Char('z'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE); // stop editing
        assert!(!app.custom_image_editing);
        assert_eq!(app.selected_image(), "quay.io/x/y:z");
        // Second Enter advances to the options screen.
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.screen, Screen::ConfigureOptions);
    }

    #[test]
    fn select_image_cursor_stays_in_bounds() {
        let mut app = app_on(Screen::SelectImage);
        app.handle_key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.image_list_state.selected(), Some(0));
        for _ in 0..(app.image_choices.len() * 2) {
            app.handle_key(KeyCode::Down, KeyModifiers::NONE);
        }
        assert_eq!(
            app.image_list_state.selected(),
            Some(app.image_choices.len() - 1)
        );
    }

    #[test]
    fn option_toggles_and_bootloader_arrows() {
        let mut app = app_on(Screen::ConfigureOptions);
        assert!(app.opt_dry_run);
        app.handle_key(KeyCode::Char(' '), KeyModifiers::NONE);
        assert!(!app.opt_dry_run);
        // Move to the bootloader row (index 2) and pick GRUB2 with →.
        app.handle_key(KeyCode::Down, KeyModifiers::NONE);
        app.handle_key(KeyCode::Down, KeyModifiers::NONE);
        app.handle_key(KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(app.opt_bootloader, Bootloader::Grub2);
        app.handle_key(KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(app.opt_bootloader, Bootloader::SystemdBoot);
        // 'n' advances to the review screen.
        app.handle_key(KeyCode::Char('n'), KeyModifiers::NONE);
        assert_eq!(app.screen, Screen::Review);
        // 'b' goes back.
        app.handle_key(KeyCode::Char('b'), KeyModifiers::NONE);
        assert_eq!(app.screen, Screen::ConfigureOptions);
    }

    #[test]
    fn quit_dialog_flow() {
        let mut app = app_on(Screen::SelectImage);
        assert!(!app.show_quit_dialog);
        app.handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(app.show_quit_dialog);
        // "Keep going" (right) + Enter closes the dialog without quitting.
        app.handle_key(KeyCode::Right, KeyModifiers::NONE);
        let quit = app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(!quit);
        assert!(!app.show_quit_dialog);
        // 'q' reopens; "Quit" (left) + Enter signals exit.
        app.handle_key(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(app.show_quit_dialog);
        app.handle_key(KeyCode::Left, KeyModifiers::NONE);
        let quit = app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(quit);
    }

    #[test]
    fn migration_channel_success_marks_phases_done() {
        let mut app = app_on(Screen::Running);
        let (tx, rx) = mpsc::channel::<MigMsg>();
        app.rx = Some(rx);
        tx.send(MigMsg::Line("=== Phase 0: Preflight ===".into()))
            .unwrap();
        tx.send(MigMsg::Line("=== MIGRATION COMPLETED ===".into()))
            .unwrap();
        tx.send(MigMsg::Done(true)).unwrap();
        app.drain_migration_channel();
        assert!(app.migration_done);
        assert!(app.migration_success);
        assert!(app.phases.iter().all(|p| p.status == PhaseStatus::Done));
        assert_eq!(app.log_lines.len(), 2);
        // Enter now advances to the Complete screen.
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.screen, Screen::Complete);
    }

    #[test]
    fn migration_channel_failure_marks_running_phase_failed() {
        let mut app = app_on(Screen::Running);
        let (tx, rx) = mpsc::channel::<MigMsg>();
        app.rx = Some(rx);
        tx.send(MigMsg::Line("=== Phase 0: Preflight ===".into()))
            .unwrap();
        tx.send(MigMsg::Line("ERROR: preflight refused".into()))
            .unwrap();
        tx.send(MigMsg::Done(false)).unwrap();
        app.drain_migration_channel();
        assert!(app.migration_done);
        assert!(!app.migration_success);
        assert_eq!(app.phases[0].status, PhaseStatus::Failed);
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.screen, Screen::Failed);
    }

    #[test]
    fn running_screen_scrolls_log() {
        let mut app = app_on(Screen::Running);
        for i in 0..30 {
            app.log_lines.push(LogLine::classify(&format!("line {i}")));
        }
        app.log_scroll = 29;
        app.handle_key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.log_scroll, 28);
        app.handle_key(KeyCode::PageUp, KeyModifiers::NONE);
        assert_eq!(app.log_scroll, 18);
        app.handle_key(KeyCode::PageDown, KeyModifiers::NONE);
        assert_eq!(app.log_scroll, 28);
        app.handle_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.log_scroll, 29);
        // Enter while the migration is still running must not leave Running.
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.screen, Screen::Running);
    }

    #[test]
    fn back_navigation_mapping() {
        let mut app = app_on(Screen::Review);
        app.prev_screen();
        assert_eq!(app.screen, Screen::ConfigureOptions);
        app.prev_screen();
        assert_eq!(app.screen, Screen::SelectImage);
        app.prev_screen();
        assert_eq!(app.screen, Screen::Preflight);
        app.prev_screen();
        assert_eq!(app.screen, Screen::Welcome);
        app.prev_screen();
        assert_eq!(app.screen, Screen::Welcome);
    }

    #[test]
    fn end_screens_exit_on_q_or_enter() {
        for screen in [Screen::Complete, Screen::Failed] {
            let mut app = app_on(screen);
            assert!(app.handle_key(KeyCode::Char('q'), KeyModifiers::NONE));
        }
    }

    // ── Render assertions ────────────────────────────────────────────────

    #[test]
    fn welcome_screen_renders_branding_and_hints() {
        let mut app = App::new();
        let text = draw_to_text(&mut app);
        assert!(text.contains("bootc-migrate"), "missing title:\n{text}");
        assert!(text.contains("Step 1 of 5"), "missing step:\n{text}");
        assert!(text.contains("[Enter]"), "missing hint:\n{text}");
    }

    #[test]
    fn select_image_screen_lists_targets() {
        let mut app = app_on(Screen::SelectImage);
        let text = draw_to_text(&mut app);
        assert!(text.contains("Step 3 of 5"), "missing step:\n{text}");
        assert!(text.contains("Dakota stable"), "missing target:\n{text}");
        assert!(text.contains("Custom"), "missing custom row:\n{text}");
    }

    // ── Target-image detection ────────────────────────────────────────────
    //
    // The screen used to offer three rows — "Dakota stable (default)", "(from
    // LTS/XFS)" and "(from Aurora)" — that all pointed at the same image and
    // differed only in a hint matched against the detected OS. Picking one was
    // a question the tool could already answer, and every answer was the same.

    #[test]
    fn ostree_host_is_offered_one_target_not_three_of_the_same() {
        let choices = image_choices(
            Some(Backend::Ostree),
            Some("ghcr.io/ublue-os/bluefin:gts"),
            "Bluefin (gts)",
        );
        let targets: Vec<&str> = choices
            .iter()
            .filter(|c| !c.custom)
            .map(|c| c.image.as_str())
            .collect();
        assert_eq!(
            targets,
            vec![DAKOTA_STABLE],
            "one conversion target, offered once"
        );
        assert!(choices.last().is_some_and(|c| c.custom), "custom stays");
    }

    /// The detected image is what justifies the preselection, so it has to be
    /// visible. It lives in the header rather than in each row's note, which
    /// is what kept the notes short enough not to be truncated at 100 columns.
    #[test]
    fn the_screen_shows_what_was_detected() {
        let mut app = app_on(Screen::SelectImage);
        app.booted_backend = Some(Backend::Ostree);
        app.booted_image = Some("ghcr.io/ublue-os/bluefin:gts".into());
        app.image_choices = image_choices(
            app.booted_backend,
            app.booted_image.as_deref(),
            &app.detected_os,
        );
        let text = draw_to_text(&mut app);
        assert!(
            text.contains("ghcr.io/ublue-os/bluefin:gts"),
            "the booted image must be on screen:\n{text}"
        );
        assert!(text.contains("ostree"), "the backend too:\n{text}");
    }

    /// Aurora and LTS had their own rows purely to be recognised. They resolve
    /// to the same single target as any other ostree host now.
    #[test]
    fn every_ostree_source_resolves_to_the_same_single_target() {
        for os in ["Bluefin (gts)", "Aurora", "Bluefin LTS", "Unknown OS"] {
            let choices = image_choices(Some(Backend::Ostree), None, os);
            assert_eq!(choices[0].image, DAKOTA_STABLE, "for {os}");
            assert_eq!(choices.len(), 2, "target + custom, for {os}");
        }
    }

    /// On composefs the operation is a swap, so the booted image leads: the
    /// user edits a tag rather than typing a reference from memory.
    #[test]
    fn composefs_host_leads_with_the_image_it_is_running() {
        let choices = image_choices(
            Some(Backend::Composefs),
            Some("ghcr.io/projectbluefin/dakota:gts"),
            "Dakota",
        );
        assert_eq!(choices[0].image, "ghcr.io/projectbluefin/dakota:gts");
        assert!(!choices[0].custom, "the current image is a real target");
        assert!(
            choices.iter().any(|c| c.image == DAKOTA_STABLE),
            "stable stays available: {choices:?}"
        );
    }

    /// Already on the default: don't offer it twice.
    #[test]
    fn composefs_host_on_stable_is_not_offered_stable_again() {
        let choices = image_choices(Some(Backend::Composefs), Some(DAKOTA_STABLE), "Dakota");
        assert_eq!(
            choices.iter().filter(|c| c.image == DAKOTA_STABLE).count(),
            1,
            "{choices:?}"
        );
    }

    /// Preflight could not run (not root, or it failed). Still usable.
    #[test]
    fn unknown_backend_still_offers_a_target_and_custom() {
        let choices = image_choices(None, None, "Unknown OS");
        assert_eq!(choices[0].image, DAKOTA_STABLE);
        assert!(choices.last().is_some_and(|c| c.custom));
    }

    /// Print the screen so a human can look at it. `cargo test -- --nocapture
    /// select_image_screen_preview`.
    #[test]
    fn select_image_screen_preview() {
        for (name, backend, image) in [
            (
                "ostree host",
                Some(Backend::Ostree),
                Some("ghcr.io/ublue-os/bluefin:gts"),
            ),
            (
                "composefs host",
                Some(Backend::Composefs),
                Some("ghcr.io/projectbluefin/dakota:gts"),
            ),
        ] {
            let mut app = app_on(Screen::SelectImage);
            app.detected_os = "Bluefin (gts)".to_owned();
            app.booted_backend = backend;
            app.booted_image = image.map(str::to_owned);
            app.image_choices = image_choices(backend, image, &app.detected_os);
            app.image_list_state.select(Some(0));
            println!("\n──────── {name} ────────");
            for line in draw_to_text(&mut app).lines().take(12) {
                println!("{}", line.trim_end());
            }
        }
    }

    /// The point of the whole change: row 0 is selected, so Enter is enough.
    #[test]
    fn the_detected_target_is_preselected() {
        let mut app = app_on(Screen::SelectImage);
        app.refresh_image_choices(&bootc_migrate_core::preflight::PreflightReport {
            booted_backend: Some(Backend::Composefs),
            booted_image: Some("ghcr.io/projectbluefin/dakota:gts".into()),
            pending_transaction: bootc_migrate_core::preflight::PendingTransactionStatus::Clean,
            is_uefi: true,
            nvram_writable: true,
            esp_path: Some("/boot/efi".into()),
            esp_free_space_bytes: 386 * 1024 * 1024,
            esp_fs_type: Some("vfat".into()),
            esp_detected: true,
            supports_reflink: true,
            is_btrfs: true,
            fs_type: Some("btrfs".into()),
            ostree_repo_size_bytes: 0,
            composefs_free_bytes: 40_600_000_000,
            container_storage_free_bytes: 50 * 1024 * 1024 * 1024,
            container_storage_path: "/var/lib/containers/storage".into(),
            var_is_separate_mount: false,
            esp_ready_for_systemd_boot: true,
            systemd_boot_binaries_present: true,
            grub_tools_available: true,
            sysroot_was_ro: false,
        });
        assert_eq!(app.image_list_state.selected(), Some(0));
        assert_eq!(
            app.selected_image(),
            "ghcr.io/projectbluefin/dakota:gts",
            "pressing Enter must migrate to the detected target"
        );
    }

    #[test]
    fn options_screen_shows_dry_run_tag_in_title() {
        let mut app = app_on(Screen::ConfigureOptions);
        let text = draw_to_text(&mut app);
        assert!(text.contains("DRY-RUN"), "missing dry-run tag:\n{text}");
        app.opt_dry_run = false;
        let text = draw_to_text(&mut app);
        assert!(text.contains("LIVE MIGRATION"), "missing live tag:\n{text}");
    }

    #[test]
    fn review_screen_shows_the_exact_command() {
        let mut app = app_on(Screen::Review);
        let text = draw_to_text(&mut app);
        assert!(text.contains("--target-image"), "missing command:\n{text}");
        assert!(text.contains("--dry-run"), "missing dry-run flag:\n{text}");
    }

    #[test]
    fn running_screen_renders_phases_and_log() {
        let mut app = app_on(Screen::Running);
        app.update_phases_from_line("=== Phase 0: Preflight ===");
        app.log_lines
            .push(LogLine::classify("=== Phase 0: Preflight ==="));
        let text = draw_to_text(&mut app);
        assert!(text.contains("Preflight"), "missing phase:\n{text}");
        assert!(
            text.contains("Migration running"),
            "missing running title:\n{text}"
        );
    }

    #[test]
    fn end_screens_render_their_verdicts() {
        let mut app = app_on(Screen::Complete);
        let text = draw_to_text(&mut app);
        assert!(
            text.contains("Migration complete"),
            "missing complete title:\n{text}"
        );
        let mut app = app_on(Screen::Failed);
        let text = draw_to_text(&mut app);
        assert!(
            text.contains("Migration failed"),
            "missing failed title:\n{text}"
        );
    }

    #[test]
    fn quit_dialog_overlays_current_screen() {
        let mut app = app_on(Screen::SelectImage);
        app.show_quit_dialog = true;
        let text = draw_to_text(&mut app);
        assert!(
            text.contains("Are you sure you want to quit?"),
            "missing dialog:\n{text}"
        );
        assert!(text.contains("Keep going"), "missing button:\n{text}");
    }

    #[test]
    fn preflight_screen_renders_without_report() {
        // preflight_state None (e.g. run as non-root) must still render.
        let mut app = app_on(Screen::Preflight);
        app.preflight_state = None;
        let text = draw_to_text(&mut app);
        assert!(text.contains("Step 2 of 5"), "missing step:\n{text}");
    }

    // ── Palette contrast ──────────────────────────────────────────────────
    //
    // These lock in the fix for the unreadable dark theme. Every rule below
    // corresponds to something that was actually wrong on screen, so a future
    // palette tweak that reintroduces it fails here rather than shipping.

    /// sRGB relative luminance (WCAG 2.1 §Relative luminance).
    fn luminance(c: Color) -> f64 {
        let Color::Rgb(r, g, b) = c else {
            panic!("palette must be explicit Rgb, got {c:?}");
        };
        let ch = |v: u8| {
            let v = f64::from(v) / 255.0;
            if v <= 0.03928 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * ch(r) + 0.7152 * ch(g) + 0.0722 * ch(b)
    }

    /// WCAG 2.1 contrast ratio, 1.0 (identical) to 21.0 (black on white).
    fn contrast(a: Color, b: Color) -> f64 {
        let (la, lb) = (luminance(a), luminance(b));
        let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
        (hi + 0.05) / (lo + 0.05)
    }

    /// Text must clear AA (4.5:1) on both panel backgrounds.
    #[test]
    fn every_text_colour_is_readable_on_both_backgrounds() {
        for (name, fg) in [
            ("TEAL", TEAL),
            ("AMBER", AMBER),
            ("MUTED", MUTED),
            ("SUCCESS", SUCCESS),
            ("DANGER", DANGER),
            ("TEXT", TEXT),
        ] {
            for (bg_name, bg) in [("DARK_BG", DARK_BG), ("SURFACE", SURFACE)] {
                let ratio = contrast(fg, bg);
                assert!(
                    ratio >= 4.5,
                    "{name} on {bg_name} is {ratio:.2}:1, below the 4.5:1 floor \
                     for text. Pick a lighter {name} or stop using it for text."
                );
            }
        }
    }

    /// `BORDER` is exempt from the text floor precisely because it must never
    /// carry text — but it still has to be visible as chrome (3:1).
    #[test]
    fn border_is_visible_chrome_but_not_text_grade() {
        for (bg_name, bg) in [("DARK_BG", DARK_BG), ("SURFACE", SURFACE)] {
            let ratio = contrast(BORDER, bg);
            assert!(
                ratio >= 3.0,
                "BORDER on {bg_name} is {ratio:.2}:1, below the 3:1 non-text floor"
            );
        }
    }

    /// Light-on-colour pairs: buttons and the error banner set their own
    /// background, so they are checked against that, not against the panel.
    #[test]
    fn on_colour_pairs_are_readable() {
        let button_green = Color::Rgb(40, 180, 70);
        let banner_text = Color::Rgb(255, 240, 240);
        for (name, fg, bg) in [
            ("welcome/dry-run button", DARK_BG, TEAL),
            ("run-migration button", DARK_BG, button_green),
            ("quit button", banner_text, DANGER_BG),
        ] {
            let ratio = contrast(fg, bg);
            assert!(ratio >= 4.5, "{name} is {ratio:.2}:1, below 4.5:1");
        }
    }

    /// The gauge readout is drawn over the panel, never over the bar, so it is
    /// held to the same floor as any other text. This is the regression that
    /// made `0%` invisible: it used to be drawn `fg(DARK_BG)` on top of the
    /// unfilled remainder, which `Gauge` leaves at the panel background.
    #[test]
    fn gauge_readout_never_lands_dark_on_dark() {
        for (name, fill) in [("SUCCESS", SUCCESS), ("AMBER", AMBER), ("DANGER", DANGER)] {
            for (bg_name, bg) in [("DARK_BG", DARK_BG), ("SURFACE", SURFACE)] {
                let ratio = contrast(fill, bg);
                assert!(
                    ratio >= 4.5,
                    "gauge readout {name} on {bg_name} is {ratio:.2}:1"
                );
            }
            // And the bar itself has to be distinguishable from its track.
            let ratio = contrast(fill, TRACK);
            assert!(
                ratio >= 3.0,
                "{name} fill against TRACK is {ratio:.2}:1 — the bar would not read as full"
            );
        }
    }

    /// The readouts must actually be on screen — an audit that only checks
    /// contrast would also pass if the text vanished entirely.
    #[test]
    fn preflight_gauges_show_their_readouts() {
        let mut app = app_on(Screen::Preflight);
        app.preflight_state = Some(preflight::PreflightTuiState::from_report(
            &bootc_migrate_core::preflight::PreflightReport {
                booted_backend: Some(bootc_migrate_core::rebase_plan::Backend::Ostree),
                booted_image: Some("ghcr.io/ublue-os/bluefin:gts".into()),
                pending_transaction: bootc_migrate_core::preflight::PendingTransactionStatus::Clean,
                is_uefi: true,
                nvram_writable: true,
                esp_path: Some("/boot/efi".into()),
                esp_free_space_bytes: 386 * 1024 * 1024,
                esp_fs_type: Some("vfat".into()),
                esp_detected: true,
                supports_reflink: true,
                is_btrfs: true,
                fs_type: Some("btrfs".into()),
                ostree_repo_size_bytes: 0,
                composefs_free_bytes: 40_600_000_000,
                container_storage_free_bytes: 50 * 1024 * 1024 * 1024,
                container_storage_path: "/var/lib/containers/storage".into(),
                var_is_separate_mount: false,
                esp_ready_for_systemd_boot: true,
                systemd_boot_binaries_present: true,
                grub_tools_available: true,
                sysroot_was_ro: false,
            },
        ));
        let text = draw_to_text(&mut app);
        assert!(text.contains("0%"), "gauge percentage missing:\n{text}");
        assert!(
            text.contains("GB used"),
            "projected-usage readout missing:\n{text}"
        );
    }

    /// Whether a glyph is chrome (borders, bars, block fills) rather than text.
    /// Chrome is held to the 3:1 non-text floor, everything else to 4.5:1.
    fn is_chrome(sym: &str) -> bool {
        sym.chars()
            .all(|c| matches!(c, '\u{2500}'..='\u{259f}' | '\u{2800}'..='\u{28ff}'))
    }

    /// Audit what is actually on screen, cell by cell, on every screen.
    ///
    /// The palette tests above check the colours we *intend* to pair. This
    /// checks the ones that end up paired, which is a different thing and is
    /// how the gauge bug survived: `DANGER`, `SURFACE` and `DARK_BG` were each
    /// fine, and the defect was a readout drawn `fg(DARK_BG)` over a cell whose
    /// background the gauge had left at the panel colour. No palette-level
    /// assertion can see that; this one reads the rendered buffer.
    #[test]
    fn no_screen_renders_unreadable_cells() {
        for screen in [
            Screen::Welcome,
            Screen::Preflight,
            Screen::SelectImage,
            Screen::ConfigureOptions,
            Screen::Review,
            Screen::Running,
            Screen::Complete,
            Screen::Failed,
        ] {
            let label = format!("{screen:?}");
            let mut app = app_on(screen);
            // Give preflight real numbers so the gauges and checklist draw.
            app.preflight_state = Some(preflight::PreflightTuiState::from_report(
                &bootc_migrate_core::preflight::PreflightReport {
                    // No bootc deployment at all — the one genuine blocker,
                    // so the red checklist path renders too.
                    booted_backend: None,
                    booted_image: None,
                    pending_transaction:
                        bootc_migrate_core::preflight::PendingTransactionStatus::Clean,
                    is_uefi: true,
                    nvram_writable: true,
                    esp_path: Some("/boot/efi".into()),
                    esp_free_space_bytes: 386 * 1024 * 1024,
                    esp_fs_type: Some("vfat".into()),
                    esp_detected: true,
                    supports_reflink: true,
                    is_btrfs: true,
                    fs_type: Some("btrfs".into()),
                    // Zero repo size is the case from the bug report: it makes
                    // every gauge 0%, which is exactly where the readout used
                    // to disappear.
                    ostree_repo_size_bytes: 0,
                    composefs_free_bytes: 40_600_000_000,
                    container_storage_free_bytes: 50 * 1024 * 1024 * 1024,
                    container_storage_path: "/var/lib/containers/storage".into(),
                    var_is_separate_mount: false,
                    esp_ready_for_systemd_boot: true,
                    systemd_boot_binaries_present: true,
                    grub_tools_available: true,
                    sysroot_was_ro: false,
                },
            ));
            let backend = TestBackend::new(100, 40);
            let mut terminal = Terminal::new(backend).expect("test terminal");
            terminal.draw(|f| render(f, &mut app)).expect("draw");
            let buf = terminal.backend().buffer();

            let mut worst: Option<(f64, u16, u16, String)> = None;
            for y in 0..buf.area.height {
                for x in 0..buf.area.width {
                    let cell = &buf[(x, y)];
                    let sym = cell.symbol();
                    if sym.trim().is_empty() {
                        continue;
                    }
                    // The app paints a DARK_BG block over the whole frame, so
                    // an unset background is that, not the terminal default.
                    let bg = match cell.bg {
                        Color::Reset => DARK_BG,
                        other => other,
                    };
                    let fg = match cell.fg {
                        Color::Reset => TEXT,
                        other => other,
                    };
                    let floor = if is_chrome(sym) { 3.0 } else { 4.5 };
                    let ratio = contrast(fg, bg);
                    if ratio < floor && worst.as_ref().is_none_or(|(w, _, _, _)| ratio < *w) {
                        worst = Some((ratio, x, y, sym.to_string()));
                    }
                }
            }
            assert!(
                worst.is_none(),
                "{label}: cell {:?} at ({}, {}) renders at {:.2}:1 — unreadable",
                worst.as_ref().map(|w| w.3.clone()),
                worst.as_ref().map(|w| w.1).unwrap_or(0),
                worst.as_ref().map(|w| w.2).unwrap_or(0),
                worst.as_ref().map(|w| w.0).unwrap_or(0.0),
            );
        }
    }
}
