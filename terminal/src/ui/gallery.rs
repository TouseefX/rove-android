//! Reusable artwork treatment for the discover gallery.
use super::*;

pub(super) fn placeholder(frame: &mut Frame, area: Rect, t: Theme, title: &str, detail: &str) {
    if area.is_empty() {
        return;
    }
    frame.render_widget(Block::default().style(style(t.text).bg(t.panel)), area);
    let inset = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    );
    if inset.height < 3 {
        return;
    }
    let y = inset.y + inset.height / 2;
    let rule = "─".repeat(inset.width.min(18) as usize);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            span("·  ", t.edge),
            span(rule, t.edge),
            span("  ·", t.edge),
        ]))
        .alignment(Alignment::Center),
        Rect::new(inset.x, y.saturating_sub(1), inset.width, 1),
    );
    if inset.height >= 5 {
        frame.render_widget(
            Paragraph::new(bold(title, t.text)).alignment(Alignment::Center),
            Rect::new(inset.x, y, inset.width, 1),
        );
        if inset.height >= 6 {
            frame.render_widget(
                Paragraph::new(span(detail, t.muted)).alignment(Alignment::Center),
                Rect::new(inset.x, y + 1, inset.width, 1),
            );
        }
    } else {
        frame.render_widget(
            Paragraph::new(span(title, t.muted)).alignment(Alignment::Center),
            Rect::new(inset.x, y, inset.width, 1),
        );
    }
}

pub(super) fn thumbnails(
    frame: &mut Frame,
    app: &App,
    t: Theme,
    areas: &[Rect],
    game_id: u64,
    pictures: &[std::sync::Arc<image::DynamicImage>],
    selected: usize,
    active: bool,
) {
    let slots = [Slot::Thumb0, Slot::Thumb1, Slot::Thumb2];
    for (index, area) in areas.iter().copied().take(3).enumerate() {
        let chosen = index == selected;
        let edge = if chosen { t.accent } else { t.edge };
        frame.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(style(edge))
                .style(style(t.text).bg(t.panel)),
            area,
        );
        if let Some(image) = pictures.get(index) {
            let inner = Rect::new(
                area.x.saturating_add(1),
                area.y.saturating_add(1),
                area.width.saturating_sub(2),
                area.height.saturating_sub(3),
            );
            if active && !inner.is_empty() {
                app.images.borrow_mut().draw(
                    frame,
                    slots[index],
                    ImageKey::new(game_id, 0, app.theme, inner),
                    inner,
                    t,
                    || Source::Picture(image.clone()),
                );
            }
        }
        if area.height >= 2 && area.width >= 4 {
            let badge = Rect::new(
                area.x + 1,
                area.bottom() - 2,
                area.width.saturating_sub(2),
                1,
            );
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    bold(
                        format!("{:02}", index + 1),
                        if chosen { t.accent } else { t.muted },
                    ),
                    span(if chosen { "  selected" } else { "  preview" }, t.muted),
                ]))
                .style(style(t.text).bg(t.panel)),
                badge,
            );
        }
    }
}
