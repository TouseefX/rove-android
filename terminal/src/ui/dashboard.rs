//! Main dashboard composition: metrics, workspace rail and welcome view.
use super::*;

pub(super) fn header(frame: &mut Frame, app: &App, r: Rect, t: Theme) {
    let c = Layout::horizontal([
        Constraint::Length(25),
        Constraint::Min(18),
        Constraint::Length(24),
    ])
    .split(r);
    let mark = Rect::new(c[0].x, c[0].y, 6, r.height.min(3));
    if !workspace_visible(app) || !draw_mark(frame, app, Slot::Brand, mark, t) {
        frame.render_widget(Paragraph::new(bold(" ◇ ", t.accent)), mark);
    }
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(bold("r o v e", t.text)),
            Line::from(span("SESSION WORKSPACE", t.muted)),
        ]),
        Rect::new(c[0].x + 7, c[0].y, 18, r.height),
    );
    let name = app
        .target
        .as_ref()
        .map(Target::name)
        .unwrap_or("make yourself at home.");
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(bold(name, t.text)),
            Line::from(span(
                format!(
                    "{}   /   {}",
                    if matches!(app.target, Some(Target::Sandbox)) {
                        "local sandbox"
                    } else {
                        "workspace"
                    },
                    app.view.label()
                ),
                t.muted,
            )),
        ]),
        c[1],
    );
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                format!(" {} ", phase_label(app)),
                style(phase_color(app, t)).bg(t.raised),
            )),
            Line::from(span(format!("{}  ·  ctrl+t", t.name), t.muted)),
        ])
        .alignment(Alignment::Right),
        c[2],
    );
    if r.height >= 4 {
        frame.render_widget(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(style(t.edge)),
            r,
        );
    }
}

pub(super) fn sidebar_rows(r: Rect) -> Vec<(Rect, Action)> {
    let inside = inset(r, 1, 1);
    [
        (1, Action::Search),
        (5, Action::View(View::Overview)),
        (6, Action::View(View::Console)),
        (7, Action::View(View::Network)),
        (11, Action::Load),
        (12, Action::Export),
        (13, Action::Sandbox),
    ]
    .into_iter()
    .filter_map(|(offset, action)| {
        (offset < inside.height).then_some((
            Rect::new(inside.x, inside.y + offset, inside.width, 1),
            action,
        ))
    })
    .collect()
}
pub(super) fn sidebar(frame: &mut Frame, app: &App, r: Rect, t: Theme) {
    frame.render_widget(Block::default().style(style(t.text).bg(t.panel)), r);
    let inside = inset(r, 2, 1);
    for ((row, action), label) in sidebar_rows(r).into_iter().zip([
        "  + find a game     f2",
        "  1 overview",
        "  2 console",
        "  3 network",
        "  ↗ open script",
        "  ↓ export samples",
        "  ◇ local sandbox",
    ]) {
        let active = matches!(action,Action::View(v) if v==app.view);
        let text = if active {
            label.replacen("  ", "▎ ", 1)
        } else {
            label.to_owned()
        };
        let style = if active {
            style(view_color(app.view, t))
                .bg(t.raised)
                .add_modifier(Modifier::BOLD)
        } else if action == Action::Search {
            style(t.secondary).bg(t.raised)
        } else {
            style(t.muted)
        };
        frame.render_widget(Paragraph::new(text).style(style), row);
    }
    for (offset, label) in [(3, "YOUR SPACE"), (9, "TOOLS"), (16, "SESSION")] {
        if offset < inside.height {
            frame.render_widget(
                Paragraph::new(bold(label, t.muted)),
                Rect::new(inside.x, inside.y + offset, inside.width, 1),
            );
        }
    }
    let sandbox = matches!(app.target, Some(Target::Sandbox));
    let info = vec![
        Line::from(bold(
            if sandbox {
                "◇  LOCAL SANDBOX"
            } else {
                "●  ROBLOX"
            },
            t.secondary,
        )),
        Line::from(span(
            app.telemetry["account"].as_str().unwrap_or(if sandbox {
                "offline sandbox"
            } else {
                "no account connected"
            }),
            t.muted,
        )),
        Line::from(""),
        Line::from(span(
            if app.telemetry["luau_ready"] == true {
                "●  Luau ready"
            } else {
                "○  Luau waiting"
            },
            if app.telemetry["luau_ready"] == true {
                t.green
            } else {
                t.muted
            },
        )),
        Line::from(span(
            if sandbox {
                "○  transport offline"
            } else if app.telemetry["connected_seconds"].is_number() {
                "●  transport connected"
            } else {
                "○  transport waiting"
            },
            t.muted,
        )),
    ];
    if inside.height > 19 {
        frame.render_widget(
            Paragraph::new(info),
            Rect::new(inside.x, inside.y + 18, inside.width, 5),
        );
    }
    if inside.height >= 31 {
        let values = [
            (
                "PHYSICS",
                if app.options.physics { "on" } else { "off" }.to_owned(),
            ),
            (
                "WORLD",
                if app.options.lightweight {
                    "lightweight"
                } else {
                    "full"
                }
                .to_owned(),
            ),
            ("LUAU", format!("{} MiB", app.options.memory_mib)),
            (
                "BUDGET",
                format!("{:.0} MB", mb(app.ram_budget.limit_bytes())),
            ),
        ];
        for (i, (key, value)) in values.into_iter().enumerate() {
            let y = inside.bottom().saturating_sub(7) + i as u16;
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    span(format!("{key:<11}"), t.muted),
                    bold(value, t.text),
                ])),
                Rect::new(inside.x, y, inside.width, 1),
            );
        }
        frame.render_widget(
            Paragraph::new(span("adaptive · 10 MB buffer", t.edge)),
            Rect::new(inside.x, inside.bottom() - 2, inside.width, 1),
        );
    }
    if inside.height > 2 {
        frame.render_widget(
            Paragraph::new(span("ctrl+k  all actions", t.secondary)),
            Rect::new(inside.x, inside.bottom() - 1, inside.width, 1),
        );
    }
}

fn items(app: &App) -> Vec<(&'static str, String, Color)> {
    let d = &app.telemetry;
    if app.view == View::Network {
        return vec![
            (
                "TRANSPORT RTT",
                d["rtt_ms"]
                    .as_f64()
                    .map(|x| format!("{x:.0} ms"))
                    .unwrap_or("—".into()),
                tint(app, 0),
            ),
            ("PING REQUESTS", display(&d["ping_requests"]), tint(app, 1)),
            ("REPLIES SENT", display(&d["ping_replies"]), tint(app, 2)),
            ("PENDING", display(&d["ping_pending"]), tint(app, 3)),
        ];
    }
    let elapsed = d["connected_seconds"]
        .as_f64()
        .or_else(|| d["elapsed_seconds"].as_f64());
    vec![
        (
            "CLIENT RSS",
            d["rss_bytes"]
                .as_u64()
                .map(|v| format!("{:.1} MB", mb(v)))
                .unwrap_or("—".into()),
            tint(app, 0),
        ),
        (
            "SAMPLED PEAK",
            d["peak_sampled_rss_bytes"]
                .as_u64()
                .map(|v| format!("{:.1} MB", mb(v)))
                .unwrap_or("—".into()),
            tint(app, 1),
        ),
        (
            "TRANSPORT RTT",
            d["rtt_ms"]
                .as_f64()
                .map(|x| format!("{x:.0} ms"))
                .unwrap_or("—".into()),
            tint(app, 2),
        ),
        (
            if d["connected_seconds"].is_number() {
                "CONNECTED"
            } else {
                "WORKER TIME"
            },
            elapsed.map(duration).unwrap_or("—".into()),
            tint(app, 3),
        ),
    ]
}
fn tint(app: &App, index: usize) -> Color {
    let t = THEMES[app.theme];
    if app.view == View::Network {
        [t.blue, t.accent, t.green, t.amber][index]
    } else {
        [t.accent, t.amber, t.blue, t.green][index]
    }
}
fn big_digit(c: char) -> [String; 3] {
    let pixels = match c {
        '0' => [7, 5, 5, 5, 7],
        '1' => [2, 6, 2, 2, 7],
        '2' => [7, 1, 7, 4, 7],
        '3' => [7, 1, 7, 1, 7],
        '4' => [5, 5, 7, 1, 1],
        '5' => [7, 4, 7, 1, 7],
        '6' => [7, 4, 7, 5, 7],
        '7' => [7, 1, 1, 1, 1],
        '8' => [7, 5, 7, 5, 7],
        '9' => [7, 5, 7, 1, 7],
        '.' => [0, 0, 0, 0, 2],
        ':' => [0, 2, 0, 2, 0],
        _ => [0; 5],
    };
    std::array::from_fn(|row| {
        (0..3)
            .map(|col| {
                let top = pixels[row * 2] & (1 << (2 - col)) != 0;
                let bottom = pixels
                    .get(row * 2 + 1)
                    .is_some_and(|n| n & (1 << (2 - col)) != 0);
                match (top, bottom) {
                    (true, true) => '█',
                    (true, false) => '▀',
                    (false, true) => '▄',
                    _ => ' ',
                }
            })
            .collect()
    })
}
pub(super) fn metrics(frame: &mut Frame, app: &App, r: Rect, t: Theme) {
    let data = items(app);
    let cols = Layout::horizontal([Constraint::Ratio(1, 4); 4])
        .spacing(1)
        .split(r);
    for (i, (label, value, color)) in data.into_iter().enumerate() {
        let w = cols[i];
        let card = Block::default()
            .borders(Borders::LEFT)
            .padding(Padding::horizontal(1))
            .border_style(style(if value == "—" { t.edge } else { color }))
            .style(style(t.text).bg(t.panel));
        let inner = card.inner(w);
        frame.render_widget(card, w);
        let raw = value.split_whitespace().next().unwrap_or("—");
        let numeric = if raw.len() == 8 && raw.contains(':') {
            if raw.starts_with("00:") {
                &raw[3..]
            } else {
                &raw[..5]
            }
        } else {
            raw
        };
        if r.height >= 7
            && value != "—"
            && numeric.chars().count() <= 5
            && numeric.chars().count() * 4 <= inner.width as usize
        {
            frame.render_widget(
                Paragraph::new(span(label, t.muted)),
                Rect::new(inner.x, inner.y, inner.width, 1),
            );
            for (line_no, y) in (0..3).zip(inner.y + 1..inner.y + 4) {
                let mut spans = Vec::new();
                for c in numeric.chars() {
                    spans.push(bold(big_digit(c)[line_no].clone(), color));
                    spans.push(span(" ", color));
                }
                frame.render_widget(
                    Paragraph::new(Line::from(spans)),
                    Rect::new(inner.x, y, inner.width, 1),
                );
            }
            frame.render_widget(
                Paragraph::new(bold(value, t.text)),
                Rect::new(inner.x, inner.y + 5, inner.width, 1),
            );
            let track = match label {
                "CLIENT RSS" => "live sample",
                "SAMPLED PEAK" => "peak sampled",
                "TRANSPORT RTT" => "measured latency",
                "PING REQUESTS" => "received",
                "REPLIES SENT" => "sent this session",
                "PENDING" => "awaiting reply",
                _ => "session clock",
            };
            frame.render_widget(
                Paragraph::new(span(track, t.muted)),
                Rect::new(inner.x, inner.y + 6, inner.width, 1),
            );
        } else {
            let short = if w.width < 22 {
                match label {
                    "CLIENT RSS" => "MEMORY",
                    "SAMPLED PEAK" => "PEAK",
                    "TRANSPORT RTT" => "LATENCY",
                    "PING REQUESTS" => "REQUESTS",
                    "REPLIES SENT" => "REPLIES",
                    "WORKER TIME" => "WORKER TIME",
                    _ => label,
                }
            } else {
                label
            };
            let value_color = if value == "—" { t.muted } else { color };
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(span(short, t.muted)),
                    Line::from(bold(value, value_color)),
                ]),
                Rect::new(inner.x, inner.y + u16::from(inner.height >= 4),
                    inner.width, inner.height.saturating_sub(u16::from(inner.height >= 4))),
            );
        }
    }
}

pub(super) fn welcome_actions(area: Rect) -> [(Rect, Action); 2] {
    let width = 26.min(area.width);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(3);
    [
        (Rect::new(x, y, width, 1), Action::Search),
        (Rect::new(x, y + 1, width, 1), Action::Sandbox),
    ]
}
pub(super) fn welcome(frame: &mut Frame, app: &App, r: Rect, t: Theme) {
    let block = panel("  A WORLD OF YOUR OWN  ", t);
    let inside = inset(block.inner(r), 1, 0);
    frame.render_widget(block, r);
    if inside.height < 5 {
        return;
    }
    let band = Layout::horizontal([Constraint::Percentage(48), Constraint::Percentage(52)]).split(
        Rect::new(
            inside.x,
            inside.y,
            inside.width,
            inside.height.saturating_sub(5),
        ),
    );
    let mark = Rect::new(
        band[0].x + band[0].width.saturating_sub(24) / 2,
        band[0].y + 1,
        band[0].width.min(24),
        band[0].height.min(12),
    );
    if workspace_visible(app) && mark.height >= 3
        && !draw_mark(frame, app, Slot::Welcome, mark, t) {
        frame.render_widget(
            Paragraph::new(vec![Line::from(bold("r o v e", t.accent)),
                Line::from(""), Line::from(span("your session, in focus", t.muted))])
                .alignment(Alignment::Center),
            Rect::new(mark.x, mark.y + mark.height.saturating_sub(3) / 2, mark.width, 3),
        );
    }
    let copy = Rect::new(
        band[1].x,
        band[1].y + 1,
        band[1].width,
        band[1].height.saturating_sub(1),
    );
    let title = if app.target.is_some() {
        "your next world is waiting."
    } else {
        "make room for a new adventure."
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(span("ROVE  /  SESSION STUDIO", t.secondary)),
            Line::from(""),
            Line::from(bold(title, t.text)),
            Line::from(""),
            Line::from(span(
                "Find a game, preview the world, then\nwatch memory and network activity live.",
                t.muted,
            )),
            Line::from(""),
            Line::from(span("F2  DISCOVER     ·     CTRL+K  COMMANDS", t.accent)),
        ])
        .wrap(Wrap { trim: true }),
        copy,
    );
    let y = inside.y + inside.height.saturating_sub(4);
    frame.render_widget(
        Block::default()
            .borders(Borders::TOP)
            .border_style(style(t.edge)),
        Rect::new(inside.x, y, inside.width, 3),
    );
    for (i, (area, _)) in welcome_actions(inside).iter().enumerate() {
        frame.render_widget(
            Paragraph::new(if i == 0 {
                "  f2   find a game       →  "
            } else {
                "     try local sandbox     "
            })
            .style(if i == 0 {
                style(t.bg).bg(t.accent).add_modifier(Modifier::BOLD)
            } else {
                style(t.secondary).bg(t.raised)
            })
            .alignment(Alignment::Center),
            *area,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn oversized_numbers_remain_unambiguous() {
        let digits: std::collections::HashSet<_> = ('0'..='9').map(big_digit).collect();
        assert_eq!(digits.len(), 10);
        assert!(big_digit('.').iter().any(|row| row.contains('▀')));
    }
}
