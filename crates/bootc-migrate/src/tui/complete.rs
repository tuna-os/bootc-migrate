//! Complete screen — the success page with next-steps.
//!
//! Extracted from `tui.rs` (bootc-migrate#134): a pure renderer over the
//! shared theme/constants in `super`.

use super::*;

// ── Complete ──────────────────────────────────────────────────────────────────

pub fn render_complete(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(Span::styled(
            if app.opt_dry_run {
                " ✓ Dry-run Complete! "
            } else {
                " ✓ Migration Complete! "
            },
            Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(SUCCESS))
        .style(Style::default().bg(DARK_BG));

    if app.opt_dry_run {
        let text = Paragraph::new(format!(
            "\n  ✓ Dry-run completed. No deployment was staged.\n\n  Target: {}\n\n  To stage it, restart the wizard, turn off Dry-run\n  in Options, review the command, then type CONFIRM.\n\n  Press [q] or [Enter] to exit.",
            app.selected_image()
        ))
        .style(Style::default().fg(TEXT))
        .block(block)
        .wrap(Wrap { trim: false });
        f.render_widget(text, area);
        return;
    }

    if app.selected_choice().is_some_and(|c| c.backend == "ostree") {
        let message = "OSTree deployment staged. Reboot, then check bootc status.";
        let text = Paragraph::new(format!(
            "\n  {message}\n\n  Target: {}\n\n  Press [q] or [Enter] to exit.",
            app.selected_image()
        ))
        .style(Style::default().fg(TEXT))
        .block(block)
        .wrap(Wrap { trim: false });
        f.render_widget(text, area);
        return;
    }
    if app.is_image_swap() {
        let text = Paragraph::new(format!(
            "\n  ✓ Image swap staged.\n\n  Target: {}\n\n  Reboot to enter the new deployment. The previous\n  deployment remains available as a fallback.\n\n  Press [q] or [Enter] to exit.",
            app.selected_image()
        ))
        .style(Style::default().fg(TEXT))
        .block(block)
        .wrap(Wrap { trim: false });
        f.render_widget(text, area);
        return;
    }
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
        Line::raw(""),
        Line::from(Span::styled(
            "  Press [q] or [Enter] to exit.",
            Style::default().fg(MUTED),
        )),
    ]);

    let para = Paragraph::new(text).block(block).wrap(Wrap { trim: false });
    f.render_widget(para, area);
}
