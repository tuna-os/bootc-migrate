//! Select Image screen — the target list, custom-image entry and inline help.
//!
//! The rows come from `super::image_choices`, built from what preflight
//! detected, so this file only renders; it makes no decision about which
//! targets exist or which one is right.

use super::*;

// ── Select image ──────────────────────────────────────────────────────────────

pub fn render_select_image(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(6),
            Constraint::Length(5),
        ])
        .split(area);

    // What we detected, and therefore why the first row is what it is.
    let os_hint_block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(BORDER))
        .style(Style::default().bg(DARK_BG));
    let mut header = vec![
        Span::styled("  Detected: ", Style::default().fg(MUTED)),
        Span::styled(
            app.detected_os.as_str(),
            Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(backend) = app.booted_backend {
        header.push(Span::styled(
            format!(" · {backend}"),
            Style::default().fg(MUTED),
        ));
    }
    if let Some(image) = app.booted_image.as_deref() {
        header.push(Span::styled(" · ", Style::default().fg(MUTED)));
        header.push(Span::styled(image, Style::default().fg(MUTED)));
    }

    let os_line = Paragraph::new(Line::from(header)).block(os_hint_block);
    f.render_widget(os_line, chunks[0]);

    let block = Block::default()
        .title(Span::styled(
            " Step 3 · Select Target Image ",
            Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(TEAL))
        .style(Style::default().bg(DARK_BG));

    let label_width = app
        .image_choices
        .iter()
        .map(|c| c.label.chars().count())
        .max()
        .unwrap_or(20)
        .max(20);

    let items: Vec<ListItem> = app
        .image_choices
        .iter()
        .enumerate()
        .map(|(i, choice)| {
            let selected = app.image_list_state.selected() == Some(i);
            let target_display = if choice.custom {
                if app.custom_image.is_empty() {
                    "<type your image reference>".to_owned()
                } else {
                    app.custom_image.clone()
                }
            } else {
                choice.image.clone()
            };
            let line = Line::from(vec![
                Span::styled(
                    if selected { "▶ " } else { "  " },
                    Style::default().fg(if selected { TEAL } else { MUTED }),
                ),
                Span::styled(
                    format!("{:<width$}", choice.label, width = label_width),
                    Style::default()
                        .fg(if selected { TEXT } else { MUTED })
                        .add_modifier(if selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
                Span::styled("  →  ", Style::default().fg(MUTED)),
                Span::styled(
                    target_display,
                    Style::default().fg(if selected { TEAL } else { MUTED }),
                ),
                Span::styled(format!("   ({})", choice.note), Style::default().fg(MUTED)),
            ]);
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().bg(SURFACE));

    f.render_stateful_widget(list, chunks[1], &mut app.image_list_state);

    // Custom input box
    if app.is_custom_selected() {
        let input_block = Block::default()
            .title(Span::styled(
                " Custom image reference ",
                Style::default()
                    .fg(if app.custom_image_editing {
                        TEAL
                    } else {
                        MUTED
                    })
                    .add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(if app.custom_image_editing {
                TEAL
            } else {
                BORDER
            }))
            .style(Style::default().bg(SURFACE));

        let cursor = if app.custom_image_editing { "█" } else { "" };
        let input_text = format!("  {}{}", app.custom_image, cursor);
        let input_para =
            Paragraph::new(Span::styled(input_text, Style::default().fg(TEXT))).block(input_block);
        f.render_widget(input_para, chunks[2]);
    } else {
        let hint = Paragraph::new(Span::styled(
            "  Press Enter to accept the selected target, or ↑↓ to choose another.",
            Style::default().fg(MUTED),
        ))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(BORDER))
                .style(Style::default().bg(SURFACE)),
        );
        f.render_widget(hint, chunks[2]);
    }
}
