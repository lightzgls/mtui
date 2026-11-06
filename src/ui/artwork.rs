//! Artwork state, sizing, and terminal image placement.

use ratatui::Frame;
use ratatui::buffer::CellDiffOption;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use crate::app::{App, CoverSize, ImageBound, ImagePlan};
use crate::terminal::graphics::Graphics;
use crate::source::cover::Cover;

use super::mouse::{MouseMap, MouseTarget};

/// Reserves the pane for an image the event loop will paint, and returns where
/// and how big it should be.
///
/// Nothing is drawn here beyond the frame around it: protocol pixels arrive by
/// escape sequence, outside anything ratatui knows about. The cells underneath
/// are marked [`CellDiffOption::Skip`] so the next redraw leaves them alone --
/// without that, ratatui would repaint blanks over the picture every frame.
pub(super) fn plan_image(
    frame: &mut Frame,
    cover: &Cover,
    area: Rect,
    graphics: Graphics,
    size: CoverSize,
) -> Option<ImagePlan> {
    let (cell_w, cell_h) = graphics.cell;
    let maximum = usable(area, size);
    let (cols, rows) = fit_cells(cover, maximum, graphics.cell);
    if cols == 0 || rows == 0 {
        return None;
    }
    let (col, row) = place(frame, area, (cols, rows), size);

    for y in row..row + rows {
        for x in col..col + cols {
            frame.buffer_mut()[(x, y)].set_diff_option(CellDiffOption::Skip);
        }
    }

    Some(ImagePlan {
        col,
        row,
        width: cols * cell_w,
        height: rows * cell_h,
        bound: if cols == maximum.0 {
            ImageBound::Columns(cols)
        } else {
            ImageBound::Rows(rows)
        },
    })
}

/// Largest whole-cell box that holds the cover at its own aspect ratio, given
/// how many pixels a cell is.
///
/// Whole cells matter: an image ending halfway through a cell leaves a sliver
/// that the next text redraw slices in half. Rounding to the nearest cell
/// rather than down keeps the aspect error under half a cell -- truncating
/// could squash a 16:9 cover by a visible seven percent.
pub(super) fn fit_cells(cover: &Cover, max: (u16, u16), cell: (u16, u16)) -> (u16, u16) {
    let (max_cols, max_rows) = (u32::from(max.0), u32::from(max.1));
    let (cell_w, cell_h) = (u32::from(cell.0), u32::from(cell.1));
    if max_cols == 0 || max_rows == 0 || cover.width == 0 || cover.height == 0 {
        return (0, 0);
    }

    let div_round = |a: u32, b: u32| (a + b / 2) / b;
    let rows_for = |cols: u32| div_round(cols * cell_w * cover.height, cover.width * cell_h).max(1);
    let cols_for = |rows: u32| div_round(rows * cell_h * cover.width, cover.height * cell_w).max(1);

    let mut cols = max_cols;
    let mut rows = rows_for(cols);
    if rows > max_rows {
        rows = max_rows;
        cols = cols_for(rows).min(max_cols);
    }
    (cols as u16, rows as u16)
}

/// Divides the main area between the results and the cover.
///
/// Either side may be absent: there is nothing to show without a cover, and
/// nothing to show it in on a narrow terminal, while a full-size cover takes
/// the area outright and the list stands down.
pub(super) fn split_cover(app: &App, area: Rect) -> (Option<Rect>, Option<Rect>) {
    if app.cover.is_none() || app.cover_size == CoverSize::Side {
        return (Some(area), None);
    }
    // Browsing keeps the full content width. The current sleeve already lives
    // in the persistent player and its dedicated page; `c` remains the one
    // explicit request to replace the page with full-size artwork.
    (None, Some(area))
}

/// Cells the picture itself may occupy.
///
/// A full-size cover drops the frame: two rows of border is forty pixels of
/// picture, in a window that has none to spare. Beside the list the frame earns
/// its keep by separating the two panes.
pub(super) fn usable(area: Rect, size: CoverSize) -> (u16, u16) {
    let inner = match size {
        CoverSize::Full => area,
        CoverSize::Side => Block::bordered().inner(area),
    };
    (inner.width, inner.height)
}

/// Match the painted picture rather than the empty margins of its pane.
pub(super) fn register_cover_target(
    mouse: &mut MouseMap, cover: &Cover, area: Rect, size: CoverSize, cell: (u16, u16),
) {
    let (cols, rows) = fit_cells(cover, usable(area, size), cell);
    if cols == 0 || rows == 0 { return; }
    let inner = match size {
        CoverSize::Full => area,
        CoverSize::Side => Block::bordered().inner(Rect {
            height: (rows + 2).min(area.height), ..area
        }),
    };
    let y = if size == CoverSize::Full { inner.y + (inner.height - rows) / 2 } else { inner.y };
    mouse.targets.push(MouseTarget::Cover(Rect::new(
        inner.x + (inner.width - cols) / 2, y, cols, rows,
    )));
}

/// Draws whatever frame the mode calls for and returns the picture's top-left
/// cell: centred in the window at full size, tucked under the top border and
/// lined up with the results beside them.
pub(super) fn place(frame: &mut Frame, area: Rect, (cols, rows): (u16, u16), size: CoverSize) -> (u16, u16) {
    match size {
        CoverSize::Full => (
            area.x + (area.width - cols) / 2,
            area.y + (area.height - rows) / 2,
        ),
        CoverSize::Side => {
            let block = Block::bordered()
                .border_style(Style::default().fg(Color::DarkGray))
                .title(" cover ");
            // Only as tall as the picture, so a wide cover does not sit atop an
            // empty box.
            let framed = Rect {
                height: (rows + 2).min(area.height),
                ..area
            };
            let inner = block.inner(framed);
            frame.render_widget(block, framed);
            (inner.x + (inner.width - cols) / 2, inner.y)
        }
    }
}

pub(super) fn cell_pixels() -> (u16, u16) {
    #[cfg(windows)]
    { crate::terminal::console::cell_size() }
    #[cfg(not(windows))]
    { (8, 16) }
}

pub(super) fn square_columns(rows: u16, cell: (u16, u16)) -> u16 {
    ((u32::from(rows) * u32::from(cell.1) + u32::from(cell.0.max(1)) / 2) / u32::from(cell.0.max(1)))
        .max(1).min(u32::from(u16::MAX)) as u16
}

/// Draws the cover with half-block cells: `▀` painted in the upper pixel's
/// colour over a cell background carrying the lower one. Two image rows per text
/// row, which -- since a cell is about twice as tall as it is wide -- makes the
/// result read as roughly square pixels.
///
/// Rebuilt every frame rather than cached. At this size it is a few hundred
/// spans, which is less work than the results list already does.
pub(super) fn render_cover(frame: &mut Frame, cover: &Cover, area: Rect, size: CoverSize) {
    let (max_cols, max_rows) = usable(area, size);
    let (cols, rows) = fit_cells(cover, (max_cols, max_rows), cell_pixels());
    let (px_w, px_h) = (u32::from(cols), u32::from(rows) * 2);
    if px_w == 0 || px_h == 0 {
        return;
    }

    let rows = px_h.div_ceil(2) as u16;
    let (col, row) = place(frame, area, (px_w as u16, rows), size);
    let inner = Rect::new(col, row, px_w as u16, rows);

    // No padding span: `place` already centred the picture, so each line is
    // exactly the cells the image occupies.
    let lines: Vec<Line> = (0..rows)
        .map(|row| {
            let y = u32::from(row) * 2;
            let mut spans = Vec::with_capacity(px_w as usize);
            for x in 0..px_w {
                let mut style = Style::default().fg(sample(cover, x, y, px_w, px_h));
                // An odd pixel height leaves the last lower half unpainted.
                // Left unstyled it shows the terminal background, which is
                // honest -- better than duplicating the row above it.
                if y + 1 < px_h {
                    style = style.bg(sample(cover, x, y + 1, px_w, px_h));
                }
                spans.push(Span::styled("▀", style));
            }
            Line::from(spans)
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), inner);
}

pub(super) const ASCII_RAMP: &[u8] = b" .:-=+*#%@";

/// Draws the cover as colored ASCII, using glyph density for brightness and the
/// source pixel for colour. Geometry matches the half-block renderer so changing
/// styles never moves the surrounding layout.
pub(super) fn render_ascii_cover(frame: &mut Frame, cover: &Cover, area: Rect, size: CoverSize) {
    let (max_cols, max_rows) = usable(area, size);
    let (cols, rows) = fit_cells(cover, (max_cols, max_rows), cell_pixels());
    let (px_w, px_h) = (u32::from(cols), u32::from(rows) * 2);
    if px_w == 0 || px_h == 0 {
        return;
    }

    let rows = px_h.div_ceil(2) as u16;
    let (col, row) = place(frame, area, (px_w as u16, rows), size);
    let inner = Rect::new(col, row, px_w as u16, rows);
    let lines = (0..rows)
        .map(|row| {
            let spans = (0..px_w)
                .map(|x| {
                    let (glyph, colour) = ascii_cell(cover, x, u32::from(row), px_w, px_h);
                    Span::styled(glyph.to_string(), ascii_style(colour))
                })
                .collect::<Vec<_>>();
            Line::from(spans)
        })
        .collect::<Vec<_>>();

    frame.render_widget(Paragraph::new(lines), inner);
}

pub(super) fn ascii_cell(cover: &Cover, x: u32, row: u32, px_w: u32, px_h: u32) -> (char, Color) {
    let y = row * 2;
    let upper = sample_rgb(cover, x, y, px_w, px_h);
    let (r, g, b) = if y + 1 < px_h {
        let lower = sample_rgb(cover, x, y + 1, px_w, px_h);
        (
            ((u16::from(upper.0) + u16::from(lower.0)) / 2) as u8,
            ((u16::from(upper.1) + u16::from(lower.1)) / 2) as u8,
            ((u16::from(upper.2) + u16::from(lower.2)) / 2) as u8,
        )
    } else {
        upper
    };
    let luminance = (u32::from(r) * 54 + u32::from(g) * 183 + u32::from(b) * 19) / 256;
    let index = luminance as usize * (ASCII_RAMP.len() - 1) / 255;
    (char::from(ASCII_RAMP[index]), Color::Rgb(r, g, b))
}

pub(super) fn ascii_style(colour: Color) -> Style {
    Style::default().fg(colour).bg(Color::Rgb(0, 0, 0))
}

/// Largest image size, in pixels, that fits a `cols` x `rows` box of cells
/// without distorting the aspect ratio.
#[cfg(test)]
pub(super) fn fit(cover: &Cover, cols: u16, rows: u16) -> (u32, u32) {
    let (max_w, max_h) = (u32::from(cols), u32::from(rows) * 2);
    if max_w == 0 || max_h == 0 || cover.width == 0 || cover.height == 0 {
        return (0, 0);
    }

    let height_at_full_width = (max_w * cover.height / cover.width).max(1);
    if height_at_full_width <= max_h {
        (max_w, height_at_full_width)
    } else {
        ((max_h * cover.width / cover.height).max(1), max_h)
    }
}

#[cfg(test)]
pub(super) fn fit_ascii(cover: &Cover, cols: u16, rows: u16) -> (u32, u32) {
    let (width, height) = fit(cover, cols, rows);
    let height = height - height % 2;
    if height == 0 {
        return (0, 0);
    }
    let width = (height * cover.width / cover.height).max(1).min(width);
    (width, height)
}

/// Nearest-neighbour sample of `cover` at a point on the `px_w` x `px_h` grid
/// being drawn. Nearest-neighbour is right here: the cover was already box
/// filtered down to near this size when it was fetched.
pub(super) fn sample(cover: &Cover, x: u32, y: u32, px_w: u32, px_h: u32) -> Color {
    let (r, g, b) = sample_rgb(cover, x, y, px_w, px_h);
    Color::Rgb(r, g, b)
}

pub(super) fn sample_rgb(cover: &Cover, x: u32, y: u32, px_w: u32, px_h: u32) -> (u8, u8, u8) {
    cover.pixel(x * cover.width / px_w, y * cover.height / px_h)
}
