//! Interactive "Boot Entry Cleanup" checklist (issue #31).
//!
//! Presents [`bootc_migrate_core::boot_audit`]'s classification as a
//! checkable list and returns a
//! [`CleanupSelection`] for
//! [`bootc_migrate_core::boot_cleanup::plan::plan_cleanup`] to turn into
//! operations — nothing here mutates anything, and nothing here decides
//! what is safe. Keybindings and layout deliberately match
//! `bootc-migrate`'s `/etc` drift review (`drift_review.rs`, issue #15) so
//! the two checklists feel like one tool.
//!
//! The one safety property this module owns is that a protected entry is
//! **unselectable**, not merely unselected: [`ReviewState::handle_key`]
//! refuses to check it at all, and "select all dead" skips it. The planner
//! would reject such a selection anyway, but a checklist that lets a user
//! tick "Fedora (rollback path)" and only complains at the end teaches the
//! wrong thing about what this tool will do.
//!
//! The terminal event loop itself carries no unit tests — driving a real
//! terminal isn't something a compile+unit-test loop can prove, the same
//! policy this project applies to its other interactive-only work (see
//! ROADMAP.md's decision log for #65/#31/#15, and AGENTS.md's "Interactive
//! testing with Corral VMs" for how to validate it by hand). The state
//! machine underneath it is pure and *is* tested.

use anyhow::{Context, Result};
use bootc_migrate_core::boot_audit::{AuditFlag, AuditedEntry};
use bootc_migrate_core::boot_cleanup::plan::{
    CleanupSelection, DeleteProtection, NvramFacts, RenameBlock, delete_protection, rename_block,
};
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
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};
use std::time::Duration;

// Palette shared with bootc-migrate's drift review, so the two checklists
// are visually the same tool.
const TEAL: Color = Color::Rgb(0, 180, 180);
const AMBER: Color = Color::Rgb(220, 160, 0);
const DANGER: Color = Color::Rgb(220, 60, 60);
const DARK_BG: Color = Color::Rgb(18, 20, 24);
const SURFACE: Color = Color::Rgb(30, 34, 42);
const SUBTLE: Color = Color::Rgb(90, 100, 115);
const SUCCESS: Color = Color::Rgb(80, 200, 100);
const TEXT: Color = Color::Rgb(210, 215, 225);

/// Width the entry label is padded to in the list, so flags line up.
const LABEL_COLUMN_WIDTH: usize = 34;

/// One row of the checklist: an audited entry plus everything the user
/// needs to decide about it, pre-computed so rendering never calls the
/// planner.
#[derive(Debug, Clone)]
pub struct EntryRow {
    pub id: String,
    pub label: String,
    pub loader_path: Option<String>,
    pub flags: Vec<AuditFlag>,
    /// `Some` when this entry may never be deleted.
    pub protection: Option<DeleteProtection>,
    /// The branding rename available for this entry, if any.
    pub rename_to: Option<String>,
    /// Why no rename is offered, when none is.
    pub rename_block: Option<RenameBlock>,
    pub delete_checked: bool,
    pub rename_checked: bool,
}

impl EntryRow {
    /// Whether the user may check this row for deletion at all.
    pub fn deletable(&self) -> bool {
        self.protection.is_none()
    }

    /// Whether the user may check this row for renaming at all.
    pub fn renameable(&self) -> bool {
        self.rename_to.is_some()
    }

    /// Pre-selected by default: clearly dead and unprotected. Duplicates
    /// and generic labels are shown but never pre-checked (issue #31).
    fn preselect(&self) -> bool {
        self.deletable() && self.flags.contains(&AuditFlag::Dead)
    }
}

/// Build the checklist rows. `pretty_name` is `/etc/os-release`'s
/// `PRETTY_NAME`; pass `None` to offer no renames at all.
pub fn build_rows(
    audited: &[AuditedEntry],
    facts: &NvramFacts,
    pretty_name: Option<&str>,
) -> Vec<EntryRow> {
    audited
        .iter()
        .map(|a| {
            let proposed = pretty_name.map(str::trim).filter(|n| !n.is_empty());
            let (rename_to, rename_block) = match proposed {
                Some(name) => match rename_block(a, name) {
                    Some(block) => (None, Some(block)),
                    None => (Some(name.to_string()), None),
                },
                None => (None, None),
            };
            let mut row = EntryRow {
                id: a.entry.id.clone(),
                label: a.entry.label.clone(),
                loader_path: a.entry.loader_path.clone(),
                flags: a.flags.clone(),
                protection: delete_protection(a, facts),
                rename_to,
                rename_block,
                delete_checked: false,
                rename_checked: false,
            };
            row.delete_checked = row.preselect();
            row
        })
        .collect()
}

/// The checklist's state machine. Pure: every transition is a method, so
/// the safety-relevant ones are unit-tested without a terminal.
#[derive(Debug)]
pub struct ReviewState {
    pub rows: Vec<EntryRow>,
    pub list_state: ListState,
    pub cancelled: bool,
}

impl ReviewState {
    pub fn new(rows: Vec<EntryRow>) -> Self {
        let mut list_state = ListState::default();
        if !rows.is_empty() {
            list_state.select(Some(0));
        }
        Self {
            rows,
            list_state,
            cancelled: false,
        }
    }

    /// The selection to hand the planner.
    pub fn selection(&self) -> CleanupSelection {
        CleanupSelection {
            delete_ids: self
                .rows
                .iter()
                .filter(|r| r.delete_checked)
                .map(|r| r.id.clone())
                .collect(),
            renames: self
                .rows
                .iter()
                .filter(|r| r.rename_checked)
                .filter_map(|r| r.rename_to.clone().map(|to| (r.id.clone(), to)))
                .collect(),
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let len = self.rows.len() as isize;
        let cur = self.list_state.selected().unwrap_or(0) as isize;
        let next = (cur + delta).rem_euclid(len);
        self.list_state.select(Some(next as usize));
    }

    /// Toggle the highlighted row's delete checkbox. A protected entry
    /// cannot be checked at all — this is the "un-selectable, not merely
    /// unselected" requirement.
    fn toggle_delete(&mut self) {
        if let Some(i) = self.list_state.selected()
            && self.rows[i].deletable()
        {
            self.rows[i].delete_checked = !self.rows[i].delete_checked;
            // Delete and rename are mutually exclusive; the planner
            // rejects a row selected for both, so don't let the UI build
            // one.
            if self.rows[i].delete_checked {
                self.rows[i].rename_checked = false;
            }
        }
    }

    /// Toggle the highlighted row's rename checkbox, when a rename is on
    /// offer for it.
    fn toggle_rename(&mut self) {
        if let Some(i) = self.list_state.selected()
            && self.rows[i].renameable()
        {
            self.rows[i].rename_checked = !self.rows[i].rename_checked;
            if self.rows[i].rename_checked {
                self.rows[i].delete_checked = false;
            }
        }
    }

    /// Re-apply the safe default: every clearly-dead unprotected entry
    /// checked, everything else unchecked. Deliberately *not* "check
    /// everything selectable" — a select-all over a NVRAM whose ESP was
    /// misread is exactly the failure mode this feature has to not have.
    fn select_dead(&mut self) {
        for row in &mut self.rows {
            row.delete_checked = row.preselect();
            if row.delete_checked {
                row.rename_checked = false;
            }
        }
    }

    fn select_none(&mut self) {
        for row in &mut self.rows {
            row.delete_checked = false;
            row.rename_checked = false;
        }
    }

    /// Handle one key press. Returns `true` when the review loop should end.
    pub fn handle_key(&mut self, key: KeyCode) -> bool {
        match key {
            KeyCode::Up | KeyCode::Char('k') => self.move_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(1),
            KeyCode::Char(' ') => self.toggle_delete(),
            KeyCode::Char('r') => self.toggle_rename(),
            KeyCode::Char('a') => self.select_dead(),
            KeyCode::Char('n') => self.select_none(),
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

fn flag_label(flag: AuditFlag) -> (&'static str, Color) {
    match flag {
        AuditFlag::Dead => ("dead", DANGER),
        AuditFlag::GenericLabel => ("generic-label", AMBER),
        AuditFlag::DuplicateLoaderPath => ("duplicate", AMBER),
        AuditFlag::FirmwareManaged => ("firmware", TEAL),
    }
}

/// The detail pane's explanation of the highlighted row: why the audit
/// flagged it, whether it can be removed or renamed, and why not.
fn detail_lines(row: &EntryRow) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("Loader: ", Style::default().fg(SUBTLE)),
        Span::styled(
            row.loader_path
                .clone()
                .unwrap_or_else(|| "(none — firmware-internal device path)".to_string()),
            Style::default().fg(TEXT),
        ),
    ]));

    if row.flags.is_empty() {
        lines.push(Line::from(Span::styled(
            "Classified: nothing flagged — this entry looks healthy.",
            Style::default().fg(SUCCESS),
        )));
    } else {
        for flag in &row.flags {
            let (name, color) = flag_label(*flag);
            let why = match flag {
                AuditFlag::Dead => "its loader file is not present on this ESP",
                AuditFlag::GenericLabel => "its label doesn't name a distribution",
                AuditFlag::DuplicateLoaderPath => "another entry points at the same loader",
                AuditFlag::FirmwareManaged => "the firmware owns this entry",
            };
            lines.push(Line::from(vec![
                Span::styled(format!("Classified {name}: "), Style::default().fg(color)),
                Span::styled(why, Style::default().fg(TEXT)),
            ]));
        }
    }

    match row.protection {
        Some(p) => lines.push(Line::from(vec![
            Span::styled(
                "Protected: ",
                Style::default().fg(DANGER).add_modifier(Modifier::BOLD),
            ),
            Span::styled(p.describe(), Style::default().fg(TEXT)),
        ])),
        None => lines.push(Line::from(Span::styled(
            "Removable: no protection applies to this entry.",
            Style::default().fg(SUBTLE),
        ))),
    }

    match (&row.rename_to, row.rename_block) {
        (Some(to), _) => lines.push(Line::from(vec![
            Span::styled("Rename available: ", Style::default().fg(SUBTLE)),
            Span::styled(format!("{to:?}"), Style::default().fg(TEXT)),
            Span::styled(
                "  (delete + recreate against NVRAM)",
                Style::default().fg(AMBER),
            ),
        ])),
        (None, Some(block)) => lines.push(Line::from(vec![
            Span::styled("No rename offered: ", Style::default().fg(SUBTLE)),
            Span::styled(block.describe(), Style::default().fg(TEXT)),
        ])),
        (None, None) => lines.push(Line::from(Span::styled(
            "No rename offered: run with --rename-branding to propose one.",
            Style::default().fg(SUBTLE),
        ))),
    }

    lines
}

fn render(f: &mut ratatui::Frame, state: &mut ReviewState) {
    let area = f.area();
    f.render_widget(Block::default().style(Style::default().bg(DARK_BG)), area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(7),
            Constraint::Length(3),
        ])
        .split(area);

    let checked = state.rows.iter().filter(|r| r.delete_checked).count();
    let renamed = state.rows.iter().filter(|r| r.rename_checked).count();
    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            " UEFI Boot Entry Cleanup ",
            Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "({} entries · {checked} to delete · {renamed} to rename)",
                state.rows.len()
            ),
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
        .rows
        .iter()
        .map(|row| {
            let (box_char, box_color) = if !row.deletable() {
                ("🔒", SUBTLE)
            } else if row.delete_checked {
                ("☑", DANGER)
            } else {
                ("☐", SUBTLE)
            };
            let mut spans = vec![
                Span::styled(format!("  {box_char} "), Style::default().fg(box_color)),
                Span::styled(
                    format!("Boot{} ", row.id),
                    Style::default().fg(SUBTLE).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{:<LABEL_COLUMN_WIDTH$}", row.label),
                    Style::default().fg(TEXT),
                ),
            ];
            for flag in &row.flags {
                let (name, color) = flag_label(*flag);
                spans.push(Span::styled(
                    format!("[{name}] "),
                    Style::default().fg(color),
                ));
            }
            if row.rename_checked
                && let Some(to) = &row.rename_to
            {
                spans.push(Span::styled(
                    format!("→ rename to {to:?}"),
                    Style::default().fg(SUCCESS),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(TEAL))
                .title(" ☑ delete   ☐ keep   🔒 protected, cannot be deleted ")
                .style(Style::default().bg(DARK_BG)),
        )
        .highlight_style(Style::default().bg(SURFACE));
    f.render_stateful_widget(list, chunks[1], &mut state.list_state);

    let detail = state
        .list_state
        .selected()
        .and_then(|i| state.rows.get(i))
        .map(detail_lines)
        .unwrap_or_default();
    let detail = Paragraph::new(detail).wrap(Wrap { trim: true }).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(SUBTLE))
            .title(" Why ")
            .style(Style::default().bg(DARK_BG)),
    );
    f.render_widget(detail, chunks[2]);

    let hint = |key: &'static str, what: &'static str| {
        [
            Span::styled(key, Style::default().fg(TEAL).add_modifier(Modifier::BOLD)),
            Span::styled(what, Style::default().fg(TEXT)),
        ]
    };
    let hints = Paragraph::new(Line::from(
        [
            hint("[↑↓/jk] ", "Move  "),
            hint("[Space] ", "Delete  "),
            hint("[r] ", "Rename  "),
            hint("[a] ", "Only dead  "),
            hint("[n] ", "None  "),
            hint("[Enter] ", "Continue  "),
            hint("[Esc/q] ", "Cancel"),
        ]
        .concat(),
    ))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(SUBTLE))
            .style(Style::default().bg(SURFACE)),
    );
    f.render_widget(hints, chunks[3]);
}

/// Run the interactive checklist and return the user's selection, or
/// `None` if they cancelled (Esc/q). An empty audit is a no-op.
pub fn run_review(rows: Vec<EntryRow>) -> Result<Option<CleanupSelection>> {
    if rows.is_empty() {
        println!("No UEFI boot entries to review.");
        return Ok(Some(CleanupSelection::default()));
    }

    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen).context("enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create terminal")?;

    let mut state = ReviewState::new(rows);
    let run_result = event_loop(&mut terminal, &mut state);

    // Restore the terminal regardless of how the loop ended.
    disable_raw_mode().context("disable raw mode")?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen).context("leave alternate screen")?;
    terminal.show_cursor().context("show cursor")?;

    run_result?;

    if state.cancelled {
        return Ok(None);
    }
    Ok(Some(state.selection()))
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
            && state.handle_key(key.code)
        {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bootc_migrate_core::boot_audit::BootEntry;

    fn audited(id: &str, label: &str, loader: Option<&str>, flags: &[AuditFlag]) -> AuditedEntry {
        AuditedEntry {
            entry: BootEntry {
                id: id.to_string(),
                label: label.to_string(),
                active: true,
                loader_path: loader.map(str::to_string),
            },
            flags: flags.to_vec(),
        }
    }

    /// Booted systemd-boot entry, the OSTree rollback entry, a dead
    /// leftover, a live generic-label entry, and a firmware entry.
    fn sample() -> Vec<AuditedEntry> {
        vec![
            audited(
                "0001",
                "Linux Boot Manager",
                Some("\\EFI\\systemd\\systemd-bootx64.efi"),
                &[],
            ),
            audited("0002", "Fedora", Some("\\EFI\\fedora\\shimx64.efi"), &[]),
            audited(
                "0003",
                "Old Ubuntu",
                Some("\\EFI\\ubuntu\\shimx64.efi"),
                &[AuditFlag::Dead],
            ),
            audited(
                "0004",
                "UEFI OS",
                Some("\\EFI\\BOOT\\BOOTX64.EFI"),
                &[AuditFlag::GenericLabel],
            ),
            audited("0005", "UEFI: PXEv4", None, &[AuditFlag::FirmwareManaged]),
        ]
    }

    fn facts() -> NvramFacts {
        NvramFacts {
            boot_current: Some("0001".to_string()),
            boot_order: Some("0001,0002,0003,0004,0005".to_string()),
            rollback_entry_id: Some("0002".to_string()),
        }
    }

    fn state_with_renames() -> ReviewState {
        ReviewState::new(build_rows(&sample(), &facts(), Some("Dakota")))
    }

    /// Move the cursor onto the row with this id.
    fn focus(state: &mut ReviewState, id: &str) {
        let i = state.rows.iter().position(|r| r.id == id).unwrap();
        state.list_state.select(Some(i));
    }

    #[test]
    fn rows_start_with_only_clearly_dead_entries_checked() {
        let state = state_with_renames();
        // (id, deletable, renameable, checked by default)
        let expected: &[(&str, bool, bool, bool)] = &[
            // BootCurrent: protected, but a rename is still on offer
            // (create-before-delete keeps it bootable throughout).
            ("0001", false, true, false),
            // Rollback path: protected from deletion.
            ("0002", false, true, false),
            // Dead: the one pre-selected row. Not renameable — a dead
            // loader must be deleted, not repointed at our ESP.
            ("0003", true, false, true),
            // Generic label, alive: renameable, but never pre-checked.
            ("0004", true, true, false),
            // Firmware: neither.
            ("0005", false, false, false),
        ];
        for (id, deletable, renameable, checked) in expected {
            let row = state.rows.iter().find(|r| r.id == *id).unwrap();
            assert_eq!(row.deletable(), *deletable, "Boot{id} deletable");
            assert_eq!(row.renameable(), *renameable, "Boot{id} renameable");
            assert_eq!(row.delete_checked, *checked, "Boot{id} pre-checked");
            assert!(!row.rename_checked, "Boot{id} rename pre-checked");
        }
        assert_eq!(state.selection().delete_ids, vec!["0003".to_string()]);
        assert!(state.selection().renames.is_empty());
    }

    #[test]
    fn no_renames_are_offered_without_a_pretty_name() {
        // (PRETTY_NAME, whether Boot0004 gets a rename proposal)
        for (pretty, expected) in [
            (None, false),
            (Some(""), false),
            (Some("   "), false),
            (Some("Dakota"), true),
        ] {
            let rows = build_rows(&sample(), &facts(), pretty);
            let row = rows.iter().find(|r| r.id == "0004").unwrap();
            assert_eq!(row.renameable(), expected, "PRETTY_NAME={pretty:?}");
        }
    }

    #[test]
    fn protected_entries_cannot_be_checked_by_any_key() {
        let mut state = state_with_renames();
        // (protected id, key that must not check it)
        for id in ["0001", "0002", "0005"] {
            for key in [KeyCode::Char(' '), KeyCode::Char('a')] {
                focus(&mut state, id);
                assert!(!state.handle_key(key), "{key:?} should not end the loop");
                let row = state.rows.iter().find(|r| r.id == id).unwrap();
                assert!(
                    !row.delete_checked,
                    "Boot{id} became checked via {key:?} despite being protected"
                );
            }
        }
        assert!(
            !state
                .selection()
                .delete_ids
                .iter()
                .any(|id| ["0001", "0002", "0005"].contains(&id.as_str()))
        );
    }

    #[test]
    fn key_handling_cases() {
        let mut state = state_with_renames();
        focus(&mut state, "0004");

        /// `(key pressed, expected delete ids, expected renames, expected quit)`
        type KeyCase = (
            KeyCode,
            &'static [&'static str],
            &'static [(&'static str, &'static str)],
            bool,
        );
        let cases: &[KeyCase] = &[
            // Space checks the focused unprotected row.
            (KeyCode::Char(' '), &["0003", "0004"], &[], false),
            // Space again unchecks it.
            (KeyCode::Char(' '), &["0003"], &[], false),
            // r stages the branding rename for the focused row.
            (KeyCode::Char('r'), &["0003"], &[("0004", "Dakota")], false),
            // Space on a rename-staged row swaps it to a deletion.
            (KeyCode::Char(' '), &["0003", "0004"], &[], false),
            // r swaps it back — the two are mutually exclusive.
            (KeyCode::Char('r'), &["0003"], &[("0004", "Dakota")], false),
            // n clears everything, renames included.
            (KeyCode::Char('n'), &[], &[], false),
            // a restores the safe default only (not "everything").
            (KeyCode::Char('a'), &["0003"], &[], false),
            // Enter ends the loop.
            (KeyCode::Enter, &["0003"], &[], true),
        ];
        for (key, delete_ids, renames, quit) in cases {
            assert_eq!(state.handle_key(*key), *quit, "{key:?} quit");
            let selection = state.selection();
            let want_deletes: Vec<String> = delete_ids.iter().map(|s| s.to_string()).collect();
            let want_renames: Vec<(String, String)> = renames
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect();
            assert_eq!(selection.delete_ids, want_deletes, "{key:?} deletes");
            assert_eq!(selection.renames, want_renames, "{key:?} renames");
        }
        assert!(!state.cancelled);
    }

    #[test]
    fn rename_key_does_nothing_when_no_rename_is_offered() {
        let mut state = state_with_renames();
        // Boot0003 is dead (not renameable), Boot0005 is firmware-managed.
        for id in ["0003", "0005"] {
            focus(&mut state, id);
            state.handle_key(KeyCode::Char('r'));
            let row = state.rows.iter().find(|r| r.id == id).unwrap();
            assert!(!row.rename_checked, "Boot{id} staged an unavailable rename");
        }
        assert!(state.selection().renames.is_empty());
    }

    #[test]
    fn escape_cancels_and_wraps_the_cursor() {
        let mut state = state_with_renames();
        // Cursor wraps in both directions rather than sticking at an end.
        assert!(!state.handle_key(KeyCode::Up));
        assert_eq!(state.list_state.selected(), Some(4));
        assert!(!state.handle_key(KeyCode::Down));
        assert_eq!(state.list_state.selected(), Some(0));

        assert!(state.handle_key(KeyCode::Esc));
        assert!(state.cancelled);
    }

    #[test]
    fn empty_audit_yields_an_empty_selection_without_panicking() {
        let mut state = ReviewState::new(Vec::new());
        assert_eq!(state.list_state.selected(), None);
        for key in [
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Char(' '),
            KeyCode::Char('r'),
            KeyCode::Char('a'),
            KeyCode::Char('n'),
        ] {
            assert!(!state.handle_key(key));
        }
        assert!(state.selection().is_empty());
    }
}
