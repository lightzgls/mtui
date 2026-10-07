//! A solid, readable artwork palette applied to UI surfaces, never image pixels.
use super::*;

fn tint(level: u8, accent: (u8, u8, u8)) -> Color {
    let channel = |value: u8| {
        // Keep blacks dark and text readable even for saturated red/blue covers.
        (u16::from(level) + u16::from(value) * 18 / 100).min(255) as u8
    };
    Color::Rgb(channel(accent.0), channel(accent.1), channel(accent.2))
}

pub(super) fn background(cover: Option<&Cover>) -> (u8, u8, u8) {
    let Some(cover) = cover else {
        return (26, 26, 26);
    };
    let Color::Rgb(r, g, b) = tint(10, cover.accent) else {
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
                // The persistent strip shares the canvas; it has no black slab.
                shell::BACKGROUND | shell::SURFACE => tint(10, cover.accent),
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
