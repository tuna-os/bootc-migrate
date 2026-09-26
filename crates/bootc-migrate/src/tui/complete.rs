//! Complete screen — the success page with next-steps.
//!
//! Extracted from `tui.rs` (bootc-migrate#134): a pure renderer over the
//! shared theme/constants in `super`.

use super::*;

// ── Complete ──────────────────────────────────────────────────────────────────

pub fn render_complete(f: &mut ratatui::Frame, area: Rect) {
    let block = Block::default()
        .title(Span::styled(
            " ✓ Migration Complete! ",
            Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(SUCCESS))
        .style(Style::default().bg(DARK_BG));

    let text = Text::from(vec![
        Line::raw(""),
        Line::from(Span::styled(
            "  ✓  Migration completed successfully!",
            Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            "  What to do next:",
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            "  1. Reboot to boot into the new ComposeFS deployment:",
            Style::default().fg(TEXT),
        )),
        Line::from(Span::styled(
            "       sudo systemctl reboot",
            Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            "  2. After reboot, validate ComposeFS is active:",
            Style::default().fg(TEXT),
        )),
        Line::from(Span::styled(
            "       cat /proc/cmdline | grep composefs=",
            Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            "  3. Check bootc status:",
            Style::default().fg(TEXT),
        )),
        Line::from(Span::styled(
            "       bootc status",
            Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            "  4. Commit the migration (removes OSTree artifacts):",
            Style::default().fg(TEXT),
        )),
        Line::from(Span::styled(
            "       sudo bootc-migrate commit",
            Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            "  ─────────────────────────────────────────────────────────",
            Style::default().fg(MUTED),
        )),
        Line::from(Span::styled(
            "  If the dry-run completed: re-run without --dry-run",
            Style::default().fg(AMBER),
        )),
        Line::from(Span::styled(
            "  to perform the actual migration.",
            Style::default().fg(AMBER),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            "  Press [q] or [Enter] to exit.",
            Style::default().fg(MUTED),
        )),
    ]);

    let para = Paragraph::new(text).block(block).wrap(Wrap { trim: false });
    f.render_widget(para, area);
}
