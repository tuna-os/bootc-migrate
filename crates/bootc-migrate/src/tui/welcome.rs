//! Welcome screen — the title-screen renderer with the "BMC" BigText logo,
//! prerequisites list, and Begin-Migration button.
//!
//! Extracted from `tui.rs` (bootc-migrate#135): a pure renderer over the shared
//! theme/constants in `super`.

use super::*;

// ── Welcome ───────────────────────────────────────────────────────────────────

fn render_welcome(f: &mut ratatui::Frame, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(TEAL))
        .style(Style::default().bg(DARK_BG));

    let inner = block.inner(area);
    f.render_widget(block, area);

    // Layout: BigText header, spacer, description, button
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4), // BigText "BMC"
            Constraint::Length(1), // subtitle
            Constraint::Length(1), // spacer
            Constraint::Min(10),   // description + prerequisites
            Constraint::Length(3), // Start button
        ])
        .split(inner);

    // ── BigText logo ──
    let big_text = BigText::builder()
        .pixel_size(PixelSize::HalfHeight)
        .style(Style::default().fg(TEAL))
        .lines(vec!["BMC".into()])
        .centered()
        .build();
    f.render_widget(big_text, chunks[0]);

    // ── Subtitle ──
    let subtitle = Paragraph::new(Line::from(vec![
        Span::styled(
            "  bootc-migrate",
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "  — OSTree → ComposeFS in-place migration",
            Style::default().fg(SUBTLE),
        ),
    ]))
    .alignment(Alignment::Center);
    f.render_widget(subtitle, chunks[1]);

    // ── Description ──
    let text = Text::from(vec![
        Line::raw(""),
        Line::from(Span::styled(
            "  Prerequisites",
            Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "  ─────────────────────────────────────────────────────",
            Style::default().fg(SUBTLE),
        )),
        Line::from(Span::styled(
            "  • Root privileges (sudo)",
            Style::default().fg(TEXT),
        )),
        Line::from(Span::styled(
            "  • Booted OSTree-backed system (Bluefin, Aurora, Silverblue…)",
            Style::default().fg(TEXT),
        )),
        Line::from(Span::styled(
            "  • ≥ 1.1× OSTree repo size in free disk space",
            Style::default().fg(TEXT),
        )),
        Line::from(Span::styled(
            "  • Network access (OCI image pull)",
            Style::default().fg(TEXT),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            "  ⚠  This modifies bootloader state. Back up first!",
            Style::default().fg(AMBER),
        )),
        Line::from(Span::styled(
            "  Default mode is --dry-run (no changes made).",
            Style::default().fg(SUBTLE),
        )),
    ]);
    let para = Paragraph::new(text).wrap(Wrap { trim: false });
    f.render_widget(para, chunks[3]);

    // ── Start button (gitui style: inverted color block) ──
    let btn_area = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(24),
            Constraint::Min(0),
        ])
        .split(chunks[4]);
    let btn = Paragraph::new(Line::from(Span::styled(
        "  ▶ Begin Migration   ",
        Style::default()
            .fg(DARK_BG)
            .bg(TEAL)
            .add_modifier(Modifier::BOLD),
    )))
    .alignment(Alignment::Center);
    f.render_widget(btn, btn_area[1]);
}
