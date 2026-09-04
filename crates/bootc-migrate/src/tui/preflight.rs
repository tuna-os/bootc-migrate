//! Preflight screen — readiness state types and the renderers that draw the
//! disk gauges, projected usage, and checklist.
//!
//! Extracted from `tui.rs` (bootc-migrate#135): `Readiness` / `PreflightTuiState`
//! are pure display-state types; the renderers read `app.preflight_state` and
//! the readiness fields without mutating state or running side-effects.

use super::*;

// ─── Preflight visualization state ────────────────────────────────────────────

/// Three-tier readiness for preflight checks.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Readiness {
    Pass,
    Tight,
    Fail,
}

impl Readiness {
    fn icon(&self) -> &'static str {
        match self {
            Readiness::Pass => "✓",
            Readiness::Tight => "⚠",
            Readiness::Fail => "✗",
        }
    }

    fn color(&self) -> Color {
        match self {
            Readiness::Pass => SUCCESS,
            Readiness::Tight => AMBER,
            Readiness::Fail => DANGER,
        }
    }
}

/// Holds preflight data formatted for TUI display.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct PreflightTuiState {
    /// OSTree repo size in bytes.
    ostree_repo_bytes: u64,
    /// Total space on the partition holding /sysroot (or /sysroot/composefs).
    composefs_total: u64,
    /// Free space available for composefs store.
    composefs_free_bytes: u64,
    /// Needed composefs space (1.1× repo with reflink, 1.5× without).
    composefs_needed_bytes: u64,
    /// ESP free space in bytes.
    esp_free_bytes: u64,
    /// ESP total space (estimated).
    esp_total_bytes: u64,
    /// Filesystem type ("btrfs", "xfs", etc.)
    fs_type: String,
    /// Whether the system supports reflink.
    supports_reflink: bool,
    /// Projected composefs usage after migration (= repo size for reflink case).
    projected_composefs_used: u64,
    /// Projected remaining free after migration.
    projected_composefs_free: u64,
    /// Individual readiness checks: (label, readiness).
    checks: Vec<(String, Readiness)>,
    /// Overall readiness.
    overall: Readiness,
}

impl PreflightTuiState {
    pub(crate) fn from_report(report: &bootc_migrate_core::preflight::PreflightReport) -> Self {
        let multiplier: f64 = if report.supports_reflink { 1.1 } else { 1.5 };
        let composefs_needed = (report.ostree_repo_size_bytes as f64 * multiplier) as u64;

        // Total = free + needed (approximate: we assume current usage is small).
        let composefs_total = report.composefs_free_bytes + composefs_needed;

        let projected_used = composefs_needed;
        let projected_free = report.composefs_free_bytes.saturating_sub(composefs_needed);

        // ESP total estimate (free + 150MB typical usage).
        let esp_total = report.esp_free_space_bytes + 150 * 1024 * 1024;

        let mut checks: Vec<(String, Readiness)> = Vec::new();

        // OSTree backend
        checks.push((
            "Booted OSTree backend".to_string(),
            if report.is_bootc_ostree {
                Readiness::Pass
            } else {
                Readiness::Fail
            },
        ));

        // UEFI boot mode
        checks.push((
            "UEFI boot mode".to_string(),
            if report.is_uefi {
                Readiness::Pass
            } else {
                Readiness::Tight
            },
        ));

        // NVRAM writable
        checks.push((
            "NVRAM writable".to_string(),
            if report.nvram_writable {
                Readiness::Pass
            } else if report.is_uefi {
                Readiness::Fail
            } else {
                Readiness::Tight
            },
        ));

        // Reflink support
        checks.push((
            format!(
                "Reflink (CoW) support ({})",
                report.fs_type.as_deref().unwrap_or("unknown")
            ),
            if report.supports_reflink {
                Readiness::Pass
            } else {
                Readiness::Tight
            },
        ));

        // ESP detected
        checks.push((
            "ESP partition detected".to_string(),
            if report.esp_detected {
                Readiness::Pass
            } else {
                Readiness::Tight
            },
        ));

        // ESP space
        let esp_mb = report.esp_free_space_bytes / (1024 * 1024);
        checks.push((
            format!("ESP ≥ 150 MB free ({} MB)", esp_mb),
            if report.esp_ready_for_systemd_boot {
                if esp_mb < 200 {
                    Readiness::Tight
                } else {
                    Readiness::Pass
                }
            } else {
                Readiness::Fail
            },
        ));

        // ComposeFS space
        let needed_gb = composefs_needed as f64 / 1_073_741_824.0;
        checks.push((
            format!(
                "ComposeFS space ≥ {:.1}× repo ({:.1} GB)",
                multiplier, needed_gb
            ),
            if report.composefs_free_bytes >= composefs_needed {
                if report.composefs_free_bytes < (composefs_needed as f64 * 1.2) as u64 {
                    Readiness::Tight
                } else {
                    Readiness::Pass
                }
            } else {
                Readiness::Fail
            },
        ));

        // Pending transaction
        checks.push((
            match &report.pending_transaction {
                bootc_migrate_core::preflight::PendingTransactionStatus::Clean => {
                    "No pending OSTree transaction".to_string()
                }
                other => format!("Pending transaction: {}", other),
            },
            if report.pending_transaction
                == bootc_migrate_core::preflight::PendingTransactionStatus::Clean
            {
                Readiness::Pass
            } else {
                Readiness::Fail
            },
        ));

        // systemd-boot binaries
        checks.push((
            "systemd-boot binaries present".to_string(),
            if report.systemd_boot_binaries_present {
                Readiness::Pass
            } else {
                Readiness::Tight
            },
        ));

        // Overall
        let overall = if checks.iter().any(|(_, r)| *r == Readiness::Fail) {
            Readiness::Fail
        } else if checks.iter().any(|(_, r)| *r == Readiness::Tight) {
            Readiness::Tight
        } else {
            Readiness::Pass
        };

        Self {
            ostree_repo_bytes: report.ostree_repo_size_bytes,
            composefs_total,
            composefs_free_bytes: report.composefs_free_bytes,
            composefs_needed_bytes: composefs_needed,
            esp_free_bytes: report.esp_free_space_bytes,
            esp_total_bytes: esp_total,
            fs_type: report
                .fs_type
                .clone()
                .unwrap_or_else(|| "unknown".to_string()),
            supports_reflink: report.supports_reflink,
            projected_composefs_used: projected_used,
            projected_composefs_free: projected_free,
            checks,
            overall,
        }
    }
}

// ── Preflight ─────────────────────────────────────────────────────────────────

pub(crate) fn render_preflight(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(Span::styled(
            " Step 2 · System Preflight ",
            Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(TEAL))
        .style(Style::default().bg(DARK_BG));

    let inner = block.inner(area);
    f.render_widget(block, area);

    match &app.preflight_state {
        Some(state) => render_preflight_content(f, state, inner),
        None => {
            let text = Paragraph::new(Text::from(vec![
                Line::raw(""),
                Line::from(Span::styled(
                    "  ⚠  Preflight checks could not run.",
                    Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
                )),
                Line::raw(""),
                Line::from(Span::styled(
                    "  This usually means the tool is not running as root, or",
                    Style::default().fg(TEXT),
                )),
                Line::from(Span::styled(
                    "  the system is not an OSTree-backed bootc deployment.",
                    Style::default().fg(TEXT),
                )),
                Line::raw(""),
                Line::from(Span::styled(
                    "  Press [Enter] to continue anyway, or [r] to retry.",
                    Style::default().fg(MUTED),
                )),
            ]))
            .wrap(Wrap { trim: false });
            f.render_widget(text, inner);
        }
    }
}

fn render_preflight_content(f: &mut ratatui::Frame, state: &PreflightTuiState, area: Rect) {
    // Layout: overall banner, disk gauges, projected usage, checklist
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // Overall status banner
            Constraint::Length(3), // OSTree repo gauge
            Constraint::Length(3), // ComposeFS gauge
            Constraint::Length(3), // ESP gauge
            Constraint::Length(1), // Separator
            Constraint::Length(4), // Projected usage
            Constraint::Length(1), // Separator
            Constraint::Min(4),    // Readiness checklist
        ])
        .split(area);

    // ── Overall status banner ──
    let (banner_icon, banner_text, banner_color) = match state.overall {
        Readiness::Pass => ("✓", "System ready for migration", SUCCESS),
        Readiness::Tight => ("⚠", "System ready (with warnings)", AMBER),
        Readiness::Fail => ("✗", "System NOT ready — resolve issues below", DANGER),
    };
    let banner = Paragraph::new(Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(
            format!("{} ", banner_icon),
            Style::default()
                .fg(banner_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            banner_text,
            Style::default()
                .fg(banner_color)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    f.render_widget(banner, chunks[0]);

    // ── OSTree Repo gauge ──
    render_disk_gauge(
        f,
        chunks[1],
        "OSTree Repo",
        state.ostree_repo_bytes,
        state.composefs_total,
        None, // no threshold
        &format!(
            "{:.1} GB on disk",
            state.ostree_repo_bytes as f64 / 1_073_741_824.0
        ),
    );

    // ── ComposeFS free space gauge ──
    let cfs_readiness = if state.composefs_free_bytes >= state.composefs_needed_bytes {
        if state.composefs_free_bytes < (state.composefs_needed_bytes as f64 * 1.2) as u64 {
            Readiness::Tight
        } else {
            Readiness::Pass
        }
    } else {
        Readiness::Fail
    };
    render_disk_gauge_with_threshold(
        f,
        chunks[2],
        "ComposeFS Space",
        state.composefs_needed_bytes,
        state.composefs_free_bytes,
        &cfs_readiness,
        &format!(
            "{:.1} GB needed / {:.1} GB free",
            state.composefs_needed_bytes as f64 / 1_073_741_824.0,
            state.composefs_free_bytes as f64 / 1_073_741_824.0,
        ),
    );

    // ── ESP gauge ──
    let esp_readiness = if state.esp_free_bytes >= 200 * 1024 * 1024 {
        Readiness::Pass
    } else if state.esp_free_bytes >= 150 * 1024 * 1024 {
        Readiness::Tight
    } else {
        Readiness::Fail
    };
    render_disk_gauge_with_threshold(
        f,
        chunks[3],
        "ESP Partition",
        150 * 1024 * 1024,
        state.esp_free_bytes,
        &esp_readiness,
        &format!(
            "{} MB free (150 MB required)",
            state.esp_free_bytes / (1024 * 1024)
        ),
    );

    // ── Separator ──
    let sep1 = Paragraph::new(Line::from(Span::styled(
        "  ─────────────────────────────────────────────────────────────",
        Style::default().fg(MUTED),
    )));
    f.render_widget(sep1, chunks[4]);

    // ── Projected usage ──
    render_projected_usage(f, state, chunks[5]);

    // ── Separator ──
    let sep2 = Paragraph::new(Line::from(Span::styled(
        "  ─────────────────────────────────────────────────────────────",
        Style::default().fg(MUTED),
    )));
    f.render_widget(sep2, chunks[6]);

    // ── Readiness checklist ──
    render_readiness_checklist(f, state, chunks[7]);
}

/// Draw a gauge whose readout sits *beside* the bar rather than inside it.
///
/// `Gauge` centres its label and writes it with one style, but it only paints
/// cells left of the fill boundary — the unfilled remainder keeps the panel
/// background. A label styled for the bright fill (`fg(DARK_BG)`, as all three
/// gauges here were) is therefore drawn dark-on-dark the moment it extends past
/// the fill, which for a centred label is any ratio under ~50%: at 0% the
/// readout was DARK_BG on DARK_BG, a contrast ratio of 1.0. Straddling the
/// boundary has no good colour either, since one style has to cover both sides.
///
/// So the readout gets its own columns, always over the panel background, and
/// the bar is left to be a bar. `TRACK` paints the remainder so an empty gauge
/// still reads as one.
fn render_gauge_with_readout(
    f: &mut ratatui::Frame,
    area: Rect,
    ratio: f64,
    color: Color,
    readout: &str,
) {
    let readout_width = (readout.chars().count() as u16) + 2;
    let bar_width = area.width.saturating_sub(readout_width);

    let bar_area = Rect {
        width: bar_width,
        height: 1,
        ..area
    };
    // Paint the track first: Gauge only styles the filled cells.
    f.render_widget(Block::default().style(Style::default().bg(TRACK)), bar_area);
    f.render_widget(
        Gauge::default()
            .gauge_style(Style::default().fg(color).bg(TRACK))
            .ratio(ratio)
            .label(Span::raw("")),
        bar_area,
    );

    let readout_area = Rect {
        x: area.x + bar_width,
        y: area.y,
        width: readout_width,
        height: 1,
    };
    f.render_widget(
        Paragraph::new(Span::styled(
            format!(" {readout}"),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )),
        readout_area,
    );
}

fn render_disk_gauge(
    f: &mut ratatui::Frame,
    area: Rect,
    label: &str,
    used: u64,
    total: u64,
    _threshold: Option<u64>,
    detail: &str,
) {
    let ratio = if total > 0 {
        (used as f64 / total as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };

    let color = if ratio < 0.6 {
        SUCCESS
    } else if ratio < 0.85 {
        AMBER
    } else {
        DANGER
    };

    let lines = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(2)])
        .split(area);

    // Label line
    let label_line = Paragraph::new(Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(
            format!("{:<18}", label),
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(detail, Style::default().fg(color)),
    ]));
    f.render_widget(label_line, lines[0]);

    // Gauge
    let gauge_area = Rect {
        x: area.x + 2,
        y: lines[1].y,
        width: area.width.saturating_sub(4),
        height: 1,
    };
    render_gauge_with_readout(
        f,
        gauge_area,
        ratio,
        color,
        &format!("{:>3}%", (ratio * 100.0) as u32),
    );
}

fn render_disk_gauge_with_threshold(
    f: &mut ratatui::Frame,
    area: Rect,
    label: &str,
    needed: u64,
    available: u64,
    readiness: &Readiness,
    detail: &str,
) {
    let ratio = if available > 0 {
        (needed as f64 / available as f64).clamp(0.0, 1.0)
    } else {
        1.0
    };

    let color = readiness.color();
    let icon = readiness.icon();

    let lines = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(2)])
        .split(area);

    // Label line with icon
    let label_line = Paragraph::new(Line::from(vec![
        Span::styled(
            format!("  {} ", icon),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{:<16}", label),
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(detail, Style::default().fg(color)),
    ]));
    f.render_widget(label_line, lines[0]);

    // Gauge
    let gauge_area = Rect {
        x: area.x + 4,
        y: lines[1].y,
        width: area.width.saturating_sub(6),
        height: 1,
    };
    render_gauge_with_readout(
        f,
        gauge_area,
        ratio,
        color,
        &format!("{:>3}%", (ratio * 100.0) as u32),
    );
}

fn render_projected_usage(f: &mut ratatui::Frame, state: &PreflightTuiState, area: Rect) {
    let lines = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title
            Constraint::Length(2), // gauge
            Constraint::Length(1), // padding
        ])
        .split(area);

    let title = Paragraph::new(Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(
            "Projected After Migration",
            Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
        ),
    ]));
    f.render_widget(title, lines[0]);

    let projected_total = state.projected_composefs_used + state.projected_composefs_free;
    let ratio = if projected_total > 0 {
        (state.projected_composefs_used as f64 / projected_total as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };

    let proj_color = if state.projected_composefs_free > state.projected_composefs_used {
        SUCCESS
    } else if state.projected_composefs_free > 0 {
        AMBER
    } else {
        DANGER
    };

    let gauge_area = Rect {
        x: area.x + 4,
        y: lines[1].y,
        width: area.width.saturating_sub(6),
        height: 1,
    };
    // The used/free figures appear nowhere else on this screen, so they are the
    // readout rather than a percentage.
    let readout = format!(
        "{:.1} GB used → {:.1} GB free",
        state.projected_composefs_used as f64 / 1_073_741_824.0,
        state.projected_composefs_free as f64 / 1_073_741_824.0,
    );
    render_gauge_with_readout(f, gauge_area, ratio, proj_color, &readout);
}

fn render_readiness_checklist(f: &mut ratatui::Frame, state: &PreflightTuiState, area: Rect) {
    let items: Vec<ListItem> = state
        .checks
        .iter()
        .map(|(label, readiness)| {
            let icon = readiness.icon();
            let color = readiness.color();
            let suffix = match readiness {
                Readiness::Pass => "",
                Readiness::Tight => "  (warning)",
                Readiness::Fail => "  (BLOCKER)",
            };
            let content = Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(
                    format!("{} ", icon),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(label.as_str(), Style::default().fg(TEXT)),
                Span::styled(suffix, Style::default().fg(color)),
            ]);
            ListItem::new(content)
        })
        .collect();

    let list = List::new(items).style(Style::default().bg(DARK_BG));
    f.render_widget(list, area);
}
