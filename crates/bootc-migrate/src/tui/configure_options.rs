//! Configure Options screen — the toggle grid for dry-run, skip-import,
//! bootloader, skip-preflight, force, and accept-cross-base flags with
//! cursor navigation.
//!
//! Extracted from `tui.rs` (bootc-migrate#133): a pure renderer over `App`
//! state via `super`.

use super::*;

// ── Configure options ─────────────────────────────────────────────────────────

pub fn render_configure_options(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(Span::styled(
            format!(" Step {} · Configure Options ", app.current_step()),
            Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(TEAL))
        .style(Style::default().bg(DARK_BG));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let check = |on: bool| if on { "[x]" } else { "[ ]" }.to_owned();
    let options: Vec<(&str, String, bool)> = app
        .option_rows()
        .iter()
        .map(|row| match row {
            OptRow::DryRun => (
                "Dry-run (recommended first run)",
                check(app.opt_dry_run),
                app.opt_dry_run,
            ),
            OptRow::SkipImport => (
                "Skip Phase 1 OSTree import (faster, less dedup)",
                check(app.opt_skip_import),
                app.opt_skip_import,
            ),
            OptRow::Bootloader => (
                "Bootloader",
                match app.opt_bootloader {
                    Bootloader::SystemdBoot => "[systemd-boot ●] [grub2 ○]".to_owned(),
                    Bootloader::Grub2 => "[systemd-boot ○] [grub2 ●]".to_owned(),
                },
                false,
            ),
            OptRow::SkipPreflight => (
                "Skip preflight checks (⚠ not recommended)",
                check(app.opt_skip_preflight),
                app.opt_skip_preflight,
            ),
            OptRow::Force => (
                "Force (ignore non-fatal warnings)",
                check(app.opt_force),
                app.opt_force,
            ),
            OptRow::AcceptCrossBase => (
                "Accept a cross-family target (⚠ target /etc wins)",
                check(app.opt_accept_cross_base),
                app.opt_accept_cross_base,
            ),
        })
        .collect();

    let mut lines: Vec<Line> = vec![Line::raw("")];
    for (i, (label, value, _active)) in options.iter().enumerate() {
        let selected = i == app.options_cursor;
        let prefix = if selected { "▶ " } else { "  " };
        let fg = if selected { TEXT } else { MUTED };
        let value_fg = if selected { TEAL } else { MUTED };
        let is_warning = label.contains('⚠');
        let label_fg = if is_warning { AMBER } else { fg };

        let line = Line::from(vec![
            Span::styled(
                prefix,
                Style::default().fg(if selected { TEAL } else { MUTED }),
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
