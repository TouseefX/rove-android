#[path = "ui/dashboard.rs"]
mod dashboard;
#[path = "ui/gallery.rs"]
mod gallery;
use crate::{
    actions::{self, Action, View},
    app::{display, App},
    backend::Target,
    editor::Editor,
    images::{Key as ImageKey, Slot, Source},
    syntax,
    theme::{Theme, THEMES},
};
use crossterm::{
    cursor::{Hide, Show},
    event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Wrap},
    Frame, Terminal,
};
use std::io;

pub struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore();
    }
}
pub fn restore() {
    let _ = disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen,
        Show
    );
}
pub fn enter() -> io::Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    if let Err(e) = execute!(
        io::stdout(),
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture,
        Hide
    ) {
        restore();
        return Err(e);
    }
    Terminal::new(CrosstermBackend::new(io::stdout())).inspect_err(|_| restore())
}
fn style(fg: Color) -> Style {
    Style::default().fg(fg)
}
fn span(text: impl Into<String>, fg: Color) -> Span<'static> {
    Span::styled(text.into(), style(fg))
}
fn bold(text: impl Into<String>, fg: Color) -> Span<'static> {
    Span::styled(text.into(), style(fg).add_modifier(Modifier::BOLD))
}
fn panel(title: &str, t: Theme) -> Block<'static> {
    tinted_panel(title, t, t.text)
}
fn tinted_panel(title: &str, t: Theme, tint: Color) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(style(t.edge))
        .title(Line::from(vec![
            span(" ", t.text),
            bold(title, tint),
            span(" ", t.text),
        ]))
        .style(style(t.text).bg(t.panel))
}
fn inset(r: Rect, x: u16, y: u16) -> Rect {
    Rect::new(
        r.x.saturating_add(x),
        r.y.saturating_add(y),
        r.width.saturating_sub(x * 2),
        r.height.saturating_sub(y * 2),
    )
}
fn centered(r: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(r.width.saturating_sub(4));
    let h = h.min(r.height.saturating_sub(2));
    Rect::new(r.x + (r.width - w) / 2, r.y + (r.height - h) / 2, w, h)
}
fn mb(n: u64) -> f64 {
    n as f64 / 1_000_000.0
}
fn duration(n: f64) -> String {
    let n = n.max(0.0) as u64;
    format!("{:02}:{:02}:{:02}", n / 3600, n / 60 % 60, n % 60)
}
fn compact(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}m", n as f64 / 1_000_000.0)
    } else if n >= 10_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}
fn phase(app: &App) -> &str {
    app.telemetry["phase"].as_str().unwrap_or("idle")
}
fn phase_color(app: &App, t: Theme) -> Color {
    match phase(app) {
        "connected" | "sandbox" => t.green,
        "error" => t.red,
        "idle" | "ended" => t.muted,
        _ => t.amber,
    }
}
fn view_color(view: View, t: Theme) -> Color {
    match view {
        View::Overview => t.accent,
        View::Console => t.secondary,
        View::Network => t.blue,
    }
}
fn workspace_visible(app: &App) -> bool {
    app.picker.is_none() && app.palette.is_none() && !app.help
}
fn draw_mark(frame: &mut Frame, app: &App, slot: Slot, area: Rect, t: Theme) -> bool {
    app.images.borrow_mut().draw(
        frame,
        slot,
        ImageKey::new(0, 0, app.theme, area),
        area,
        t,
        || Source::Mark,
    )
}
fn phase_label(app: &App) -> String {
    if matches!(
        phase(app),
        "starting" | "allocating" | "connecting" | "authenticating" | "leaving"
    ) {
        format!(
            "{} {}",
            ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]
                [(app.started.elapsed().as_millis() / 110) as usize % 10],
            phase(app)
        )
    } else {
        format!("● {}", phase(app))
    }
}
struct Regions {
    header: Rect,
    sidebar: Option<Rect>,
    tabs: Rect,
    metrics: Rect,
    body: Rect,
    input: Rect,
    footer: Rect,
}
fn regions(area: Rect, app: &App) -> Regions {
    let outer = Layout::vertical([
        Constraint::Length(if area.height >= 36 {
            4
        } else if area.height >= 30 {
            3
        } else {
            2
        }),
        Constraint::Min(14),
        Constraint::Length(1),
    ])
    .margin(1)
    .split(area);
    let (sidebar, work) = if area.width >= 112 {
        let cols = Layout::horizontal([Constraint::Length(25), Constraint::Min(60)])
            .spacing(1)
            .split(outer[1]);
        (Some(cols[0]), cols[1])
    } else {
        (None, outer[1])
    };
    let input_height = if app.view == View::Console {
        (app.input.text.lines().count() as u16 + 3).clamp(7, 12)
    } else {
        (app.input.text.lines().count() as u16 + 3).clamp(if area.height >= 30 { 5 } else { 3 }, 8)
    };
    let rows = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(if app.view == View::Console {
            0
        } else if area.height >= 40 {
            7
        } else if area.height >= 32 {
            5
        } else {
            3
        }),
        Constraint::Min(6),
        Constraint::Length(input_height),
    ])
    .split(work);
    let (body, input) = if app.view == View::Console {
        let split = Layout::vertical([Constraint::Length(input_height), Constraint::Min(5)]).split(
            Rect::new(
                work.x,
                rows[2].y,
                work.width,
                rows[2].height + rows[3].height,
            ),
        );
        (split[1], split[0])
    } else {
        (rows[2], rows[3])
    };
    Regions {
        header: outer[0],
        sidebar,
        tabs: rows[0],
        metrics: rows[1],
        body,
        input,
        footer: outer[2],
    }
}
fn body_regions(area: Rect) -> (Rect, Option<Rect>) {
    if area.width >= 95 {
        let r = Layout::horizontal([Constraint::Min(58), Constraint::Percentage(34)])
            .spacing(1)
            .split(area);
        (r[0], Some(r[1]))
    } else {
        (area, None)
    }
}
fn graph_regions(area: Rect) -> (Rect, Rect) {
    let r = Layout::vertical([Constraint::Percentage(60), Constraint::Min(5)]).split(area);
    (r[0], r[1])
}
pub fn draw(frame: &mut Frame, app: &App) {
    let t = THEMES[app.theme];
    let area = frame.area();
    frame.render_widget(Block::default().style(style(t.text).bg(t.bg)), area);
    if area.width < 68 || area.height < 22 {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(bold("rove", t.accent)),
                Line::from(""),
                Line::from("A little more room, please."),
                Line::from("Use at least 68 × 22. Ctrl+Q still quits."),
            ])
            .block(panel("resize terminal", t))
            .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }
    if app.picker.is_some() {
        picker(frame, app, t);
        if app.palette.is_some() || app.help {
            for cell in &mut frame.buffer_mut().content {
                cell.fg = t.edge;
                cell.bg = t.bg;
            }
            if app.palette.is_some() {
                palette(frame, app, t);
            }
            if app.help {
                help(frame, t);
            }
        }
        return;
    }
    let r = regions(area, app);
    header(frame, app, r.header, t);
    if let Some(side) = r.sidebar {
        sidebar(frame, app, side, t);
    }
    tabs(frame, app, r.tabs, t);
    if r.metrics.height > 0 {
        metrics(frame, app, r.metrics, t);
    }
    match app.view {
        View::Overview if phase(app) == "idle" && app.samples.is_empty() => {
            welcome(frame, app, r.body, t);
        }
        View::Overview => {
            let (graphs, activity) = graph_regions(r.body);
            let (memory, network) = body_regions(graphs);
            plot(frame, app, memory, t, false);
            if let Some(network) = network {
                if matches!(app.target, Some(Target::Game(_))) && network.height >= 10 {
                    session_card(frame, app, network, t);
                } else {
                    plot(frame, app, network, t, true);
                }
            }
            logs(frame, app, activity, t);
        }
        View::Console => logs(frame, app, r.body, t),
        View::Network => {
            let (graph, details) = graph_regions(r.body);
            plot(frame, app, graph, t, true);
            network_details(frame, app, details, t);
        }
    }
    editor(
        frame,
        &app.input,
        r.input,
        t,
        app.picker.is_none() && app.palette.is_none() && !app.help,
        true,
        if app.telemetry["luau_ready"] == true {
            "script / Luau ready"
        } else {
            "script / waiting"
        },
        "-- write Luau here; Enter runs, Alt+Enter adds a line",
    );
    footer(frame, app, r.footer, t);
    if app.picker.is_some() || app.palette.is_some() || app.help {
        for cell in &mut frame.buffer_mut().content {
            cell.fg = t.edge;
            cell.bg = t.bg;
        }
    }
    if app.picker.is_some() {
        picker(frame, app, t);
    }
    if app.palette.is_some() {
        palette(frame, app, t);
    }
    if app.help {
        help(frame, t);
    }
}
fn header(frame: &mut Frame, app: &App, r: Rect, t: Theme) {
    dashboard::header(frame, app, r, t)
}
fn sidebar_rows(r: Rect) -> Vec<(Rect, Action)> {
    dashboard::sidebar_rows(r)
}
fn sidebar(frame: &mut Frame, app: &App, r: Rect, t: Theme) {
    dashboard::sidebar(frame, app, r, t)
}
fn tab_boxes(r: Rect) -> Vec<Rect> {
    Layout::horizontal([
        Constraint::Length(16),
        Constraint::Length(16),
        Constraint::Length(16),
        Constraint::Min(0),
        Constraint::Length(17),
    ])
    .split(r)
    .to_vec()
}
fn tabs(frame: &mut Frame, app: &App, r: Rect, t: Theme) {
    frame.render_widget(
        Block::default()
            .borders(Borders::BOTTOM)
            .border_style(style(t.edge)),
        r,
    );
    let boxes = tab_boxes(r);
    for (i, v) in [View::Overview, View::Console, View::Network]
        .iter()
        .enumerate()
    {
        frame.render_widget(
            Paragraph::new(format!(" {}  {} ", i + 1, v.label())).style(if *v == app.view {
                style(view_color(*v, t))
                    .bg(t.raised)
                    .add_modifier(Modifier::BOLD)
            } else {
                style(t.muted)
            }),
            Rect::new(boxes[i].x, boxes[i].y, boxes[i].width, 1),
        );
    }
    frame.render_widget(
        Paragraph::new("commands  ctrl+k ")
            .alignment(Alignment::Right)
            .style(style(t.muted)),
        boxes[4],
    );
}
fn metrics(frame: &mut Frame, app: &App, r: Rect, t: Theme) {
    dashboard::metrics(frame, app, r, t)
}
fn plot(frame: &mut Frame, app: &App, r: Rect, t: Theme, network: bool) {
    let color = if network { t.blue } else { t.accent };
    let b = tinted_panel(
        if network {
            "transport latency"
        } else {
            "memory over time"
        },
        t,
        color,
    );
    let inside = b.inner(r);
    frame.render_widget(b, r);
    if inside.height < 4 {
        return;
    }
    let parts = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(if inside.height >= 5 { 1 } else { 0 }),
        Constraint::Length(if inside.height >= 7 { 2 } else { 1 }),
    ])
    .split(inside);
    let narrow = inside.width < 45;
    let label = if network {
        vec![span(" ● RTT / ms", color)]
    } else {
        vec![
            span(" ● client", t.accent),
            span("  ● client + dashboard", t.secondary),
        ]
    };
    frame.render_widget(Paragraph::new(Line::from(label)), parts[0]);
    if parts[0].width > 55 {
        frame.render_widget(
            Paragraph::new(format!("{} f8 ", ["30s", "2m", "10m"][app.graph_window]))
                .alignment(Alignment::Right)
                .style(style(t.muted)),
            Rect::new(parts[0].right() - 9, parts[0].y, 9, 1),
        );
    }
    let period = [30., 120., 600.][app.graph_window];
    let end = app.samples.back().map(|s| s.time).unwrap_or(1.).max(1.);
    let start = (end - period).max(0.);
    let a: Vec<_> = app
        .samples
        .iter()
        .filter(|s| s.time >= start)
        .filter_map(|s| {
            if network {
                s.rtt_ms
                    .filter(|v| v.is_finite() && *v >= 0.)
                    .map(|v| (s.time, v))
            } else {
                Some((s.time, mb(s.client_bytes)))
            }
        })
        .collect();
    let total: Vec<_> = if network {
        vec![]
    } else {
        app.samples
            .iter()
            .filter(|s| s.time >= start)
            .map(|s| (s.time, mb(s.client_bytes.saturating_add(s.ui_bytes))))
            .collect()
    };
    if a.is_empty() {
        let empty = if network {
            vec![
                Line::from(bold("—", t.muted)),
                Line::from(""),
                Line::from(span("No transport measurements yet.", t.muted)),
                Line::from(""),
                Line::from(span("RTT appears after a game server connects.", t.muted)),
            ]
        } else {
            vec![
                Line::from(""),
                Line::from(bold("Waiting for the first sample", t.text)),
                Line::from(""),
                Line::from(span(
                    "Find a game with F2, or open a local sandbox.",
                    t.muted,
                )),
            ]
        };
        frame.render_widget(
            Paragraph::new(empty)
                .wrap(Wrap { trim: true })
                .alignment(Alignment::Center),
            inset(parts[1], 1, 1),
        );
    } else {
        let high = a.iter().chain(&total).map(|(_, v)| *v).fold(1., f64::max);
        let upper = crate::graph::ceiling(high);
        let cols = Layout::horizontal([
            Constraint::Length(if upper >= 1000. { 5 } else { 4 }),
            Constraint::Min(1),
        ])
        .split(parts[1]);
        let mut labels = vec![(0, upper)];
        if cols[0].height >= 4 {
            labels.push((cols[0].height / 2, upper / 2.));
        }
        if cols[0].height >= 2 {
            labels.push((cols[0].height - 1, 0.));
        }
        for (row, value) in labels {
            frame.render_widget(
                Paragraph::new(format!("{value:.0} "))
                    .alignment(Alignment::Right)
                    .style(style(t.muted)),
                Rect::new(cols[0].x, cols[0].y + row, cols[0].width, 1),
            );
        }
        let supports_pixels = app.images.borrow().pixels();
        let pixel_chart = workspace_visible(app)
            && supports_pixels
            && app.images.borrow_mut().draw(
                frame,
                if network { Slot::Network } else { Slot::Memory },
                ImageKey::new(
                    app.graph_window as u64
                        ^ app.samples.front().map(|s| s.time.to_bits()).unwrap_or(0),
                    (end * 2.) as u64,
                    app.theme,
                    cols[1],
                ),
                cols[1],
                t,
                || {
                    Source::Chart(crate::graph::Chart {
                        primary: a.clone(),
                        secondary: total.clone(),
                        start,
                        end,
                        upper,
                        color,
                    })
                },
            );
        if !pixel_chart {
            frame.render_widget(
                crate::graph::History {
                    primary: &a,
                    secondary: &total,
                    start,
                    end,
                    upper,
                    primary_color: color,
                    theme: t,
                },
                cols[1],
            );
        }
        let time = Layout::horizontal([Constraint::Ratio(1, 3); 3]).split(parts[2]);
        for (i, value) in [start, (start + end) / 2., end].iter().enumerate() {
            frame.render_widget(
                Paragraph::new(format!("{value:.0}s"))
                    .alignment([Alignment::Left, Alignment::Center, Alignment::Right][i])
                    .style(style(t.muted)),
                time[i],
            );
        }
    }
    let current = a
        .last()
        .map(|(_, v)| format!("{v:.1}"))
        .unwrap_or("—".into());
    let max = a
        .iter()
        .map(|(_, v)| *v)
        .reduce(f64::max)
        .map(|v| format!("{v:.1}"))
        .unwrap_or("—".into());
    let avg = if a.is_empty() {
        "—".into()
    } else {
        format!(
            "{:.1}",
            a.iter().map(|(_, v)| *v).sum::<f64>() / a.len() as f64
        )
    };
    let mut summary = vec![span(" now ", t.muted), bold(current, color)];
    if !narrow {
        summary.extend([span("  ·  avg ", t.muted), span(avg, t.secondary)]);
    }
    summary.extend([
        span(if narrow { "  max " } else { "  ·  max " }, t.muted),
        bold(max, t.amber),
    ]);
    if !narrow {
        summary.push(span(if network { " ms" } else { " MB" }, t.muted));
    }
    let sub = if network {
        if narrow {
            " actual transport".into()
        } else {
            " actual transport observations".into()
        }
    } else {
        format!(
            " budget {:.0} MB (auto, 10 MB buffer)",
            mb(app.ram_budget.limit_bytes())
        )
    };
    frame.render_widget(
        Paragraph::new(vec![Line::from(summary), Line::from(span(sub, t.muted))]),
        parts[3],
    );
}
fn logs(frame: &mut Frame, app: &App, r: Rect, t: Theme) {
    let rows = app.visible_logs();
    let end = rows
        .len()
        .saturating_sub(app.log_scroll.min(rows.len().saturating_sub(1)));
    let available = r.height.saturating_sub(3) as usize;
    let begin = end.saturating_sub(available);
    let b = tinted_panel(
        if app.view == View::Console {
            "console output"
        } else {
            "activity"
        },
        t,
        t.secondary,
    )
    .title_top(
        Line::from(span(
            if app.log_scroll > 0 {
                " ↑ history "
            } else {
                " ● following "
            },
            if app.log_scroll > 0 { t.amber } else { t.green },
        ))
        .alignment(Alignment::Right),
    );
    let inside = b.inner(r);
    frame.render_widget(b, r);
    if inside.height == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            span("  filter ", t.muted),
            bold(["all", "scripts", "problems"][app.log_filter], t.secondary),
            span("  ·  ctrl+e", t.muted),
        ])),
        Rect::new(inside.x, inside.y, inside.width, 1),
    );
    let area = Rect::new(
        inside.x,
        inside.y + 1,
        inside.width,
        inside.height.saturating_sub(1),
    );
    if rows.is_empty() {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from(span(
                    if app.log_filter == 2 {
                        "No problems in the activity log."
                    } else {
                        "Output will appear here as your session runs."
                    },
                    t.muted,
                )),
            ])
            .alignment(Alignment::Center),
            area,
        );
        return;
    }
    let lines: Vec<_> = rows[begin..end]
        .iter()
        .map(|log| {
            let color = match log.level {
                "error" => t.red,
                "warn" => t.amber,
                "result" => t.green,
                "print" => t.secondary,
                "input" => t.blue,
                "job" => t.accent,
                _ => t.muted,
            };
            let marker = match log.level {
                "input" => "›",
                "result" => "↳",
                "error" => "!",
                "warn" => "!",
                "job" => "·",
                _ => "│",
            };
            Line::from(vec![
                span(format!(" {} ", duration(log.at)), t.muted),
                span(format!("{marker} "), color),
                span(format!("{:<7}", log.level), color),
                span(
                    log.text.clone(),
                    if log.level == "error" { t.red } else { t.text },
                ),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}
fn network_details(frame: &mut Frame, app: &App, r: Rect, t: Theme) {
    let d = &app.telemetry;
    let fields = [
        ("application messages", display(&d["messages_received"])),
        ("ping requests received", display(&d["ping_requests"])),
        ("ping replies sent", display(&d["ping_replies"])),
        ("pending replies", display(&d["ping_pending"])),
        ("physics frames sent", display(&d["physics_frames_sent"])),
        (
            "last packet",
            d["last_packet_age_seconds"]
                .as_f64()
                .map(|n| format!("{n:.2}s ago"))
                .unwrap_or_else(|| "—".into()),
        ),
    ];
    let lines: Vec<_> = fields
        .into_iter()
        .map(|(key, value)| {
            Line::from(vec![
                span(format!("  {key:<29}"), t.muted),
                bold(value, t.text),
            ])
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines).block(panel("session traffic", t).padding(Padding::vertical(1))),
        r,
    );
}
fn editor(
    frame: &mut Frame,
    input: &Editor,
    r: Rect,
    t: Theme,
    focused: bool,
    code: bool,
    title: &str,
    placeholder: &str,
) {
    let mut b = tinted_panel(title, t, t.secondary).border_style(style(if focused {
        t.secondary
    } else {
        t.edge
    }));
    if code && r.width > 65 {
        b = b.title_top(
            Line::from(span(" enter run · alt+enter newline ", t.muted))
                .alignment(Alignment::Right),
        );
    }
    let (row, col) = input.position();
    if code && r.width >= 32 {
        b = b.title_bottom(
            Line::from(vec![
                span(" luau ", t.secondary),
                span(format!("· {}:{} ", row + 1, col + 1), t.muted),
            ]).alignment(Alignment::Right),
        );
    }
    let inside = inset(b.inner(r), 1, 0);
    frame.render_widget(b, r);
    if inside.width < 5 || inside.height == 0 {
        return;
    }
    let sy = row.saturating_sub(inside.height.saturating_sub(1) as usize);
    let gutter = if code { 4 } else { 0 };
    let width = inside.width.saturating_sub(gutter);
    let sx = col.saturating_sub(width.saturating_sub(1) as usize);
    let area = Rect::new(inside.x + gutter, inside.y, width, inside.height);
    if code && focused {
        frame.render_widget(
            Block::default().style(Style::default().bg(t.raised)),
            Rect::new(inside.x, inside.y + (row - sy) as u16, inside.width, 1),
        );
    }
    if input.text.is_empty() {
        frame.render_widget(Paragraph::new(placeholder).style(style(t.muted)), area);
    } else {
        let lines: Vec<_> = input
            .text
            .split('\n')
            .skip(sy)
            .take(inside.height as usize)
            .enumerate()
            .map(|(offset, line)| {
                let line = if code {
                    syntax::line(line, t)
                } else {
                    Line::from(line)
                };
                if code && focused && sy + offset == row {
                    line.style(Style::default().bg(t.raised))
                } else { line }
            })
            .collect();
        frame.render_widget(
            Paragraph::new(lines)
                .style(style(t.text))
                .scroll((0, sx.min(u16::MAX as usize) as u16)),
            area,
        );
    }
    if code {
        let lines: Vec<_> = (sy..sy + inside.height as usize)
            .map(|n| {
                Line::from(span(
                    format!("{:>2} │", n + 1),
                    if n == row && focused {
                        t.accent
                    } else {
                        t.edge
                    },
                ))
            })
            .collect();
        frame.render_widget(
            Paragraph::new(lines),
            Rect::new(inside.x, inside.y, gutter, inside.height),
        );
    }
    if focused {
        frame.set_cursor_position((area.x + (col - sx) as u16, area.y + (row - sy) as u16));
    }
}
fn footer(frame: &mut Frame, app: &App, r: Rect, t: Theme) {
    if let Some((message, when)) = &app.toast {
        if when.elapsed().as_secs_f32() < 3.0 {
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    bold(" ✓ ", t.green),
                    span(message.clone(), t.text),
                ])),
                r,
            );
            return;
        }
    }
    if app.quitting {
        frame.render_widget(Paragraph::new(span(" leaving the session…", t.amber)), r);
        return;
    }
    let key = |key: &str, label: &str| vec![
        Span::styled(format!(" {key} "), style(t.text).bg(t.raised)),
        span(format!(" {label}  "), t.muted),
    ];
    // Keep help and exit visible, adding secondary shortcuts only when they fit.
    let tail = [key("f1", "help"), key("ctrl+q", "quit")].concat();
    let reserved = Line::from(tail.clone()).width();
    let mut hints = Vec::new();
    for (binding, label) in [("ctrl+k", "actions"), ("f2", "games"),
        ("alt+1/2/3", "views"), ("ctrl+t", "theme"), ("ctrl+d", "leave")] {
        let next = key(binding, label);
        if Line::from(hints.clone()).width() + Line::from(next.clone()).width() + reserved <= r.width as usize {
            hints.extend(next);
        }
    }
    hints.extend(tail);
    frame.render_widget(Paragraph::new(Line::from(hints)), r);
}

struct PickerRects {
    outer: Rect,
    heading: Rect,
    input: Rect,
    message: Rect,
    list: Rect,
    detail: Option<Rect>,
    button: Rect,
    toggle: Rect,
    footer: Rect,
}
fn picker_rects(area: Rect, expanded: bool) -> PickerRects {
    let outer = inset(area, 1, 1);
    let inner = inset(outer, 1, 0);
    let rows = Layout::vertical([
        Constraint::Length(if area.height >= 30 { 3 } else { 2 }),
        Constraint::Length(if expanded { 0 } else { 3 }),
        Constraint::Length(if expanded { 0 } else { 2 }),
        Constraint::Min(5),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .split(inner);
    let (list, detail) = if expanded {
        let width = 104.min(rows[3].width);
        (
            Rect::default(),
            Some(Rect::new(
                rows[3].x + (rows[3].width - width) / 2,
                rows[3].y,
                width,
                rows[3].height,
            )),
        )
    } else if inner.width >= 86 {
        let cols = Layout::horizontal([Constraint::Percentage(37), Constraint::Min(40)])
            .spacing(3)
            .split(rows[3]);
        (cols[0], Some(cols[1]))
    } else {
        (rows[3], None)
    };
    let controls = Layout::horizontal([
        Constraint::Length(25),
        Constraint::Min(0),
        Constraint::Length(24),
    ])
    .spacing(1)
    .split(rows[4]);
    PickerRects {
        outer,
        heading: rows[0],
        input: rows[1],
        message: rows[2],
        list,
        detail,
        button: controls[0],
        toggle: controls[2],
        footer: rows[5],
    }
}
fn picker_row_height(area: Rect) -> u16 {
    if area.height >= 18 {
        3
    } else {
        2
    }
}
fn selection_start(selected: usize, slots: usize) -> usize {
    selected.saturating_sub(slots.saturating_sub(1))
}
fn popup(frame: &mut Frame, r: Rect, t: Theme, title: &str) {
    let shadow = Rect::new(
        r.x + 1,
        r.y + 1,
        r.width.min(frame.area().right().saturating_sub(r.x + 1)),
        r.height.min(frame.area().bottom().saturating_sub(r.y + 1)),
    );
    frame.render_widget(
        Block::default().style(Style::default().bg(Color::Black)),
        shadow,
    );
    frame.render_widget(Clear, r);
    frame.render_widget(panel(title, t).border_style(style(t.edge)), r);
}
fn highlighted(text: &str, positions: &[usize], t: Theme, _selected: bool) -> Vec<Span<'static>> {
    text.chars()
        .enumerate()
        .map(|(i, c)| {
            Span::styled(
                c.to_string(),
                if positions.contains(&i) {
                    style(t.accent).add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                } else {
                    style(t.text)
                },
            )
        })
        .collect()
}

struct PreviewRegions {
    heading: Rect,
    hero: Rect,
    thumbs: Vec<Rect>,
    copy: Rect,
    footer: Rect,
}
fn preview_inner(detail: Rect) -> Rect {
    inset(detail, 1, 0)
}
fn preview_regions(inside: Rect) -> PreviewRegions {
    let heading = Rect::new(inside.x, inside.y, inside.width, 2.min(inside.height));
    let body_top = heading.bottom() + u16::from(inside.height > 10);
    let available = inside.bottom().saturating_sub(body_top);
    let strip_h = if available >= 18 && inside.width < 96 {
        5
    } else {
        0
    };
    let copy_h = if available >= 21 && inside.width < 96 {
        6
    } else if available >= 12 {
        4
    } else {
        2
    };
    let hero_h = available.saturating_sub(strip_h + copy_h + 2);
    let hero = Rect::new(inside.x, body_top, inside.width, hero_h);
    let strip = Rect::new(inside.x, hero.bottom(), inside.width, strip_h);
    let thumbs = if strip_h > 0 {
        Layout::horizontal([Constraint::Ratio(1, 3); 3])
            .spacing(1)
            .split(strip)
            .to_vec()
    } else {
        vec![]
    };
    let copy = Rect::new(inside.x, strip.bottom() + 1, inside.width, copy_h);
    PreviewRegions {
        heading,
        hero,
        thumbs,
        copy,
        footer: Rect::new(inside.x, inside.bottom().saturating_sub(1), inside.width, 1),
    }
}
fn preview(frame: &mut Frame, app: &App, t: Theme, detail: Rect) {
    let p = app.picker.as_ref().unwrap();
    let inside = preview_inner(detail);
    frame.render_widget(Block::default().style(style(t.text).bg(t.panel)), detail);
    let active = app.palette.is_none() && !app.help;
    let Some(game) = p.selected_game() else {
        let logo_h = inside.height.saturating_sub(8).min(13);
        if active {
            draw_mark(
                frame,
                app,
                Slot::Welcome,
                Rect::new(inside.x, inside.y + 1, inside.width, logo_h),
                t,
            );
        }
        let y = inside.y + logo_h + 2;
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(bold("less setup. more possibility.", t.text)),
                Line::from(""),
                Line::from(span("find a world. make it yours.", t.muted)),
                Line::from(""),
                Line::from(span("search a name or paste a place link", t.secondary)),
            ])
            .alignment(Alignment::Center),
            Rect::new(inside.x, y, inside.width, inside.bottom().saturating_sub(y)),
        );
        return;
    };
    let r = preview_regions(inside);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(bold(game.name.clone(), t.text)),
            Line::from(vec![
                span(format!("by {}", game.creator), t.muted),
                span(if game.verified { "  ✓" } else { "" }, t.blue),
            ]),
        ]),
        r.heading,
    );
    let (artwork, picture, status) = {
        let images = app.images.borrow();
        (
            images.art.get(game.place_id).cloned(),
            images.art.picture,
            if images.enabled() {
                images.art.status(game.place_id).to_owned()
            } else {
                "images are turned off".into()
            },
        )
    };
    let count = artwork.as_ref().map(|a| a.pictures.len()).unwrap_or(0);
    let selected = picture.min(count.saturating_sub(1));
    if r.hero.height > 0 {
        let image = artwork
            .as_ref()
            .and_then(|a| a.pictures.get(selected))
            .cloned();
        let b = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(style(t.edge))
            .style(style(t.text).bg(t.panel))
            .title_bottom(
                Line::from(span(
                    if count > 0 {
                        format!(" artwork {} / {} ", selected + 1, count)
                    } else {
                        " gallery ".into()
                    },
                    t.muted,
                ))
                .alignment(Alignment::Right),
            );
        let hero = b.inner(r.hero);
        frame.render_widget(b, r.hero);
        let drawn = active
            && image.is_some_and(|image| {
                app.images.borrow_mut().draw(
                    frame,
                    Slot::Hero,
                    ImageKey::new(
                        game.place_id.wrapping_mul(4) + selected as u64,
                        0,
                        app.theme,
                        hero,
                    ),
                    hero,
                    t,
                    || Source::Picture(image),
                )
            });
        if !drawn {
            gallery::placeholder(
                frame,
                hero,
                t,
                "a window into this world",
                if count > 0 {
                    "preparing preview…"
                } else {
                    &status
                },
            );
        }
        if let Some(art) = &artwork {
            gallery::thumbnails(
                frame,
                app,
                t,
                &r.thumbs,
                game.place_id,
                &art.pictures,
                selected,
                active,
            );
        }
    }
    if r.copy.height > 0 {
        let stats = Line::from(vec![
            bold(
                game.players
                    .map(|v| format!("● {}", compact(v)))
                    .unwrap_or("—".into()),
                t.green,
            ),
            span(" playing   ", t.muted),
            bold(
                game.approval
                    .map(|v| format!("{v:.0}%"))
                    .unwrap_or("—".into()),
                t.text,
            ),
            span(" approval", t.muted),
        ]);
        frame.render_widget(
            Paragraph::new(stats),
            Rect::new(r.copy.x, r.copy.y, r.copy.width, 1),
        );
        if r.copy.height >= 4 {
            frame.render_widget(
                Paragraph::new(span(
                    game.description.chars().take(500).collect::<String>(),
                    t.muted,
                ))
                .wrap(Wrap { trim: true }),
                Rect::new(r.copy.x, r.copy.y + 2, r.copy.width, r.copy.height - 2),
            );
        }
    }
    frame.render_widget(
        Paragraph::new(span("ctrl+←/→ photos  ·  f6 expand / restore", t.muted)),
        r.footer,
    );
}

fn picker(frame: &mut Frame, app: &App, t: Theme) {
    let p = app.picker.as_ref().unwrap();
    let r = picker_rects(frame.area(), p.expanded_preview);
    let matches = p.matches();
    frame.render_widget(Block::default().style(style(t.text).bg(t.bg)), r.outer);
    let head = Layout::horizontal([Constraint::Min(20), Constraint::Length(27)]).split(r.heading);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            bold("r o v e", t.accent),
            span("  /  discover", t.muted),
        ])),
        head[0],
    );
    frame.render_widget(
        Paragraph::new(if p.expanded_preview {
            "GALLERY / EXPANDED"
        } else {
            "find your next world."
        })
        .alignment(Alignment::Right)
        .style(style(t.muted)),
        head[1],
    );
    let row_height = picker_row_height(r.list);
    let slots = (r.list.height / row_height).max(1) as usize;
    let start = selection_start(p.selected, slots);
    if p.expanded_preview {
        // The full content area belongs to the gallery.
    } else if p.results.is_empty() {
        let direct = search_direct_hint(&p.input.text);
        let lines = if let Some(id) = direct {
            vec![
                Line::from(""),
                Line::from(bold("PLACE READY", t.accent)),
                Line::from(""),
                Line::from(format!("  {id}")),
                Line::from(""),
                Line::from(span("Press Enter to join this place.", t.muted)),
            ]
        } else {
            vec![
                Line::from(""),
                Line::from(bold("Find a world to drop into.", t.text)),
                Line::from(""),
                Line::from(span("  › Natural Disaster Survival", t.accent)),
                Line::from(""),
                Line::from(span("  › Obby", t.accent)),
                Line::from(""),
                Line::from(span("  › Driving", t.accent)),
                Line::from(""),
                Line::from(span("Type below, or click a suggestion.", t.muted)),
            ]
        };
        frame.render_widget(Paragraph::new(lines), r.list);
    } else if matches.is_empty() {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from(bold("No matches in these results.", t.text)),
                Line::from(""),
                Line::from(span(
                    "Shorten the filter, or Tab to search Roblox.",
                    t.muted,
                )),
            ])
            .wrap(Wrap { trim: true }),
            r.list,
        );
    } else {
        for (row, (index, filter_positions)) in matches.iter().enumerate().skip(start).take(slots) {
            let game = &p.results[*index];
            let selected = row == p.selected;
            let remote = crate::fuzzy::match_text(&game.name, &p.searched_for)
                .map(|(_, pos)| pos)
                .unwrap_or_default();
            let positions = if p.filtering {
                filter_positions
            } else {
                &remote
            };
            let mut title = vec![bold(if selected { " ▎ " } else { "   " }, t.accent)];
            title.extend(highlighted(&game.name, positions, t, selected));
            let subtitle = Line::from(vec![
                span(
                    format!(
                        "   {}",
                        if game.creator.is_empty() {
                            "Roblox experience"
                        } else {
                            &game.creator
                        }
                    ),
                    t.muted,
                ),
                span(" · ", t.muted),
                span(
                    game.players.map(compact).unwrap_or("—".into()),
                    if game.players.is_some() {
                        t.green
                    } else {
                        t.muted
                    },
                ),
                span(" playing", t.muted),
            ]);
            frame.render_widget(
                Paragraph::new(vec![Line::from(title), subtitle])
                    .style(Style::default().bg(if selected { t.raised } else { t.panel })),
                Rect::new(
                    r.list.x,
                    r.list.y + ((row - start) * row_height as usize) as u16,
                    r.list.width.saturating_sub(1),
                    2,
                ),
            );
        }
        if matches.len() > slots {
            let height = r.list.height as usize;
            let thumb = (height * slots / matches.len()).max(1);
            let offset = start * (height - thumb) / (matches.len() - slots).max(1);
            for y in 0..height {
                frame.render_widget(
                    Paragraph::new(if (offset..offset + thumb).contains(&y) {
                        "┃"
                    } else {
                        "│"
                    })
                    .style(style(if (offset..offset + thumb).contains(&y) {
                        t.accent
                    } else {
                        t.edge
                    })),
                    Rect::new(r.list.right() - 1, r.list.y + y as u16, 1, 1),
                );
            }
        }
    }
    if let Some(detail) = r.detail {
        preview(frame, app, t, detail);
    }
    let status = if let Some(feedback) = &p.feedback {
        feedback.clone()
    } else if p.searching {
        format!(
            " {} searching Roblox…",
            ["⠋", "⠙", "⠹", "⠸"][(app.started.elapsed().as_millis() / 130) as usize % 4]
        )
    } else if p.filtering {
        format!(
            " {} / {} results  ·  filter stays on this device",
            matches.len(),
            p.results.len()
        )
    } else {
        format!(" {}", p.message)
    };
    frame.render_widget(
        Paragraph::new(status).style(style(if p.searching { t.accent } else { t.muted })),
        r.message,
    );
    editor(
        frame,
        if p.filtering { &p.filter } else { &p.input },
        r.input,
        t,
        !app.help,
        false,
        if p.filtering {
            "filter results / local"
        } else {
            "search games"
        },
        if p.filtering {
            "type a few letters to narrow these results"
        } else {
            "game name, place ID, or Roblox link"
        },
    );
    let ready = (p.input.text.trim() == p.searched_for && p.selected_game().is_some())
        || crate::search::direct(&p.input.text).is_some();
    let label = if p.filtering && matches.is_empty() {
        "no selection"
    } else if ready {
        "enter  join this world →"
    } else {
        "enter  search →"
    };
    frame.render_widget(
        Paragraph::new(label)
            .alignment(Alignment::Center)
            .block(Block::default().padding(Padding::vertical(1)))
            .style(
                style(t.bg)
                    .bg(if p.filtering && matches.is_empty() {
                        t.muted
                    } else {
                        t.accent
                    })
                    .add_modifier(Modifier::BOLD),
            ),
        r.button,
    );
    frame.render_widget(
        Paragraph::new(if p.expanded_preview {
            "f6  back to results"
        } else {
            "f6  expand preview"
        })
        .alignment(Alignment::Center)
        .block(Block::default().padding(Padding::vertical(1)))
        .style(
            style(if p.selected_game().is_some() {
                t.text
            } else {
                t.muted
            })
            .bg(t.panel),
        ),
        r.toggle,
    );
    frame.render_widget(
        Paragraph::new(if p.expanded_preview {
            "←/→ photos   ↑↓ games   f6 / esc close preview   enter join"
        } else {
            "↑↓ select   tab filter   ctrl+←/→ photos   esc back   ctrl+k actions"
        })
        .style(style(t.muted)),
        r.footer,
    );
}
fn search_direct_hint(text: &str) -> Option<u64> {
    crate::search::place_id(text)
}
struct PaletteRects {
    outer: Rect,
    input: Rect,
    list: Rect,
    footer: Rect,
}
fn palette_rects(area: Rect) -> PaletteRects {
    let outer = centered(area, 86, 26);
    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(4),
        Constraint::Length(1),
    ])
    .spacing(1)
    .split(inset(outer, 2, 1));
    PaletteRects {
        outer,
        input: rows[0],
        list: rows[1],
        footer: rows[2],
    }
}
fn palette(frame: &mut Frame, app: &App, t: Theme) {
    let p = app.palette.as_ref().unwrap();
    let r = palette_rects(frame.area());
    popup(frame, r.outer, t, "commands");
    editor(
        frame,
        &p.input,
        r.input,
        t,
        true,
        false,
        "what would you like to do?",
        "search, theme, export, leave…",
    );
    let rows = actions::matching(&p.input.text);
    let slots = (r.list.height / 2).max(1) as usize;
    let start = selection_start(p.selected, slots);
    if rows.is_empty() {
        frame.render_widget(
            Paragraph::new("No actions match. Try a shorter phrase.").style(style(t.muted)),
            r.list,
        );
    }
    for (n, index) in rows.iter().enumerate().skip(start).take(slots) {
        let item = &actions::ITEMS[*index];
        let active = n == p.selected;
        let row = Rect::new(
            r.list.x,
            r.list.y + ((n - start) * 2) as u16,
            r.list.width,
            2,
        );
        let cols = Layout::horizontal([Constraint::Min(20), Constraint::Length(12)]).split(row);
        frame.render_widget(
            Block::default().style(Style::default().bg(if active { t.raised } else { t.panel })),
            row,
        );
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(bold(
                    format!(" {} {}", if active { "›" } else { " " }, item.name),
                    if active { t.accent } else { t.text },
                )),
                Line::from(span(format!("   {}", item.hint), t.muted)),
            ]),
            cols[0],
        );
        frame.render_widget(
            Paragraph::new(item.key)
                .alignment(Alignment::Right)
                .style(style(t.secondary)),
            cols[1],
        );
    }
    frame.render_widget(
        Paragraph::new(format!(
            "{} actions   ·   ↑↓ navigate   enter select   esc close",
            rows.len()
        ))
        .style(style(t.muted)),
        r.footer,
    );
}
fn help(frame: &mut Frame, t: Theme) {
    let r = centered(frame.area(), 86, 28);
    popup(frame, r, t, "keyboard guide");
    let inside = inset(r, 3, 2);
    let mut rows = vec![
        Line::from(bold("Everything is a few keys away.", t.accent)),
        Line::from(""),
        Line::from("ctrl+k / ctrl+p    searchable command menu"),
        Line::from("f2                 find a game by name, ID, or link"),
        Line::from("alt+1 / 2 / 3      overview / console / network"),
        Line::from("ctrl+t             cycle the 5 themes; starts with aurora"),
        Line::from("f8                 graph window: 30s / 2m / 10m"),
        Line::from("ctrl+e             activity filter: all / scripts / problems"),
        Line::from(""),
        Line::from(bold("WRITING LUAU", t.text)),
        Line::from("enter              run your source"),
        Line::from("alt+enter          newline; multiline paste waits for Enter"),
        Line::from("↑ / ↓              command history"),
        Line::from("ctrl+u / ctrl+l    clear input / clear activity"),
        Line::from(":run PATH          load a script file (up to 1 MiB)"),
        Line::from(":status ID         inspect a script job"),
        Line::from(":save PATH         export RAM measurements to a new CSV"),
        Line::from(""),
        Line::from("ctrl+d / ctrl+q    leave / leave and quit"),
        Line::from(span(
            "Search: Tab filters; F6 expands artwork; Esc returns.",
            t.muted,
        )),
        Line::from(span(
            "RAM uses decimal MB; total includes the dashboard process.",
            t.muted,
        )),
    ];
    if inside.height < 22 {
        // Keep every key binding readable in a standard 80x24 terminal.
        rows = rows
            .into_iter()
            .enumerate()
            .filter_map(|(i, row)| (!matches!(i, 0 | 1 | 8 | 9 | 17 | 19 | 20)).then_some(row))
            .collect();
    }
    frame.render_widget(
        Paragraph::new(rows),
        Rect::new(
            inside.x,
            inside.y,
            inside.width,
            inside.height.saturating_sub(1),
        ),
    );
    frame.render_widget(
        Paragraph::new(span("esc or f1 closes this guide", t.secondary)),
        Rect::new(inside.x, inside.bottom().saturating_sub(1), inside.width, 1),
    );
}
pub enum Hit {
    Action(Action),
    Palette(usize),
    Game(usize),
    Picture(usize),
    Preview,
    Join,
    Query(String),
    Commands,
}
pub fn hit(app: &App, area: Rect, x: u16, y: u16) -> Option<Hit> {
    let at = ratatui::layout::Position::new(x, y);
    if app.help {
        return None;
    }
    if let Some(p) = &app.palette {
        let r = palette_rects(area);
        if r.list.contains(at) {
            let rows = actions::matching(&p.input.text);
            let start = selection_start(p.selected, (r.list.height / 2).max(1) as usize);
            return rows
                .get(start + (y - r.list.y) as usize / 2)
                .map(|i| Hit::Palette(*i));
        }
        return None;
    }
    if let Some(p) = &app.picker {
        let r = picker_rects(area, p.expanded_preview);
        if let Some(detail) = r.detail {
            let inner = preview_inner(detail);
            let art = preview_regions(inner);
            if let Some(game) = p.selected_game() {
                let count = app
                    .images
                    .borrow()
                    .art
                    .get(game.place_id)
                    .map(|a| a.pictures.len())
                    .unwrap_or(0);
                for (index, slot) in art.thumbs.iter().enumerate().take(count) {
                    if slot.contains(at) {
                        return Some(Hit::Picture(index));
                    }
                }
            }
        }
        if r.toggle.contains(at) {
            return Some(Hit::Preview);
        }
        if r.button.contains(at) {
            return Some(Hit::Join);
        }
        if r.list.contains(at) {
            if p.results.is_empty() {
                let row = y - r.list.y;
                return match row {
                    3 => Some(Hit::Query("Natural Disaster Survival".into())),
                    5 => Some(Hit::Query("Obby".into())),
                    7 => Some(Hit::Query("Driving".into())),
                    _ => None,
                };
            }
            let row_height = picker_row_height(r.list);
            let start = selection_start(p.selected, (r.list.height / row_height).max(1) as usize);
            let index = start + (y - r.list.y) as usize / row_height as usize;
            if index < p.matches().len() {
                return Some(Hit::Game(index));
            }
        }
        return None;
    }
    let r = regions(area, app);
    if let Some(side) = r.sidebar {
        for (row, action) in sidebar_rows(side) {
            if row.contains(at) {
                return Some(Hit::Action(action));
            }
        }
    }
    if app.view == View::Overview && phase(app) == "idle" && app.samples.is_empty() {
        let inside = inset(
            panel("your workspace", THEMES[app.theme]).inner(r.body),
            1,
            0,
        );
        for (rect, action) in welcome_actions(inside) {
            if rect.contains(at) {
                return Some(Hit::Action(action));
            }
        }
    }
    let tabs = tab_boxes(r.tabs);
    for (i, v) in [View::Overview, View::Console, View::Network]
        .iter()
        .enumerate()
    {
        if tabs[i].contains(at) {
            return Some(Hit::Action(Action::View(*v)));
        }
    }
    if tabs[4].contains(at) {
        return Some(Hit::Commands);
    }
    if app.view != View::Console {
        let (graphs, _) = graph_regions(r.body);
        let (graph, _) = if app.view == View::Network {
            (graphs, None)
        } else {
            body_regions(graphs)
        };
        if y == graph.y + 1 && x >= graph.right().saturating_sub(12) && x < graph.right() {
            return Some(Hit::Action(Action::Window));
        }
    }
    if r.header.contains(at) && x > r.header.right().saturating_sub(21) {
        return Some(Hit::Action(Action::Theme));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{app::Sample, backend::Options};
    use ratatui::backend::TestBackend;
    #[test]
    fn compact_help_keeps_commands_and_exit_visible() {
        let mut app = App::new(Options::default(), None);
        app.picker = None;
        app.help = true;
        for (w, h) in [(80, 24), (68, 22)] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal.draw(|f| draw(f, &app)).unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect();
            for label in [
                "ctrl+k / ctrl+p",
                ":run PATH",
                ":save PATH",
                "ctrl+d / ctrl+q",
                "esc or f1 closes",
            ] {
                assert!(text.contains(label), "missing {label} at {w}x{h}");
            }
        }
    }
    #[test]
    fn all_views_and_overlays_fit_supported_terminal_sizes() {
        let mut app = App::new(Options::default(), None);
        for (w, h) in [(140, 46), (100, 32), (80, 24), (68, 22), (30, 10), (1, 1)] {
            for view in [View::Overview, View::Console, View::Network] {
                app.view = view;
                for theme in 0..THEMES.len() {
                    app.theme = theme;
                    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
                    terminal.draw(|f| draw(f, &app)).unwrap();
                }
            }
        }
        app.picker = None;
        app.theme = 0;
        app.view = View::Overview;
        app.samples.push_back(Sample {
            time: 1.0,
            client_bytes: 50_000_000,
            ui_bytes: 10_000_000,
            rtt_ms: None,
        });
        let mut terminal = Terminal::new(TestBackend::new(140, 46)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("client + dashboard"));
        assert!(text.contains("memory over time"));
        assert!(text.contains("budget 128 MB (auto, 10 MB buffer)"));
        app.ram_budget.observe(118_000_000);
        terminal.draw(|f| draw(f, &app)).unwrap();
        let grown: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(grown.contains("budget 138 MB (auto, 10 MB buffer)"));
        app.palette = Some(crate::actions::Palette::default());
        terminal.draw(|f| draw(f, &app)).unwrap();
        app.help = true;
        terminal.draw(|f| draw(f, &app)).unwrap();
    }
    #[test]
    fn compact_metrics_keep_values_and_units_readable() {
        let mut app = App::new(Options::default(), None);
        app.picker = None;
        app.telemetry = serde_json::json!({"phase":"connected","rss_bytes":51200000,
            "peak_sampled_rss_bytes":65000000,"rtt_ms":42,"connected_seconds":65});
        for (w, h) in [(68, 22), (80, 24), (100, 32), (140, 46)] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal.draw(|f| draw(f, &app)).unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect();
            for value in ["51.2 MB", "65.0 MB", "42 ms", "00:01:05"] {
                assert!(text.contains(value), "missing {value} at {w}x{h}");
            }
        }
    }
    #[test]
    fn welcome_actions_are_visible_and_clickable_at_compact_sizes() {
        let mut app = App::new(Options::default(), None);
        app.picker = None;
        for (w, h) in [(68, 22), (80, 24), (140, 46)] {
            let area = Rect::new(0, 0, w, h);
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal.draw(|f| draw(f, &app)).unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect();
            assert!(text.contains("find a game"));
            assert!(text.contains("try local sandbox"));
            let body = regions(area, &app).body;
            let inside = inset(panel("your workspace", THEMES[app.theme]).inner(body), 1, 0);
            for (r, action) in welcome_actions(inside) {
                assert!(matches!(hit(&app,area,r.x+2,r.y),Some(Hit::Action(a)) if a==action));
            }
        }
    }
    #[test]
    fn discover_rows_and_expanded_gallery_keep_click_targets_in_bounds() {
        let mut app = App::new(Options::default(), None);
        app.picker.as_mut().unwrap().results = (1..=20)
            .map(|n| crate::search::direct(&n.to_string()).unwrap())
            .collect();
        for (w, h) in [(68, 22), (80, 24), (100, 32), (140, 46)] {
            for expanded in [false, true] {
                app.picker.as_mut().unwrap().expanded_preview = expanded;
                let area = Rect::new(0, 0, w, h);
                let r = picker_rects(area, expanded);
                let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
                terminal.draw(|f| draw(f, &app)).unwrap();
                assert!(matches!(
                    hit(&app, area, r.toggle.x + 1, r.toggle.y + 1),
                    Some(Hit::Preview)
                ));
                assert!(matches!(
                    hit(&app, area, r.button.x + 1, r.button.y + 1),
                    Some(Hit::Join)
                ));
                if expanded {
                    assert!(r.list.is_empty());
                    assert!(r.detail.is_some());
                    assert!(r.input.is_empty());
                } else {
                    let row = picker_row_height(r.list);
                    assert!(matches!(
                        hit(&app, area, r.list.x + 1, r.list.y + row),
                        Some(Hit::Game(1))
                    ));
                }
                if let Some(detail) = r.detail {
                    let inner = preview_inner(detail);
                    let preview = preview_regions(inner);
                    for rect in [preview.heading, preview.hero, preview.copy, preview.footer] {
                        assert!(rect.bottom() <= inner.bottom(), "preview overflows {w}x{h}");
                    }
                }
            }
        }
    }

    #[test]
    fn pointer_targets_follow_the_same_layout_as_rendering() {
        let mut app = App::new(Options::default(), None);
        app.picker = None;
        let area = Rect::new(0, 0, 140, 46);
        let r = regions(area, &app);
        let (row, _) = sidebar_rows(r.sidebar.unwrap())
            .into_iter()
            .find(|(_, a)| *a == Action::View(View::Console))
            .unwrap();
        assert!(matches!(
            hit(&app, area, row.x + 1, row.y),
            Some(Hit::Action(Action::View(View::Console)))
        ));
        let tabs = tab_boxes(r.tabs);
        assert!(matches!(
            hit(&app, area, tabs[1].x + 2, tabs[1].y),
            Some(Hit::Action(Action::View(View::Console)))
        ));
        app.palette = Some(crate::actions::Palette::default());
        let p = palette_rects(area);
        assert!(matches!(
            hit(&app, area, p.list.x + 2, p.list.y),
            Some(Hit::Palette(0))
        ));
    }
}

#[cfg(test)]
#[path = "ui/preview_tests.rs"]
mod preview_tests;

fn welcome_actions(area: Rect) -> [(Rect, Action); 2] {
    dashboard::welcome_actions(area)
}
fn welcome(frame: &mut Frame, app: &App, r: Rect, t: Theme) {
    dashboard::welcome(frame, app, r, t)
}
fn session_card(frame: &mut Frame, app: &App, r: Rect, t: Theme) {
    let Some(Target::Game(game)) = app.target.as_ref() else {
        return;
    };
    let b = panel("in this world", t);
    let inside = inset(b.inner(r), 1, 0);
    frame.render_widget(b, r);
    let cover = Rect::new(
        inside.x,
        inside.y,
        inside.width,
        inside.height.saturating_sub(5),
    );
    if workspace_visible(app) {
        let art = app
            .images
            .borrow()
            .art
            .get(game.place_id)
            .and_then(|art| art.pictures.first())
            .cloned();
        if let Some(image) = art {
            app.images.borrow_mut().draw(
                frame,
                Slot::Session,
                ImageKey::new(game.place_id, 0, app.theme, cover),
                cover,
                t,
                || Source::Picture(image),
            );
        } else {
            draw_mark(frame, app, Slot::Session, cover, t);
        }
    }
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(bold(game.name.clone(), t.text)),
            Line::from(span(game.creator.clone(), t.muted)),
            Line::from(""),
            Line::from(vec![
                span("● ", phase_color(app, t)),
                span(phase(app), phase_color(app, t)),
                span("  ·  f2 switch", t.muted),
            ]),
        ]),
        Rect::new(inside.x, cover.bottom() + 1, inside.width, 4),
    );
}
