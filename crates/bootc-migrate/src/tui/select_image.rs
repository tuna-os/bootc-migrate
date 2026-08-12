//! Select Image screen — the preset image list with custom-image text entry,
//! source-OS hint highlighting, and inline help.
//!
//! Extracted from `tui.rs` (bootc-migrate#133): a pure renderer over `App`
//! state; `PRESET_IMAGES` and `detect_source_os` stay in `super` and are
//! pulled in via `use super::*`.

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

    // Source OS context header
    let os_hint_block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(SUBTLE))
        .style(Style::default().bg(DARK_BG));
    let os_line = Paragraph::new(Line::from(vec![
        Span::styled("  Detected source: ", Style::default().fg(SUBTLE)),
        Span::styled(
            app.detected_os.as_str(),
            Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "  — all presets migrate to Dakota (composefs-backed)",
            Style::default().fg(SUBTLE),
        ),
    ]))
    .block(os_hint_block);
    f.render_widget(os_line, chunks[0]);

    let block = Block::default()
        .title(Span::styled(
            " Step 3 · Select Target Image ",
            Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(TEAL))
        .style(Style::default().bg(DARK_BG));

    // Determine which preset matches the detected OS
    let detected_lower = app.detected_os.to_lowercase();
    let recommended_idx = PRESET_IMAGES
        .iter()
        .position(|(_, _, hint)| !hint.is_empty() && detected_lower.contains(hint))
        .unwrap_or(0);

    let items: Vec<ListItem> = PRESET_IMAGES
        .iter()
        .enumerate()
        .map(|(i, (label, image, _hint))| {
            let selected = app.image_list_state.selected() == Some(i);
            let is_custom = i == PRESET_IMAGES.len() - 1;
            let is_recommended = i == recommended_idx;
            let prefix = if selected { "▶ " } else { "  " };
            let target_display = if is_custom {
                if app.custom_image.is_empty() {
                    "<type your image reference>".to_owned()
                } else {
                    app.custom_image.clone()
                }
            } else {
                image.to_string()
            };
            let rec_tag = if is_recommended && !is_custom {
                " ★"
            } else {
                ""
            };
            let line = Line::from(vec![
                Span::styled(
                    prefix,
                    Style::default().fg(if selected { TEAL } else { SUBTLE }),
                ),
                Span::styled(
                    format!("{:<28}", label),
                    Style::default()
                        .fg(if selected { TEXT } else { SUBTLE })
                        .add_modifier(if selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
                Span::styled(
                    rec_tag,
                    Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
                ),
                Span::styled("  →  ", Style::default().fg(SUBTLE)),
                Span::styled(
                    target_display,
                    Style::default().fg(if selected { TEAL } else { SUBTLE }),
                ),
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
                        SUBTLE
                    })
                    .add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(if app.custom_image_editing {
                TEAL
            } else {
                SUBTLE
            }))
            .style(Style::default().bg(SURFACE));

        let cursor = if app.custom_image_editing { "█" } else { "" };
        let input_text = format!("  {}{}", app.custom_image, cursor);
        let input_para =
            Paragraph::new(Span::styled(input_text, Style::default().fg(TEXT))).block(input_block);
        f.render_widget(input_para, chunks[2]);
    } else {
        let hint = Paragraph::new(Span::styled(
            "  Select an image with ↑↓ then press Enter.  ★ = recommended for your system",
            Style::default().fg(SUBTLE),
        ))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(SUBTLE))
                .style(Style::default().bg(SURFACE)),
        );
        f.render_widget(hint, chunks[2]);
    }
}
