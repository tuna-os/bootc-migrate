//! Review screen — the pre-execution confirmation: command line, target image,
//! and the toggled options, with the Begin button.
//!
//! Extracted from `tui.rs` (bootc-migrate#134): pure renderers over shared
//! `App` state via `super`.

use super::*;

// ── Review ────────────────────────────────────────────────────────────────────

pub fn render_review(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(Span::styled(
            " Step 5 · Review & Run ",
            Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(TEAL))
        .style(Style::default().bg(DARK_BG));

    let inner = block.inner(area);
    f.render_widget(block, area);

    // Split: text content above, button at bottom
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(8), Constraint::Length(3)])
        .split(inner);

    let mode_label = if app.opt_dry_run {
        Line::from(vec![
            Span::raw("  Mode:  "),
            Span::styled(
                "⚠ DRY-RUN",
                Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "  — no changes will actually be made",
                Style::default().fg(SUBTLE),
            ),
        ])
    } else {
        Line::from(vec![
            Span::raw("  Mode:  "),
            Span::styled(
                "⚠ LIVE MIGRATION",
                Style::default().fg(DANGER).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  — system will be modified!", Style::default().fg(AMBER)),
        ])
    };

    let cmd = app.command_display();
    let summary_lines = build_review_summary(app);

    let mut text_lines: Vec<Line> = vec![
        Line::raw(""),
        mode_label,
        Line::raw(""),
        Line::from(Span::styled(
            "  Command to be executed:",
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            format!("  $ {}", cmd),
            Style::default()
                .fg(TEAL)
                .bg(SURFACE)
                .add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            "  What will happen:",
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        )),
    ];

    for l in summary_lines {
        text_lines.push(l);
    }

    let para = Paragraph::new(Text::from(text_lines)).wrap(Wrap { trim: false });
    f.render_widget(para, chunks[0]);

    // ── RUN button ──
    let btn_layout = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(22),
            Constraint::Min(0),
        ])
        .split(chunks[1]);

    let (btn_label, btn_bg) = if app.opt_dry_run {
        ("  ▶ Run Dry-Run     ", TEAL)
    } else {
        ("  ⚡ Run Migration   ", Color::Rgb(40, 180, 70))
    };
    let btn = Paragraph::new(Line::from(Span::styled(
        btn_label,
        Style::default()
            .fg(DARK_BG)
            .bg(btn_bg)
            .add_modifier(Modifier::BOLD),
    )))
    .alignment(Alignment::Center);
    f.render_widget(btn, btn_layout[1]);
}

fn build_review_summary(app: &App) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let img = app.selected_image();
    lines.push(Line::from(Span::styled(
        format!("  • Migrate to image: {img}"),
        Style::default().fg(TEXT),
    )));
    if app.opt_skip_import {
        lines.push(Line::from(Span::styled(
            "  • Phase 1 OSTree import will be skipped",
            Style::default().fg(AMBER),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "  • Phase 1: Import OSTree objects into ComposeFS store",
            Style::default().fg(TEXT),
        )));
    }
    lines.push(Line::from(Span::styled(
        "  • Phase 2: Pull OCI image layers",
        Style::default().fg(TEXT),
    )));
    lines.push(Line::from(Span::styled(
        "  • Phase 3: Seal EROFS ComposeFS image",
        Style::default().fg(TEXT),
    )));
    lines.push(Line::from(Span::styled(
        "  • Phase 4: Stage deployment state",
        Style::default().fg(TEXT),
    )));
    lines.push(Line::from(Span::styled(
        "  • Phase 5: Configure bootloader",
        Style::default().fg(TEXT),
    )));
    let bl = match app.opt_bootloader {
        Bootloader::SystemdBoot => "systemd-boot",
        Bootloader::Grub2 => "grub2",
    };
    lines.push(Line::from(Span::styled(
        format!("  • Bootloader: {bl}"),
        Style::default().fg(TEXT),
    )));
    if app.opt_force {
        lines.push(Line::from(Span::styled(
            "  • ⚠ Force mode: non-fatal warnings will be ignored",
            Style::default().fg(AMBER),
        )));
    }
    if app.opt_skip_preflight {
        lines.push(Line::from(Span::styled(
            "  • ⚠ Preflight checks are SKIPPED",
            Style::default().fg(DANGER),
        )));
    }
    lines
}
