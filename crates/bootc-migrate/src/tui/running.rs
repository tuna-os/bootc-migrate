//! Running screen — live migration progress: phase status list, spinner,
//! and the scrollable log panel.
//!
//! Extracted from `tui.rs` (bootc-migrate#134): pure renderers over shared
//! `App` state via `super`.

use super::*;

// ── Running ───────────────────────────────────────────────────────────────────

pub fn render_running(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(area);

    render_phase_list(f, app, chunks[0]);
    render_log_panel(f, app, chunks[1]);
}

fn render_phase_list(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(Span::styled(
            " Phases ",
            Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(TEAL))
        .style(Style::default().bg(DARK_BG));

    let spinner_ch = app.spinner_char();

    let items: Vec<ListItem> = app
        .phases
        .iter()
        .map(|p| {
            let (icon, fg) = match p.status {
                PhaseStatus::Pending => ("○", MUTED),
                PhaseStatus::Running => ("⟳", TEAL),
                PhaseStatus::Done => ("✓", SUCCESS),
                PhaseStatus::Failed => ("✗", DANGER),
                PhaseStatus::Skipped => ("⊘", AMBER),
            };

            let mut spans = vec![
                Span::styled(
                    format!(" {} ", icon),
                    Style::default().fg(fg).add_modifier(Modifier::BOLD),
                ),
                Span::styled(p.label, Style::default().fg(fg)),
            ];

            if p.status == PhaseStatus::Running {
                spans.push(Span::styled(
                    format!(" {}", spinner_ch),
                    Style::default().fg(TEAL),
                ));
            }

            ListItem::new(Line::from(spans))
        })
        .collect();

    let footer_text = if app.migration_done {
        if app.migration_success {
            Span::styled(
                " ✓ Complete — press Enter ",
                Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(
                " ✗ Failed ",
                Style::default().fg(DANGER).add_modifier(Modifier::BOLD),
            )
        }
    } else {
        Span::styled(" Running… ", Style::default().fg(TEAL))
    };

    let list = List::new(items).block(block);
    f.render_widget(list, area);

    // Overlay footer at bottom of the phase panel
    let footer_area = Rect {
        x: area.x + 1,
        y: area.y + area.height - 2,
        width: area.width - 2,
        height: 1,
    };
    f.render_widget(Paragraph::new(Line::from(footer_text)), footer_area);
}

fn render_log_panel(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let block = Block::default()
        .title(Span::styled(
            " Live Output ",
            Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BORDER))
        .style(Style::default().bg(DARK_BG));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let visible_height = inner.height as usize;
    let total = app.log_lines.len();

    // Clamp scroll
    if app.log_scroll >= total && total > 0 {
        app.log_scroll = total - 1;
    }

    let start = if total > visible_height {
        app.log_scroll.min(total - visible_height)
    } else {
        0
    };
    let end = (start + visible_height).min(total);

    let lines: Vec<Line> = app.log_lines[start..end]
        .iter()
        .map(|l| {
            let style = match l.kind {
                LogKind::Header => Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
                LogKind::Phase => Style::default().fg(MUTED),
                LogKind::Error => Style::default().fg(DANGER),
                LogKind::Success => Style::default().fg(SUCCESS),
                LogKind::Normal => Style::default().fg(TEXT),
            };
            Line::from(Span::styled(l.text.as_str(), style))
        })
        .collect();

    let para = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    f.render_widget(para, inner);

    // Scrollbar
    if total > visible_height {
        let scrollbar = Scrollbar::default()
            .orientation(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("↑"))
            .end_symbol(Some("↓"));
        let mut scrollbar_state = ScrollbarState::new(total).position(start);
        f.render_stateful_widget(scrollbar, area, &mut scrollbar_state);
    }
}
