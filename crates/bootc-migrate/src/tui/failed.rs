//! Failed screen — the error page showing the last log lines and exit options.
//!
//! Extracted from `tui.rs` (bootc-migrate#134): a pure renderer over shared
//! `App` state via `super`.

use super::*;

// ── Failed ────────────────────────────────────────────────────────────────────

pub fn render_failed(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(Span::styled(
            " ✗ Migration Failed ",
            Style::default().fg(DANGER).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(DANGER))
        .style(Style::default().bg(DARK_BG));

    // Show last 10 log lines as excerpt
    let excerpt_start = app.log_lines.len().saturating_sub(10);
    let mut text_lines: Vec<Line> = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "  ✗  Migration did not complete successfully.",
            Style::default().fg(DANGER).add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            "  Last log output:",
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
    ];

    for ll in &app.log_lines[excerpt_start..] {
        let fg = match ll.kind {
            LogKind::Error => DANGER,
            LogKind::Header => TEAL,
            _ => SUBTLE,
        };
        text_lines.push(Line::from(Span::styled(
            format!("  {}", ll.text),
            Style::default().fg(fg),
        )));
    }

    text_lines.push(Line::raw(""));
    text_lines.push(Line::from(Span::styled(
        "  To undo partial migration artifacts, run:",
        Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
    )));
    text_lines.push(Line::from(Span::styled(
        "    sudo bootc-migrate undo",
        Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
    )));
    text_lines.push(Line::raw(""));
    text_lines.push(Line::from(Span::styled(
        "  Check the log at /var/log/bootc-migrate.log for details.",
        Style::default().fg(SUBTLE),
    )));
    text_lines.push(Line::raw(""));
    text_lines.push(Line::from(Span::styled(
        "  Press [q] or [Enter] to exit.",
        Style::default().fg(SUBTLE),
    )));

    let para = Paragraph::new(Text::from(text_lines))
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(para, area);
}
