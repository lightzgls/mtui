//! Mixed search rows and provider category filters share keyboard/mouse actions.
use super::*;
use crate::source::search::Filter;

pub(super) fn render(frame: &mut Frame, app: &mut App, area: Rect, mouse: &mut MouseMap) {
    let area = shell::inset(area);
    let [header, filters, list] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(if area.width < 62 { 3 } else { 2 }),
        Constraint::Min(0),
    ])
    .areas(area);
    frame.render_widget(
        Paragraph::new(truncate(
            &format!("Results for “{}”", app.query),
            header.width as usize,
        ))
        .style(
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        header,
    );
    let mut x = filters.x;
    let mut y = filters.y;
    for (index, filter) in Filter::ALL.into_iter().enumerate() {
        let label = format!("{} {}", index + 1, filter.label());
        let width = (display_width(&label) as u16).min(filters.width);
        if x + width > filters.right() {
            x = filters.x;
            y += 1;
        }
        if y >= filters.bottom() {
            break;
        }
        let rect = Rect::new(x, y, width, 1);
        let style = if app.search_filter == filter {
            Style::default()
                .fg(ambient(app))
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            Style::default().fg(Color::Gray)
        };
        frame.render_widget(
            Paragraph::new(truncate(&label, width as usize)).style(style),
            rect,
        );
        mouse
            .targets
            .push(MouseTarget::Area(rect, MouseAction::SearchFilter(filter)));
        x += width + 3;
    }
    let height = if list.height >= 4 && list.width >= 32 {
        3
    } else {
        1
    };
    let visible = usize::from(list.height / height);
    app.clamp_scroll(visible);
    if app.search_items.is_empty() {
        message(
            frame,
            if app.busy {
                "Searching YouTube Music…"
            } else {
                "No results in this category."
            },
            list,
        );
        return;
    }
    let accent = ambient(app);
    let use_images = app.kitty_images() && !app.overlay.is_open() && app.menu().is_none();
    let mut wanted = Vec::new();
    let mut tiles = Tiles {
        shape: CardShape::Tile,
        art: &app.art,
        wanted: &mut wanted,
        images: use_images.then_some((&mut app.images, app.graphics)),
    };
    for (slot, index) in (app.offset..app.search_items.len())
        .take(visible)
        .enumerate()
    {
        let item = &app.search_items[index];
        let row = Rect::new(list.x, list.y + slot as u16 * height, list.width, height);
        let selected = index == app.selected;
        if selected {
            frame.render_widget(
                Block::default().style(Style::default().bg(shell::SURFACE)),
                row,
            );
        }
        let thumb = if height > 1 {
            browse::square_cols(2, app.graphics)
                .min(8)
                .min(row.width / 4)
        } else {
            0
        };
        let start = if thumb > 0 { thumb + 2 } else { 2 };
        if thumb > 0 {
            browse::artwork(
                frame,
                Rect::new(row.x, row.y, thumb, 2),
                item.card.art_key(),
                item.card.art.clone(),
                &mut tiles,
            );
        } else {
            frame.render_widget(
                Paragraph::new(if selected { ">" } else { " " }).style(Style::default().fg(accent)),
                Rect::new(row.x, row.y, 1, 1),
            );
        }
        let text = Rect::new(
            row.x + start,
            row.y,
            row.width.saturating_sub(start),
            height.min(2),
        );
        let metadata = format!(
            "{} · {}",
            item.kind.label().trim_end_matches('s'),
            item.card.detail()
        );
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    focused_text(&item.card.title, text.width as usize, selected),
                    Style::default().fg(Color::White).add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
                )),
                Line::from(Span::styled(
                    truncate(&metadata, text.width as usize),
                    Style::default().fg(Color::Gray),
                )),
            ]),
            text,
        );
        mouse
            .targets
            .push(MouseTarget::Area(row, MouseAction::PlayTrack(index)));
    }
    app.want_art(wanted);
}
