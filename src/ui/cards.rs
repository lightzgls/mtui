//! Shared artwork cards and shelf geometry.

use ratatui::Frame;
use ratatui::buffer::CellDiffOption;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use unicode_segmentation::UnicodeSegmentation;

use crate::app::{CardShape, ImageBound, ImagePlan, ImageSource, PlannedImage};
use crate::terminal::graphics::art::ArtCache;
use crate::terminal::graphics::Graphics;
use crate::source::ArtistRef;
use crate::source::cover::Cover;
use crate::source::catalog::home::{Card, Shelf};

use super::{pages::home, paging};
use super::artwork::{cell_pixels, square_columns};
use super::text::{clock, display_width, focused_text, truncate};

/// Card geometry on the landing page, borders included, per shape.
///
/// The heights are where the arithmetic of a terminal shows through. A cell is
/// about twice as tall as it is wide, so a square sleeve `n` columns across is
/// `n / 2` rows tall -- and the poster card, which is the one that looks like a
/// music app, spends ten rows on its picture before a word is written. That is
/// why there are three of these rather than one: see [`CardShape`].
///
/// The widths are nominal. Cards are stretched to fill the row they are on --
/// leftover columns are worth more inside the titles than as a ragged margin --
/// so these decide how many fit across, not how wide they end up.
///
/// Width is the free dimension. Only the height reaches [`shelf_height`] and
/// so only the height decides which shape a window can hold, which means these
/// can be widened to thin out a crowded row without pushing any window down to
/// a smaller card than it draws today. The heights are the opposite: every row
/// added here is a row taken from the shelf below.
///
/// The sleeve is square and sized from the rows left under the text, so a card
/// wider than `2 * (height - 2 - CARD_TEXT_ROWS)` spends the difference on
/// margins either side of its picture rather than on a bigger picture. That is
/// the intended look up to a point and the reason these widths are generous
/// rather than extravagant -- see the centring in [`render_card`].
pub(super) const TEXT_CARD: (u16, u16) = (32, 3);

/// Also left alone. A tile's sleeve is sized from its four rows, so widening it
/// stretches the text column into whitespace rather than thinning the row --
/// measured at 120 columns, where 32 and 36 both fit three across.
pub(super) const TILE_CARD: (u16, u16) = (32, 5);

/// Left at the width its own sleeve wants. A poster is flush only while its
/// width is about `2 * rows + 2`, and widening it past that buys margin either
/// side of an unchanged picture rather than a bigger one -- so thinning out a
/// crowded row of posters is [`GALLERY_CARD`]'s job, not a wider poster's.
pub(super) const POSTER_CARD: (u16, u16) = (22, 13);

/// The roomy shape, sized so its sleeve fills the card edge to edge rather than
/// sitting in margins: fourteen rows of picture make a twenty-eight-pixel
/// sleeve across twenty-eight columns, against the poster's twenty.
pub(super) const GALLERY_CARD: (u16, u16) = (30, 17);

/// Rows of text under a card's picture: the title, what YouTube wrote under it,
/// and the badge line. The tile shape puts these beside the picture instead, but
/// spends the same three rows on them.
pub(super) const CARD_TEXT_ROWS: u16 = 3;

/// Colour of a card with no artwork -- either because none arrived, or because
/// it has not arrived yet. Grey rather than a guessed hue: a made-up colour on
/// a card is a lie about a record nobody has seen.
pub(super) const NO_ART: (u8, u8, u8) = (110, 110, 110);

/// Columns the `▌` before a shelf heading occupies, space included.
pub(super) const MARKER_WIDTH: usize = 2;

/// Heading width defended before the position counter is granted. Same
/// principle as [`super::pages::results::MIN_TITLE_WIDTH`].
pub(super) const MIN_SHELF_TITLE: usize = 8;

/// One card's nominal size, borders included.
pub(super) fn card_size(shape: CardShape) -> (u16, u16) {
    match shape {
        CardShape::Text => TEXT_CARD,
        CardShape::Tile => TILE_CARD,
        CardShape::Poster => POSTER_CARD,
        CardShape::Gallery => GALLERY_CARD,
    }
}

/// One shelf: its heading, its cards, and a blank row under them so two shelves
/// do not read as one block of boxes.
pub(super) fn shelf_height(shape: CardShape) -> u16 {
    card_size(shape).1 + 2
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ShelfLayout {
    pub(super) index: usize,
    pub(super) shape: CardShape,
    pub(super) area: Rect,
    pub(super) across: u16,
    pub(super) slot: u16,
    pub(super) rows: u16,
    pub(super) heading_height: u16,
}

impl ShelfLayout {
    pub(super) fn card_rect(self, slot: u16) -> Rect {
        let height = self.area.height.saturating_sub(self.heading_height) / self.rows.max(1);
        Rect::new(self.area.x + (slot / self.rows.max(1)) * self.slot,
            self.area.y + self.heading_height + (slot % self.rows.max(1)) * height,
            self.slot.saturating_sub(2), height)
    }
}

pub(super) fn section_shape(shelf: &Shelf, index: usize, max_shape: CardShape) -> CardShape {
    home::shape(shelf, index, max_shape)
}

pub(super) fn shelf_layouts(
    shelves: &[Shelf],
    area: Rect,
    top: usize,
    max_shape: CardShape,
) -> Vec<ShelfLayout> {
    home::layouts(shelves, area, top, max_shape)
}

/// The artwork side of drawing a shelf: what shape to draw, what pictures are
/// already in hand, and where to write down the ones that are not.
///
/// Bundled into one struct because all three travel together down four calls of
/// nesting, and because the third is the only mutable thing in a render pass
/// that is otherwise read-only -- which is worth being able to see at a glance.
pub(super) struct Tiles<'a> {
    pub(super) shape: CardShape,
    pub(super) art: &'a ArtCache,
    /// Key and optional URL of every visible picture. Cached URL-less duplicates
    /// still keep their shared key recent; only real misses need the URL.
    pub(super) wanted: &'a mut Vec<(String, Option<String>)>,
    /// Protocol images are planned only for visible artwork. Unchanged
    /// placements are retained so navigation does not re-encode every sleeve.
    pub(super) images: Option<(&'a mut Vec<PlannedImage>, Graphics)>,
}

/// Where the cursor stands in one shelf, as far as that shelf needs to know.
#[derive(Debug, Clone, Copy)]
pub(super) struct ShelfCursor {
    /// Whether the cursor is in this shelf at all. Only one shelf draws a
    /// selection, so that the page has a single cursor rather than one per row.
    pub(super) focused: bool,
    pub(super) selected: usize,
    /// Leftmost visible card.
    pub(super) offset: usize,
}

/// One shelf: a heading, then as many cards as fit beside each other.
pub(super) fn render_shelf(
    frame: &mut Frame,
    shelf: &Shelf,
    area: Rect,
    cursor: ShelfCursor,
    (across, slot): (u16, u16),
    tiles: &mut Tiles,
) {
    let controls = paging::Controls::new(area, shelf.cards.len(), cursor.offset, usize::from(across), 1);
    let heading = controls.as_ref().map_or(Rect::new(area.x, area.y, area.width, area.height.min(1)), |controls| controls.heading);
    frame.render_widget(
        Paragraph::new(heading_line(shelf, cursor, heading.width as usize)),
        heading,
    );
    if let Some(controls) = controls { controls.render(frame); }

    for column in 0..across {
        let Some(card) = shelf.cards.get(cursor.offset + column as usize) else {
            break;
        };
        let rect = shelf_card_rect(area, slot, column);
        let selected = cursor.focused && cursor.offset + column as usize == cursor.selected;
        render_card(frame, card, rect, selected, tiles);
    }
}

pub(super) fn shelf_card_rect(area: Rect, slot: u16, column: u16) -> Rect {
    Rect::new(area.x + column * slot, area.y + area.height.min(1), slot.saturating_sub(1), area.height.saturating_sub(1))
}

/// A shelf's heading: the name YouTube gave it, and -- when the cursor is in it
/// -- how far along the shelf the cursor has got.
///
/// The counter is only drawn for the focused shelf because it is the only one
/// whose position can change, and a row of counters on shelves nobody is
/// looking at is noise that competes with the headings themselves.
pub(super) fn heading_line(shelf: &Shelf, cursor: ShelfCursor, width: usize) -> Line<'static> {
    // Narrower than the marker and there is no heading to draw, only an
    // overhang. The pane refuses to draw cards well before this.
    if width < MARKER_WIDTH + 1 {
        return Line::from("");
    }

    let (marker, title_style) = if cursor.focused {
        (
            Style::default().fg(Color::Cyan),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        (
            Style::default().fg(Color::DarkGray),
            Style::default().fg(Color::Gray),
        )
    };

    let counter = if cursor.focused && shelf.cards.len() > 1 {
        format!("{} of {} ", cursor.selected + 1, shelf.cards.len())
    } else {
        String::new()
    };
    // The name outranks the position, the same way a playlist row's name
    // outranks its track count: a counter beside a two-letter stub of a
    // heading says where the cursor is in a shelf the user can no longer
    // identify.
    let counter = if width >= MARKER_WIDTH + MIN_SHELF_TITLE + counter.len() {
        counter
    } else {
        String::new()
    };

    // The marker leads, and the counter is right-aligned against the far edge
    // of the shelf.
    let room = width.saturating_sub(MARKER_WIDTH + counter.len());
    let title = truncate(&shelf.title, room);
    let pad = room.saturating_sub(display_width(&title));

    Line::from(vec![
        Span::styled(if cursor.focused { "━ " } else { "  " }, marker),
        Span::styled(title, title_style),
        Span::styled(
            format!("{:pad$}{counter}", "", pad = pad),
            Style::default().fg(Color::DarkGray),
        ),
    ])
}

/// One card: its sleeve, its title, and whatever YouTube wrote under it.
///
/// The three shapes differ only in where the picture goes, so they share
/// everything after the split: the same border, the same accent taken from the
/// same sleeve, the same lines of text. [`CardShape::Text`] is the shape with
/// nowhere to put a picture, which is also what a card falls back to when the
/// window is too small to give it one.
pub(super) fn render_card(frame: &mut Frame, card: &Card, area: Rect, selected: bool, tiles: &mut Tiles) {
    let _ = draw_card(frame, card, area, selected, tiles);
}

/// Only complete, literal artist names become links; other metadata stays plain.
pub(super) fn artist_prefix_width(text: &str, artist: &ArtistRef, room: u16) -> Option<u16> {
    let name = artist.name.trim();
    if name.is_empty() { return None; }
    let rest = text.strip_prefix(name)?;
    if rest.chars().next().is_some_and(char::is_alphanumeric)
        || (!rest.is_empty() && !text.grapheme_indices(true).any(|(at, _)| at == name.len()))
    { return None; }
    if !truncate(text, usize::from(room)).starts_with(name) { return None; }
    let width = display_width(name) as u16;
    (width > 0).then_some(width)
}

pub(super) fn underline_artist(frame: &mut Frame, area: Rect) {
    for x in area.x..area.right() {
        frame.buffer_mut()[(x, area.y)].set_style(Style::default().add_modifier(Modifier::UNDERLINED));
    }
}

pub(super) fn render_card_artist_link(frame: &mut Frame, card: &Card, area: Rect, selected: bool, tiles: &mut Tiles, artist: Option<&ArtistRef>) -> Option<Rect> {
    let text = draw_card(frame, card, area, selected, tiles)?;
    let artist = artist?;
    if text.height < 2 { return None; }
    let room = text.width.saturating_sub(1);
    let start = if tiles.shape == CardShape::Text {
        let badge_width = badge_spans(card, Color::Reset).iter().map(|span| display_width(&span.content)).sum::<usize>() as u16;
        if room <= badge_width + 3 { return None; }
        1 + badge_width + 3
    } else { 1 };
    let detail = card.detail();
    let lead = detail.split('•').next()?.trim();
    let width = artist_prefix_width(lead, artist, text.width.saturating_sub(start))?;
    let link = Rect::new(text.x + start, text.y + 1, width, 1);
    underline_artist(frame, link);
    Some(link)
}

pub(super) fn draw_card(frame: &mut Frame, card: &Card, area: Rect, selected: bool, tiles: &mut Tiles) -> Option<Rect> {
    let shape = tiles.shape;
    let art_key = card.art_key().to_string();
    let picture_cols = |rows: u16| {
        tiles
            .images
            .as_ref()
            .map_or_else(|| square_columns(rows, cell_pixels()), |(_, graphics)| {
                let (cell_w, cell_h) = (u32::from(graphics.cell.0), u32::from(graphics.cell.1));
                ((u32::from(rows) * cell_h + cell_w / 2) / cell_w)
                    .max(1)
                    .min(u32::from(u16::MAX)) as u16
            })
    };
    let art = tiles.art.get(&art_key);
    if art.is_some() || card.art.is_some() {
        tiles.wanted.push((art_key.clone(), card.art.clone()));
    }
    // The card takes its colour from its own sleeve. This is the whole reason
    // the accent is computed at all: a page bordered in one colour says nothing
    // about what is on it, and a page that borrows each record's own is a page
    // of records. A card still waiting on its picture gets the neutral grey,
    // and changes colour under the user when it lands -- which is a page
    // filling in, and reads as one.
    let accent = art.map_or(NO_ART, |cover| cover.accent);
    let accent = Color::Rgb(accent.0, accent.1, accent.2);

    // Cards stay borderless like the shelves in YouTube Music and Spotify.
    // Selection uses one artwork-derived rail and a quiet solid surface, so it
    // is obvious without turning every item into a separate framed panel.
    if selected {
        frame.render_widget(
            Block::default().style(Style::default().bg(Color::from_u32(0x001d_1d1d))),
            area,
        );
    }
    let rail = (0..area.height)
        .map(|_| {
            Line::from(Span::styled(
                if selected { "▌" } else { " " },
                Style::default().fg(if selected { accent } else { Color::DarkGray }),
            ))
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(rail),
        Rect::new(area.x, area.y, area.width.min(1), area.height),
    );
    let inner = Rect::new(
        area.x.saturating_add(1),
        area.y,
        area.width.saturating_sub(1),
        area.height,
    );
    if inner.width == 0 || inner.height == 0 {
        return None;
    }

    // Where the picture goes and what is left for the words. Both shapes with a
    // picture keep it square by deriving its width from its height: two pixels
    // to a row and one to a column, so a tile `n` rows tall is `2n` wide.
    let text = match shape {
        CardShape::Text => inner,
        CardShape::Tile => {
            let rows = inner.height;
            let cols = picture_cols(rows).min(inner.width);
            render_tile(
                frame,
                art,
                &art_key,
                Rect {
                    width: cols,
                    ..inner
                },
                &mut tiles.images,
            );
            Rect {
                x: inner.x + cols + 1,
                width: inner.width.saturating_sub(cols + 1),
                ..inner
            }
        }
        // One arm for both: a gallery card is a poster with more of everything,
        // and the arithmetic that centres a square sleeve over three rows of
        // text is the same either way.
        CardShape::Poster | CardShape::Gallery => {
            let gap = u16::from(shape == CardShape::Gallery);
            let rows = inner.height.saturating_sub(CARD_TEXT_ROWS + gap);
            let cols = picture_cols(rows).min(inner.width);
            render_tile(
                frame,
                art,
                &art_key,
                Rect {
                    // Centred: a card stretched wider than its nominal width
                    // must not leave its sleeve pinned to the left edge with a
                    // hole beside it.
                    x: inner.x + (inner.width - cols) / 2,
                    width: cols,
                    height: rows,
                    ..inner
                },
                &mut tiles.images,
            );
            Rect {
                y: inner.y + rows + gap,
                height: CARD_TEXT_ROWS,
                ..inner
            }
        }
    };

    if text.width == 0 || text.height == 0 {
        return None;
    }
    frame.render_widget(
        Paragraph::new(card_lines(
            card,
            text.width as usize,
            selected,
            accent,
            shape,
        )),
        text,
    );
    Some(text.intersection(area))
}

/// The words on a card: its title, then what it is, then who made it.
///
/// A text card gets two of the three lines and has to fold the badge into the
/// second, which is the cost of being the shape that fits anywhere.
pub(super) fn card_lines(
    card: &Card,
    width: usize,
    selected: bool,
    accent: Color,
    shape: CardShape,
) -> Vec<Line<'static>> {
    let title = if selected {
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    // One column of padding on the left so text never touches the border.
    let room = width.saturating_sub(1);

    let mut lines = vec![Line::from(vec![
        Span::raw(" "),
        Span::styled(focused_text(&card.title, room, selected), title),
    ])];

    let detail = card.detail();
    let badge = badge_spans(card, accent);
    let badge_width: usize = badge.iter().map(|span| display_width(&span.content)).sum();

    if shape == CardShape::Text {
        // Two rows and three things to say. The badge leads, because what a
        // card *is* -- a song to play or an album to open -- is the thing a
        // grid of near-identical boxes hides, and the artist is already the
        // start of the line beside it.
        let mut spans = vec![Span::raw(" ")];
        spans.extend(badge);
        if !detail.is_empty() && room > badge_width + 3 {
            spans.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
            spans.extend(detail_spans(&detail, room.saturating_sub(badge_width + 3)));
        }
        lines.push(Line::from(spans));
        return lines;
    }

    let mut spans = vec![Span::raw(" ")];
    spans.extend(detail_spans(&detail, room));
    lines.push(Line::from(spans));

    let mut spans = vec![Span::raw(" ")];
    spans.extend(badge);
    lines.push(Line::from(spans));
    lines
}

/// The line under a title, styled by what each part of it is.
///
/// YouTube writes this as one string with bullets in it -- "Tame Impala • 2015"
/// -- and drawing the whole thing in one grey is what makes a shelf of cards
/// read as a paragraph. The parts are not equal: the first field is who made
/// the record, which is the one thing on this line anybody scans for, and what
/// follows is a year or a play count that only matters once they have found it.
///
/// So the artist is drawn a step brighter than the rest and the bullets a step
/// dimmer than either, which turns one grey line into a line with a subject.
/// The fields are not re-ordered or dropped: this styles what YouTube wrote
/// rather than second-guessing it, for the reason [`Card::subtitle`] gives.
pub(super) fn detail_spans(detail: &str, room: usize) -> Vec<Span<'static>> {
    let lead = Style::default().fg(Color::Gray);
    let rest = Style::default().fg(Color::DarkGray);
    // Dimmer than the words either side, so the bullets separate the fields
    // without competing with them.
    let bullet = Style::default().fg(Color::from_u32(0x0044_4444));

    let mut spans = Vec::new();
    let mut left = room;
    for (at, field) in detail.split('•').map(str::trim).enumerate() {
        if left == 0 {
            break;
        }
        if at > 0 {
            // Spent from the same budget as the words, so a line that has to be
            // cut is cut at a field rather than mid-separator.
            let sep = " • ";
            if left <= display_width(sep) {
                break;
            }
            spans.push(Span::styled(sep, bullet));
            left -= display_width(sep);
        }
        let text = truncate(field, left);
        left -= display_width(&text);
        spans.push(Span::styled(text, if at == 0 { lead } else { rest }));
    }
    spans
}

/// The badge under a card: what it is, and how long it runs when that is known.
///
/// Kept as quiet metadata rather than a filled chip. Artwork and the active
/// rail already supply colour; repeating a colored component on every card
/// makes the shelf harder to scan.
pub(super) fn badge_spans(card: &Card, accent: Color) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    // The clock is the actionable metadata and is never sacrificed to the tag
    // on a narrow tile.
    if let Some(duration) = card.duration {
        spans.push(Span::styled(
            format!("{} ", clock(duration)),
            Style::default().fg(Color::Gray),
        ));
    }
    if let Some(kind) = card.kind() {
        spans.push(Span::styled(
            kind.to_lowercase(),
            Style::default().fg(accent),
        ));
    }
    // If metadata is absent, say what Enter does. Symbols remain optional.
    if spans.is_empty() {
        spans.push(Span::styled(
            if card.is_playable() {
                crate::terminal::navigation::current().label(crate::terminal::navigation::Icon::Playing, "play")
            } else {
                crate::terminal::navigation::current().label(crate::terminal::navigation::Icon::Menu, "open")
            },
            Style::default().fg(Color::DarkGray),
        ));
    }
    spans
}

/// Draws a sleeve into exactly `area`, filling it corner to corner.
///
/// Cropped to fill rather than fitted inside, which is the opposite of what the
/// cover pane does and is right for the opposite reason. A pane is showing one
/// picture and must not distort or crop it; a grid is showing twelve, and
/// twelve tiles of different shapes with different amounts of dead space around
/// them is not a grid. So a 16:9 video thumbnail loses its sides here and every
/// tile is the same square.
///
/// Both Kitty and SIXEL can paint these tiles. The universal fallback is made
/// of half-blocks; protocol pixels are retained until their placement changes.
pub(super) fn render_tile(
    frame: &mut Frame,
    art: Option<&Cover>,
    key: &str,
    area: Rect,
    images: &mut Option<(&mut Vec<PlannedImage>, Graphics)>,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    if art.is_some()
        && let Some((images, graphics)) = images.as_mut()
        && plan_tile_image(
            frame,
            ImageSource::Card(key.to_string()),
            area,
            images,
            *graphics,
        )
    {
        return;
    }
    render_tile_blocks(frame, art, area);
}

pub(super) fn render_tile_blocks(frame: &mut Frame, art: Option<&Cover>, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let Some(cover) = art else {
        // A picture on its way, or one that never arrived. Drawn as a muted
        // hatch rather than left blank so the card keeps its shape either way
        // and nothing on the page moves when the sleeve lands.
        let rows: Vec<Line> = (0..area.height)
            .map(|_| {
                Line::from(Span::styled(
                    "░".repeat(area.width as usize),
                    Style::default().fg(Color::from_u32(0x0020_2020)),
                ))
            })
            .collect();
        frame.render_widget(Paragraph::new(rows), area);
        return;
    };

    let (px_w, px_h) = (u32::from(area.width), u32::from(area.height) * 2);
    let cell = cell_pixels();
    let crop = Crop::fill(cover, u32::from(area.width) * u32::from(cell.0),
        u32::from(area.height) * u32::from(cell.1));

    let lines: Vec<Line> = (0..area.height)
        .map(|row| {
            let y = u32::from(row) * 2;
            let spans = (0..px_w)
                .map(|x| {
                    let mut style = Style::default().fg(crop.at(cover, x, y, px_w, px_h));
                    if y + 1 < px_h {
                        style = style.bg(crop.at(cover, x, y + 1, px_w, px_h));
                    }
                    Span::styled("▀", style)
                })
                .collect::<Vec<_>>();
            Line::from(spans)
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), area);
}

pub(super) fn plan_tile_image(
    frame: &mut Frame,
    source: ImageSource,
    area: Rect,
    images: &mut Vec<PlannedImage>,
    graphics: Graphics,
) -> bool {
    if area.width == 0 || area.height == 0 {
        return false;
    }
    let (cell_w, cell_h) = graphics.cell;
    let (pixel_w, pixel_h) = (
        u32::from(area.width) * u32::from(cell_w),
        u32::from(area.height) * u32::from(cell_h),
    );
    let (cols, rows, bound) = if pixel_w > pixel_h {
        let cols = ((pixel_h + u32::from(cell_w) / 2) / u32::from(cell_w))
            .max(1)
            .min(u32::from(area.width)) as u16;
        (cols, area.height, ImageBound::Rows(area.height))
    } else {
        let rows = ((pixel_w + u32::from(cell_h) / 2) / u32::from(cell_h))
            .max(1)
            .min(u32::from(area.height)) as u16;
        (area.width, rows, ImageBound::Columns(area.width))
    };
    let col = area.x + (area.width - cols) / 2;
    let row = area.y + (area.height - rows) / 2;
    for y in row..row + rows {
        for x in col..col + cols {
            frame.buffer_mut()[(x, y)].set_diff_option(CellDiffOption::Skip);
        }
    }
    images.push(PlannedImage {
        source,
        plan: ImagePlan {
            col,
            row,
            width: cols * cell_w,
            height: rows * cell_h,
            bound,
        },
    });
    true
}

/// The window of a cover that fills a tile without distorting it: the largest
/// centred rect of the source with the tile's aspect ratio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Crop {
    pub(super) x: u32,
    pub(super) y: u32,
    pub(super) width: u32,
    pub(super) height: u32,
}

impl Crop {
    pub(super) fn fill(cover: &Cover, px_w: u32, px_h: u32) -> Self {
        if cover.width == 0 || cover.height == 0 || px_w == 0 || px_h == 0 {
            return Self {
                x: 0,
                y: 0,
                width: cover.width.max(1),
                height: cover.height.max(1),
            };
        }
        // Compared as a single product rather than as two ratios, so the
        // integer division that would decide a near-square case by rounding
        // never happens.
        if cover.width * px_h > cover.height * px_w {
            // Wider than the tile: keep every row, take a centred band of
            // columns. This is the 16:9 thumbnail losing its sides.
            let width = (cover.height * px_w / px_h).clamp(1, cover.width);
            Self {
                x: (cover.width - width) / 2,
                y: 0,
                width,
                height: cover.height,
            }
        } else {
            let height = (cover.width * px_h / px_w).clamp(1, cover.height);
            Self {
                x: 0,
                y: (cover.height - height) / 2,
                width: cover.width,
                height,
            }
        }
    }

    /// Nearest-neighbour sample of the cropped window at a point on the
    /// `px_w` x `px_h` grid being drawn.
    pub(super) fn at(self, cover: &Cover, x: u32, y: u32, px_w: u32, px_h: u32) -> Color {
        let (r, g, b) = cover.pixel(
            self.x + x * self.width / px_w,
            self.y + y * self.height / px_h,
        );
        Color::Rgb(r, g, b)
    }
}
