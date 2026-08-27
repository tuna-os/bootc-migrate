//! Interactive "Config Drift Review" checklist (issue #15, "Phase 0.5").
//!
//! Presents the `/etc` config drift computed by
//! [`bootc_migrate_core::migration::deploy::compute_etc_drift`] as a
//! checkable list before migration mutates anything. A checked entry keeps
//! the user's live version — today's default behavior; unchecking an entry
//! takes the target image's new default instead. Confirming produces an
//! [`EtcDriftManifest`] that Phase 4's `/etc` merge honors per path (see
//! `bootc_migrate_core::mergetc::merge_etc_files_with_overrides`).
//!
//! The terminal event loop itself carries no unit tests — driving a real
//! terminal isn't something a compile+unit-test loop can prove, the same
//! policy this project applies to its other interactive-only work (see
//! ROADMAP.md's decision log for #65/#31/#15). The [`ReviewState`] state
//! machine and the `render` function underneath it are pure and *are*
//! tested (keys via `handle_key`, frames via ratatui's `TestBackend`),
//! matching `bootc-rebase`'s boot-entry checklist; the raw-terminal loop
//! is exercised live by the tui-migrate E2E cell (tests/tui-e2e-driver.py)
//! and by hand on Corral VMs (AGENTS.md).

use anyhow::{Context, Result};
use bootc_migrate_core::mergetc::{DriftKind, EtcDriftEntry, EtcDriftManifest};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};
use std::time::Duration;

const TEAL: Color = Color::Rgb(0, 180, 180);
const AMBER: Color = Color::Rgb(220, 160, 0);
const DANGER: Color = Color::Rgb(220, 60, 60);
const DARK_BG: Color = Color::Rgb(18, 20, 24);
const SURFACE: Color = Color::Rgb(30, 34, 42);
const SUBTLE: Color = Color::Rgb(90, 100, 115);
const SUCCESS: Color = Color::Rgb(80, 200, 100);
const TEXT: Color = Color::Rgb(210, 215, 225);

fn kind_label(kind: DriftKind) -> (&'static str, Color) {
    match kind {
        DriftKind::Added => ("Added", SUCCESS),
        DriftKind::Modified => ("Modified", AMBER),
        DriftKind::Removed => ("Removed", DANGER),
        DriftKind::TypeChanged => ("TypeChanged", TEAL),
    }
}

struct ReviewState {
    entries: Vec<EtcDriftEntry>,
    /// `true` = keep current (checked), `false` = take target default.
    checked: Vec<bool>,
    list_state: ListState,
    cancelled: bool,
}

impl ReviewState {
    fn new(entries: Vec<EtcDriftEntry>) -> Self {
        // Default: every entry pre-selected "keep current" — matching
        // today's merge behavior when no review takes place, so behavior
        // doesn't silently change for anyone who doesn't interact (#15).
        let checked = vec![true; entries.len()];
        let mut list_state = ListState::default();
        if !entries.is_empty() {
            list_state.select(Some(0));
        }
        Self {
            entries,
            checked,
            list_state,
            cancelled: false,
        }
    }

    fn manifest(&self) -> EtcDriftManifest {
        let decisions = self
            .entries
            .iter()
            .zip(&self.checked)
            .map(|(entry, keep_current)| (entry.path.clone(), *keep_current))
            .collect();
        EtcDriftManifest { decisions }
    }

    fn move_cursor(&mut self, delta: isize) {
        if self.entries.is_empty() {
            return;
        }
        let len = self.entries.len() as isize;
        let cur = self.list_state.selected().unwrap_or(0) as isize;
        let next = (cur + delta).rem_euclid(len);
        self.list_state.select(Some(next as usize));
    }

    fn toggle_current(&mut self) {
        if let Some(i) = self.list_state.selected() {
            self.checked[i] = !self.checked[i];
        }
    }

    fn select_all(&mut self, value: bool) {
        self.checked.fill(value);
    }

    /// Handle one key press. Returns `true` when the review loop should end.
    fn handle_key(&mut self, key: KeyCode) -> bool {
        match key {
            KeyCode::Up | KeyCode::Char('k') => self.move_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(1),
            KeyCode::Char(' ') => self.toggle_current(),
            KeyCode::Char('a') => self.select_all(true),
            KeyCode::Char('n') => self.select_all(false),
            KeyCode::Enter => return true,
            KeyCode::Esc | KeyCode::Char('q') => {
                self.cancelled = true;
                return true;
            }
            _ => {}
        }
        false
    }
}

fn render(f: &mut ratatui::Frame, state: &mut ReviewState) {
    let area = f.area();
    f.render_widget(Block::default().style(Style::default().bg(DARK_BG)), area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(3),
        ])
        .split(area);

    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            " Phase 0.5 · /etc Config Drift Review ",
            Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("({} change(s))", state.entries.len()),
            Style::default().fg(SUBTLE),
        ),
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(TEAL))
            .style(Style::default().bg(SURFACE)),
    );
    f.render_widget(title, chunks[0]);

    let items: Vec<ListItem> = state
        .entries
        .iter()
        .zip(&state.checked)
        .map(|(entry, checked)| {
            let (kind_str, kind_color) = kind_label(entry.kind);
            let box_char = if *checked { "☑" } else { "☐" };
            let line = Line::from(vec![
                Span::styled(
                    format!("  {box_char} "),
                    Style::default().fg(if *checked { SUCCESS } else { SUBTLE }),
                ),
                Span::styled(
                    format!("/etc/{:<50}", entry.path),
                    Style::default().fg(TEXT),
                ),
                Span::styled(format!(" [{kind_str}]"), Style::default().fg(kind_color)),
            ]);
            ListItem::new(line)
        })
        .collect();

    let list_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(TEAL))
        .title(" ☑ keep current   ☐ take target default ")
        .style(Style::default().bg(DARK_BG));
    let list = List::new(items)
        .block(list_block)
        .highlight_style(Style::default().bg(SURFACE));
    f.render_stateful_widget(list, chunks[1], &mut state.list_state);

    let hints = Paragraph::new(Line::from(vec![
        Span::styled(
            "[↑↓/jk] ",
            Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
        ),
        Span::styled("Move  ", Style::default().fg(TEXT)),
        Span::styled(
            "[Space] ",
            Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
        ),
        Span::styled("Toggle  ", Style::default().fg(TEXT)),
        Span::styled(
            "[a] ",
            Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
        ),
        Span::styled("Select all  ", Style::default().fg(TEXT)),
        Span::styled(
            "[n] ",
            Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
        ),
        Span::styled("Select none  ", Style::default().fg(TEXT)),
        Span::styled(
            "[Enter] ",
            Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
        ),
        Span::styled("Continue  ", Style::default().fg(TEXT)),
        Span::styled(
            "[Esc/q] ",
            Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
        ),
        Span::styled("Cancel", Style::default().fg(TEXT)),
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(SUBTLE))
            .style(Style::default().bg(SURFACE)),
    );
    f.render_widget(hints, chunks[2]);
}

/// Run the interactive Config Drift Review checklist over `drift` and
/// return the resulting manifest, or `None` if the user cancelled
/// (Esc/q). An empty `drift` list is a no-op — nothing to review — and
/// returns an empty manifest without opening a terminal screen.
pub fn run_review(drift: Vec<EtcDriftEntry>) -> Result<Option<EtcDriftManifest>> {
    if drift.is_empty() {
        println!("No /etc config drift from the OSTree factory default; nothing to review.");
        return Ok(Some(EtcDriftManifest::default()));
    }

    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen).context("enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create terminal")?;

    let mut state = ReviewState::new(drift);
    let run_result = event_loop(&mut terminal, &mut state);

    // Restore the terminal regardless of how the loop ended.
    disable_raw_mode().context("disable raw mode")?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen).context("leave alternate screen")?;
    terminal.show_cursor().context("show cursor")?;

    run_result?;

    if state.cancelled {
        return Ok(None);
    }
    Ok(Some(state.manifest()))
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    state: &mut ReviewState,
) -> Result<()> {
    loop {
        terminal.draw(|f| render(f, state))?;

        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            let should_quit = state.handle_key(key.code);
            if should_quit {
                break;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn entries() -> Vec<EtcDriftEntry> {
        vec![
            EtcDriftEntry {
                path: "hostname".into(),
                kind: DriftKind::Modified,
            },
            EtcDriftEntry {
                path: "sudoers.d/90-realuser".into(),
                kind: DriftKind::Added,
            },
            EtcDriftEntry {
                path: "motd".into(),
                kind: DriftKind::Removed,
            },
        ]
    }

    #[test]
    fn defaults_match_todays_merge_behavior() {
        // Every entry pre-checked "keep current" so a review nobody touches
        // is a no-op relative to not reviewing at all (#15).
        let state = ReviewState::new(entries());
        assert!(state.checked.iter().all(|c| *c));
        assert_eq!(state.list_state.selected(), Some(0));
        let manifest = state.manifest();
        assert_eq!(manifest.decisions.len(), 3);
        assert!(manifest.decisions.values().all(|keep| *keep));
    }

    #[test]
    fn cursor_wraps_both_directions() {
        let mut state = ReviewState::new(entries());
        state.handle_key(KeyCode::Up);
        assert_eq!(state.list_state.selected(), Some(2));
        state.handle_key(KeyCode::Down);
        state.handle_key(KeyCode::Char('j'));
        assert_eq!(state.list_state.selected(), Some(1));
    }

    #[test]
    fn toggle_and_bulk_select_feed_the_manifest() {
        let mut state = ReviewState::new(entries());
        state.handle_key(KeyCode::Down);
        state.handle_key(KeyCode::Char(' '));
        let manifest = state.manifest();
        assert!(manifest.decisions["hostname"]);
        assert!(!manifest.decisions["sudoers.d/90-realuser"]);
        state.handle_key(KeyCode::Char('n'));
        assert!(state.manifest().decisions.values().all(|keep| !keep));
        state.handle_key(KeyCode::Char('a'));
        assert!(state.manifest().decisions.values().all(|keep| *keep));
    }

    #[test]
    fn enter_confirms_and_esc_cancels() {
        let mut state = ReviewState::new(entries());
        assert!(!state.handle_key(KeyCode::Char(' ')));
        assert!(state.handle_key(KeyCode::Enter));
        assert!(!state.cancelled);

        let mut state = ReviewState::new(entries());
        assert!(state.handle_key(KeyCode::Esc));
        assert!(state.cancelled);
    }

    #[test]
    fn empty_drift_state_handles_keys_without_panicking() {
        let mut state = ReviewState::new(Vec::new());
        state.handle_key(KeyCode::Down);
        state.handle_key(KeyCode::Char(' '));
        assert_eq!(state.list_state.selected(), None);
        assert!(state.manifest().decisions.is_empty());
    }

    #[test]
    fn render_shows_paths_kinds_and_checkboxes() {
        let mut state = ReviewState::new(entries());
        state.handle_key(KeyCode::Char(' ')); // uncheck "hostname"
        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal.draw(|f| render(f, &mut state)).expect("draw");
        let buf = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                text.push_str(buf[(x, y)].symbol());
            }
            text.push('\n');
        }
        assert!(text.contains("Config Drift Review"), "title:\n{text}");
        assert!(text.contains("(3 change(s))"), "count:\n{text}");
        assert!(text.contains("/etc/hostname"), "path:\n{text}");
        assert!(text.contains("[Modified]"), "kind:\n{text}");
        assert!(text.contains("☐"), "unchecked box:\n{text}");
        assert!(text.contains("☑"), "checked box:\n{text}");
        assert!(text.contains("[Enter]"), "hints:\n{text}");
    }
}
