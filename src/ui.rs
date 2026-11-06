//! Terminal frame composition.
//!
//! Feature renderers draw from [`App`] and update viewport-dependent selection,
//! scrolling, artwork requests, and image placement plans. Network and audio
//! work is dispatched through the application rather than performed here.
//!
//! The results list is genuinely virtualized -- only the rows that fit on
//! screen are turned into widget items, so a full result set costs the same to
//! draw as a single screenful.

use ratatui::Frame;
use ratatui::style::{Color, Style};
use ratatui::widgets::Block;

use crate::app::{App, CoverSize, ImageSource, PlannedImage, View};
use crate::config::CoverStyle;
use crate::source::cover::Cover;

use artist::render_artist;
use artwork::{
    cell_pixels, plan_image, register_cover_target, render_ascii_cover, render_cover, split_cover,
};
use cards::section_shape;
use home::render_home;
use mouse::button_style;
use overlays::render_overlay;
use palette::ambient;
use player::render_player;
use results::{render_compact_results, render_results};
use status::render_status;
use text::MARQUEE_ACTIVE;

mod actions;
mod artist;
mod artwork;
mod browse;
mod cards;
mod comments;
mod dialog;
mod home;
mod library;
mod lyrics;
mod menu;
mod mouse;
mod overlays;
mod paging;
mod palette;
mod player;
mod player_split;
mod preferences;
mod queue;
mod related;
mod results;
mod search;
mod shell;
mod sidebar;
mod split;
pub(crate) use split::{Split, Splits};
mod status;
mod text;

pub use mouse::MouseMap;
pub use text::animated;

/// Narrower than this and the cover would squeeze the results into a column too
/// thin to read titles in, so it is dropped instead.
const MIN_WIDTH_WITH_COVER: u16 = 64;

/// Side panels keep metadata readable without overwhelming the artwork.
/// The minimum still leaves room for all four compact tab labels.
const PANEL_MIN_WIDTH: u16 = 34;

/// Room around track identity; short windows retain its compact three rows.
const INFO_HEIGHT: u16 = 4;

/// Progress, transport/details, hints, actions, and a blank bottom margin.
const STATUS_HEIGHT: u16 = 5;

const HEADER_HEIGHT: u16 = 3;

/// Compact track identity on narrow Now Playing screens, plus one spacer row.
const HERO_HEIGHT: u16 = 4;

/// Below this a card holds nothing but borders and an ellipsis, so the page
/// stands down and says so rather than drawing a column of empty boxes.
const MIN_HOME_WIDTH: u16 = 24;

pub fn render(frame: &mut Frame, app: &mut App, mouse: &mut MouseMap) {
    MARQUEE_ACTIVE.set(false);
    mouse.targets.clear();
    mouse.key_hints_hidden = !app.show_key_hints;
    app.images.clear();
    frame.render_widget(
        Block::default().style(Style::default().bg(shell::BACKGROUND).fg(Color::White)),
        frame.area(),
    );
    let [search_area, main_area, status_area] = shell::regions(frame.area());

    shell::render_header(
        frame,
        search_area,
        shell::Header {
            mode: app.mode,
            view: app.view,
            query: &app.query,
            query_cursor: app.query_cursor,
            can_go_back: app.can_go_back(),
            accent: ambient(app),
            has_player: app.now.is_some(),
            accepts_input: app.menu().is_none() && !app.overlay.is_open(),
            symbols: crate::navigation::current(),
        },
        mouse,
    );

    let (content_area, sidebar_area) = sidebar::areas(main_area,
        app.now.is_some() && app.view != View::Playing, &app.splits);
    // The player page lays itself out around the cover rather than beside it,
    // so it reserves its own rect instead of going through `split_cover`.
    let cover_area = if app.view == View::Playing {
        render_player(frame, app, main_area, mouse)
    } else if app.view == View::Library {
        library::render(frame, app, content_area, mouse);
        None
    } else {
        let (list_area, cover_area) = split_cover(app, content_area);
        if let Some(area) = list_area {
            match app.view {
                View::Home => render_home(frame, app, area, mouse),
                View::Tracks => render_results(frame, app, area, mouse),
                View::Artist => render_artist(frame, app, area, mouse),
                View::Library => library::render(frame, app, area, mouse),
                // Handled above.
                View::Playing => {}
            }
        }
        cover_area
    };
    let sidebar_cover = sidebar_area.and_then(|area| sidebar::render(frame, app, area, main_area, mouse));
    render_status(frame, app, status_area, mouse);

    // Planned last so it can reach into the finished buffer and mark the cells
    // the image covers.
    //
    // The player page always draws the cover full-size: `Side` exists to frame
    // a picture sitting next to a list, and here it is the subject of the view
    // rather than an annotation on one. The rect it is centred in has already
    // been carved out by `render_player`.
    let size = if app.view == View::Playing {
        CoverSize::Full
    } else {
        app.cover_size
    };
    render_playing_cover(frame, app, mouse, cover_area, size);
    render_playing_cover(frame, app, mouse, sidebar_cover, CoverSize::Full);

    // Over everything, including the cover pane's cells.
    render_overlay(frame, app, mouse);
    menu::render(frame, app, mouse);
    palette::apply(frame, app.cover.as_ref(), if app.cover_style == CoverStyle::ColoredAscii {
        cover_area.or(sidebar_cover)
    } else { None });
}

fn render_playing_cover(frame: &mut Frame, app: &mut App, mouse: &mut MouseMap,
    cover_area: Option<ratatui::layout::Rect>, size: CoverSize,
) {
    let image = match (cover_area, app.cover.as_ref()) {
        // Protocol image pixels are not in ratatui's buffer, so an overlay
        // drawn over them would not cover them -- the modal would appear with
        // the picture showing through. Dropping the plan is what puts the
        // existing clear-and-redraw path to work erasing it.
        _ if app.overlay.is_open() || app.menu().is_some() => None,
        (Some(area), Some(cover))
            if app.cover_style == CoverStyle::Pixel && app.protocol_images() =>
        {
            plan_image(frame, cover, area, app.graphics, size)
        }
        (Some(area), Some(cover)) if app.cover_style == CoverStyle::ColoredAscii => {
            render_ascii_cover(frame, cover, area, size);
            None
        }
        (Some(area), Some(cover)) => {
            render_cover(frame, cover, area, size);
            None
        }
        _ => None,
    };
    if let Some(plan) = image {
        app.images.push(PlannedImage {
            source: ImageSource::Playing,
            plan,
        });
    }
    if app.now.is_some() && !app.overlay.is_open() && app.menu().is_none()
        && let (Some(area), Some(cover)) = (cover_area, app.cover.as_ref())
    {
        let cell = if app.cover_style == CoverStyle::Pixel && app.protocol_images() {
            app.graphics.cell
        } else { cell_pixels() };
        register_cover_target(mouse, cover, area, size, cell);
    }

}

#[cfg(windows)]
pub fn canvas_color(cover: Option<&Cover>) -> (u8, u8, u8) {
    palette::background(cover)
}

#[cfg(test)]
pub(crate) fn preview_buffer(name: &str, buffer: &ratatui::buffer::Buffer) {
    tests::print_preview(name, buffer);
}

#[cfg(test)]
mod tests;
