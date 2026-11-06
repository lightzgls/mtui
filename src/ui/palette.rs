//! A solid, readable artwork palette applied to UI surfaces, never image pixels.
use ratatui::Frame;
use ratatui::buffer::CellDiffOption;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use crate::app::App;
use crate::source::cover::Cover;

use super::shell;
use super::mouse::contains;

fn tint(level: u8, accent: (u8, u8, u8)) -> Color {
    let channel = |value: u8| {
        // Keep blacks dark and text readable even for saturated red/blue covers.
        (u16::from(level) + u16::from(value) * 18 / 100).min(255) as u8
    };
    Color::Rgb(channel(accent.0), channel(accent.1), channel(accent.2))
}

#[cfg(windows)]
pub(super) fn background(cover: Option<&Cover>) -> (u8, u8, u8) {
    let Some(cover) = cover else {
        return (26, 26, 26);
    };
    // Native pixels beyond the last full terminal cell extend the footer.
    let Color::Rgb(r, g, b) = tint(22, cover.accent) else {
        unreachable!()
    };
    (r, g, b)
}

pub(super) fn apply(frame: &mut Frame, cover: Option<&Cover>, artwork: Option<Rect>) {
    let Some(cover) = cover else {
        return;
    };
    let bounds = frame.area();
    for y in bounds.y..bounds.bottom() {
        for x in bounds.x..bounds.right() {
            if artwork.is_some_and(|area| contains(area, x, y)) {
                continue;
            }
            let cell = &mut frame.buffer_mut()[(x, y)];
            if matches!(cell.symbol(), "▀" | "▄" | "█" | "▌" | "▐")
                || cell.diff_option == CellDiffOption::Skip
            {
                continue;
            }
            cell.bg = match cell.bg {
                Color::Reset | Color::Black => tint(10, cover.accent),
                shell::BACKGROUND => tint(10, cover.accent),
                shell::SURFACE => tint(22, cover.accent),
                Color::Rgb(r, g, b) if r == g && g == b && r < 100 => tint(r, cover.accent),
                other => other,
            };
            cell.fg = match cell.fg {
                Color::DarkGray => tint(115, cover.accent),
                Color::Gray => tint(175, cover.accent),
                Color::White => Color::Rgb(245, 245, 245),
                other => other,
            };
        }
    }
}

/// Luma below which a fill is too dark to write black on. Chosen against the
/// terminal palette rather than derived: at 140 a mid green takes black ink and
/// a deep red takes white, which is where the eye puts the boundary too.
pub(super) const INK_FLIP: u32 = 140;

/// The colour the interface takes from whatever is playing.
///
/// One colour across the whole of the chrome -- the progress bar, the open tab,
/// the row under the cursor -- pulled from the sleeve of the track that is
/// playing, so the program is the colour of the song rather than the colour it
/// was compiled in. The cards are deliberately not included: each of those
/// wears its *own* sleeve's colour, and a page of records is the one place
/// where many colours at once is the point.
///
/// Cyan before the first cover lands and whenever nothing is playing, which is
/// exactly what the interface was before any of this.
pub(super) fn ambient(app: &App) -> Color {
    app.cover.as_ref().map_or(Color::Cyan, |cover| {
        Color::Rgb(cover.accent.0, cover.accent.1, cover.accent.2)
    })
}

/// A row under the cursor, filled in the ambient colour.
///
/// The ink is chosen against the fill rather than fixed at black. The accent is
/// lifted until its brightest *channel* clears a threshold, which is not the
/// same as its luminance clearing one: a deep red sleeve yields (170, 0, 0),
/// bright by that measure and far too dark to write black on. Fixing the ink
/// would make the selected row the one row on the page nobody can read, at the
/// moment they are looking straight at it.
pub(super) fn highlight(fill: Color) -> Style {
    let ink = match fill {
        Color::Rgb(r, g, b) if luma(r, g, b) < INK_FLIP => Color::White,
        _ => Color::Black,
    };
    Style::default()
        .fg(ink)
        .bg(fill)
        .add_modifier(Modifier::BOLD)
}

/// Rec. 601 luma, in thousandths to stay in integers. The cheap standard
/// answer, and the one the terminal palettes were themselves eyeballed against.
pub(super) fn luma(r: u8, g: u8, b: u8) -> u32 {
    (299 * u32::from(r) + 587 * u32::from(g) + 114 * u32::from(b)) / 1000
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn saturated_covers_keep_the_canvas_dark_and_body_text_readable() {
        for accent in [(255, 0, 0), (0, 255, 0), (0, 0, 255), (255, 255, 255)] {
            let Color::Rgb(r, g, b) = tint(10, accent) else {
                unreachable!()
            };
            assert!(r <= 56 && g <= 56 && b <= 56);
            let Color::Rgb(r, g, b) = tint(175, accent) else {
                unreachable!()
            };
            assert!(r >= 175 && g >= 175 && b >= 175);
        }
    }
}
