//! Library chrome, compact collection cards, song rows and explicit paging.

use super::*;
use crate::source::library::browse::Category;

#[allow(clippy::too_many_arguments)]
fn button(
    frame: &mut Frame,
    mouse: &mut MouseMap,
    area: Rect,
    label: &str,
    active: bool,
    enabled: bool,
    action: MouseAction,
    accent: Color,
) {
    if area.is_empty() {
        return;
    }
    let style = if active {
        highlight(accent).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(if enabled {
                Color::Gray
            } else {
                Color::DarkGray
            })
            .bg(shell::SURFACE)
    };
    frame.render_widget(
        Paragraph::new(Span::styled(truncate(label, area.width as usize), style)),
        area,
    );
    if enabled {
        mouse.targets.push(MouseTarget::Area(area, action));
    }
}

fn categories(
    frame: &mut Frame,
    mouse: &mut MouseMap,
    area: Rect,
    selected: Category,
    accent: Color,
) {
    if area.is_empty() {
        return;
    }
    if area.width >= 72 {
        let mut x = area.x;
        for (index, category) in Category::ALL.into_iter().enumerate() {
            let label = format!(" {} {} ", index + 1, category.label());
            let width = display_width(&label) as u16;
            button(
                frame,
                mouse,
                Rect::new(x, area.y, width, 1),
                &label,
                category == selected,
                true,
                MouseAction::LibraryCategory(category),
                accent,
            );
            x += width + 1;
        }
    } else {
        let label = format!("{} {}", selected.index() + 1, selected.label());
        let width = area.width.saturating_sub(8);
        frame.render_widget(
            Paragraph::new(Span::styled(
                truncate(&label, width as usize),
                highlight(accent),
            ))
            .alignment(ratatui::layout::Alignment::Center),
            Rect::new(area.x + area.width.min(4), area.y, width, 1),
        );
        let previous = Category::ALL[(selected.index() + 5) % 6];
        let next = Category::ALL[(selected.index() + 1) % 6];
        button(
            frame,
            mouse,
            Rect::new(area.x, area.y, area.width.min(3), 1),
            " < ",
            false,
            true,
            MouseAction::LibraryCategory(previous),
            accent,
        );
        if area.width >= 7 {
            button(
                frame,
                mouse,
                Rect::new(area.right() - 3, area.y, 3, 1),
                " > ",
                false,
                true,
                MouseAction::LibraryCategory(next),
                accent,
            );
        }
    }
}

pub(super) fn render(frame: &mut Frame, app: &mut App, area: Rect, mouse: &mut MouseMap) {
    let inner = shell::inset(area);
    if inner.is_empty() {
        return;
    }
    let accent = ambient(app);
    let account = if app.library.connected {
        app.library.account.as_deref().unwrap_or("YouTube Music")
    } else {
        "Not signed in"
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "Library",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    " · {}",
                    truncate(account, inner.width.saturating_sub(10) as usize)
                ),
                Style::default().fg(Color::Gray),
            ),
        ])),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    if inner.height < 2 {
        return;
    }
    categories(
        frame,
        mouse,
        Rect::new(inner.x, inner.y + 1, inner.width, 1),
        app.library.category,
        accent,
    );
    if inner.height < 3 {
        return;
    }
    let tools = Rect::new(inner.x, inner.y + 2, inner.width, 1);
    if app.library.connected {
        let liked_width = tools.width.min(17);
        button(
            frame,
            mouse,
            Rect::new(tools.x, tools.y, liked_width, 1),
            " Liked Music [f] ",
            false,
            true,
            MouseAction::OpenLikedMusic,
            accent,
        );
        let has_sort = app.library.collection().sorts.len() > 1;
        let refresh_width = tools
            .width
            .saturating_sub(liked_width)
            .min(if tools.width >= 60 { 13 } else { 5 });
        let sort_width = tools.width.saturating_sub(liked_width + refresh_width + 2);
        if has_sort && sort_width >= 5 {
            let label = if sort_width >= 18 {
                format!(" Sort: {} [o] ", app.library.collection().sort_label)
            } else {
                "Sort".into()
            };
            button(
                frame,
                mouse,
                Rect::new(tools.x + liked_width + 1, tools.y, sort_width, 1),
                &label,
                false,
                !app.library.loading(),
                MouseAction::LibrarySort,
                accent,
            );
        }
        if refresh_width > 0 {
            button(
                frame,
                mouse,
                Rect::new(tools.right() - refresh_width, tools.y, refresh_width, 1),
                if refresh_width >= 13 {
                    " Refresh [r] "
                } else {
                    " [r] "
                },
                false,
                !app.library.loading(),
                MouseAction::RefreshLibrary,
                accent,
            );
        }
    } else {
        button(
            frame,
            mouse,
            Rect::new(tools.x, tools.y, tools.width.min(15), 1),
            " Sign in [M] ",
            false,
            true,
            MouseAction::LibrarySignIn,
            accent,
        );
    }
    let heading = if inner.height >= 10 { 4 } else { 3 };
    let content = Rect::new(
        inner.x,
        inner.y + heading,
        inner.width,
        inner.height.saturating_sub(heading + 1),
    );
    let footer = Rect::new(inner.x, inner.bottom() - 1, inner.width, 1);
    let songs = app.library.category == Category::Songs;
    let shape = if content.width >= 36 && content.height >= 5 {
        CardShape::Tile
    } else {
        CardShape::Text
    };
    let item_height = if songs {
        if content.height >= 6 { 3 } else { 1 }
    } else {
        card_size(shape).1.min(content.height.max(1))
    };
    let columns = if songs {
        1
    } else {
        usize::from((content.width / 32).max(1))
    };
    let rows = usize::from(content.height / item_height.max(1));
    let visible = columns * rows;
    app.clamp_library(columns, visible);
    let collection = app.library.collection();
    if collection.items.is_empty() {
        let message = if !app.library.connected {
            "Sign in to access your saved playlists, songs, albums and artists.".into()
        } else if let Some(error) = &collection.error {
            format!("{error}\nPress r to retry.")
        } else if app.library.loading() {
            format!("Loading {}…", app.library.category.label().to_lowercase())
        } else if collection.continuation.is_some() {
            "No music items on this page. Choose More to continue.".into()
        } else if app.library.category == Category::All {
            "Your Library is empty.".into()
        } else {
            format!(
                "No {} in your Library yet.",
                app.library.category.label().to_lowercase()
            )
        };
        frame.render_widget(
            Paragraph::new(message)
                .style(Style::default().fg(Color::Gray))
                .wrap(ratatui::widgets::Wrap { trim: true }),
            content,
        );
        if collection.continuation.is_some() && !app.library.loading() {
            let width = footer.width.min(8);
            button(
                frame,
                mouse,
                Rect::new(footer.right() - width, footer.y, width, 1),
                " More ",
                false,
                true,
                MouseAction::MoreLibrary,
                accent,
            );
        }
        app.want_art(Vec::new());
        return;
    }
    let offset = collection.offset;
    let selected = collection.selected;
    let mut wanted = Vec::new();
    let use_images = app.protocol_images() && !app.overlay.is_open() && app.menu().is_none();
    let mut tiles = Tiles {
        shape,
        art: &app.art,
        wanted: &mut wanted,
        images: use_images.then_some((&mut app.images, app.graphics)),
    };
    for (slot, item) in app
        .library
        .collection()
        .items
        .iter()
        .skip(offset)
        .take(visible)
        .enumerate()
    {
        let index = offset + slot;
        let x = slot % columns;
        let y = slot / columns;
        let slot_width = content.width / columns as u16;
        let item_area = Rect::new(
            content.x + x as u16 * slot_width,
            content.y + y as u16 * item_height,
            slot_width,
            item_height,
        );
        if songs && let Some(track) = &item.track {
            browse::song_row(
                frame,
                item_area,
                track,
                index,
                selected == index,
                &mut tiles,
                app.graphics,
            );
        } else {
            render_card(frame, &item.card, item_area, selected == index, &mut tiles);
        }
        mouse.targets.push(MouseTarget::Area(
            item_area,
            MouseAction::OpenLibraryItem(index),
        ));
    }
    let collection = app.library.collection();
    let count = collection.items.len();
    let has_more = collection.continuation.is_some();
    let count_width = footer.width.saturating_sub(18);
    let label = if app.library.loading() {
        "Loading…".into()
    } else if collection.error.is_some() {
        "r retry loading".into()
    } else {
        format!(
            "{}–{} / {}{}",
            offset + 1,
            (offset + visible).min(count),
            count,
            if has_more { "+" } else { "" }
        )
    };
    frame.render_widget(
        Paragraph::new(Span::styled(
            truncate(&label, count_width as usize),
            Style::default().fg(Color::DarkGray),
        )),
        Rect::new(footer.x, footer.y, count_width, 1),
    );
    if footer.width >= 18 {
        let next_offset = (offset + visible.max(1)).min(count.saturating_sub(1));
        button(
            frame,
            mouse,
            Rect::new(footer.right() - 17, footer.y, 3, 1),
            " < ",
            false,
            offset > 0,
            MouseAction::PageLibrary {
                offset: offset.saturating_sub(visible.max(1)),
                visible: visible.max(1),
            },
            accent,
        );
        button(
            frame,
            mouse,
            Rect::new(footer.right() - 13, footer.y, 3, 1),
            " > ",
            false,
            offset + visible < count,
            MouseAction::PageLibrary {
                offset: next_offset,
                visible: visible.max(1),
            },
            accent,
        );
        if has_more {
            button(
                frame,
                mouse,
                Rect::new(footer.right() - 8, footer.y, 8, 1),
                " More ",
                false,
                !app.library.loading(),
                MouseAction::MoreLibrary,
                accent,
            );
        }
    }
    app.want_art(wanted);
}
