//! Configure Options screen — the toggle grid for dry-run, skip-import,
//! bootloader, skip-preflight, and force flags with cursor navigation.
//!
//! Extracted from `tui.rs` (bootc-migrate#133): a pure renderer over `App`
//! state via `super`.

use super::*;

// ── Configure options ─────────────────────────────────────────────────────────

pub fn render_configure_options(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(Span::styled(
            " Step 4 · Configure Options ",
            Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(TEAL))
        .style(Style::default().bg(DARK_BG));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let options: Vec<(&str, String, bool)> = vec![
        (
            "Dry-run (recommended first run)",
            if app.opt_dry_run {
                "[x]".to_owned()
            } else {
                "[ ]".to_owned()
            },
            app.opt_dry_run,
        ),
        (
            "Skip Phase 1 OSTree import (faster, less dedup)",
            if app.opt_skip_import {
                "[x]".to_owned()
            } else {
                "[ ]".to_owned()
            },
            app.opt_skip_import,
        ),
        (
            "Bootloader",
            match app.opt_bootloader {
                Bootloader::SystemdBoot => "[systemd-boot ●] [grub2 ○]".to_owned(),
                Bootloader::Grub2 => "[systemd-boot ○] [grub2 ●]".to_owned(),
            },
            false,
        ),
        (
            "Skip preflight checks (⚠ not recommended)",
            if app.opt_skip_preflight {
                "[x]".to_owned()
            } else {
                "[ ]".to_owned()
            },
            app.opt_skip_preflight,
        ),
        (
            "Force (ignore non-fatal warnings)",
            if app.opt_force {
                "[x]".to_owned()
            } else {
                "[ ]".to_owned()
            },
            app.opt_force,
        ),
    ];

    let mut lines: Vec<Line> = vec![Line::raw("")];
    for (i, (label, value, _active)) in options.iter().enumerate() {
        let selected = i == app.options_cursor;
        let prefix = if selected { "▶ " } else { "  " };
        let fg = if selected { TEXT } else { SUBTLE };
        let value_fg = if selected { TEAL } else { SUBTLE };
        let is_warning = label.contains('⚠');
        let label_fg = if is_warning { AMBER } else { fg };

        let line = Line::from(vec![
            Span::styled(
                prefix,
                Style::default().fg(if selected { TEAL } else { SUBTLE }),
            ),
            Span::styled(
                format!("{:<48}", label),
                Style::default().fg(label_fg).add_modifier(if selected {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
            ),
            Span::styled(
                value.as_str(),
                Style::default().fg(value_fg).add_modifier(Modifier::BOLD),
            ),
        ]);
        lines.push(line);
        lines.push(Line::raw(""));
    }

    let para = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    f.render_widget(para, inner);
}
