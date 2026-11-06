//! Terminal text measurement, wrapping, clipping, and marquee animation.

use std::cell::Cell;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use ratatui::layout::Rect;
use unicode_segmentation::UnicodeSegmentation;

/// Focused long titles rest at each end before moving one column per frame.
pub(super) const MARQUEE_HOLD: usize = 6;

pub(super) static MARQUEE_START: OnceLock<Instant> = OnceLock::new();

thread_local! {
    pub(super) static MARQUEE_ACTIVE: Cell<bool> = const { Cell::new(false) };
}

pub fn animated() -> bool { MARQUEE_ACTIVE.get() }

/// Breaks text onto lines of at most `width` columns, at spaces.
///
/// A word longer than the width is left to overhang rather than split. The only
/// such words passed here are URLs, and half a URL is not useful.
pub(super) fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if line.is_empty() {
            line = word.to_string();
        } else if display_width(&line) + 1 + display_width(word) <= width {
            line.push(' ');
            line.push_str(word);
        } else {
            lines.push(std::mem::take(&mut line));
            line = word.to_string();
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Wraps prose without allowing a language that omits spaces to overhang.
pub(super) fn bounded_wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }

    let mut lines = Vec::new();
    for line in wrap(text, width) {
        if display_width(&line) <= width {
            lines.push(line);
            continue;
        }

        let mut chunk = String::new();
        let mut used = 0;
        for grapheme in line.graphemes(true) {
            let size = display_width(grapheme).max(1);
            if size > width {
                if !chunk.is_empty() {
                    lines.push(std::mem::take(&mut chunk));
                    used = 0;
                }
                lines.push(truncate(grapheme, width));
            } else {
                if used + size > width {
                    lines.push(std::mem::take(&mut chunk));
                    used = 0;
                }
                chunk.push_str(grapheme);
                used += size;
            }
        }
        if !chunk.is_empty() {
            lines.push(chunk);
        }
    }
    lines
}

/// Centres a box of at most `width` x `height` inside `area`.
pub(super) fn centred(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// First visible row of a list that keeps its cursor near the middle.
///
/// Centred rather than scrolled at the edges, which is what lets the panel hold
/// no scroll offset of its own: where to start is a function of where the
/// cursor is, so nothing has to be remembered between frames.
pub(super) fn centred_offset(cursor: usize, viewport: usize, total: usize) -> usize {
    if viewport == 0 || total <= viewport {
        return 0;
    }
    cursor.saturating_sub(viewport / 2).min(total - viewport)
}

/// `H:MM:SS` / `M:SS`, matching how the results list writes a length.
pub(super) fn clock(d: Duration) -> String {
    let total = d.as_secs();
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Renders one fixed-width column: truncated to fit, padded out to `width`, and
/// always leaving a trailing space so neighbouring columns cannot run together.
pub(super) fn cell(text: &str, width: usize) -> String {
    let text = truncate(text, width.saturating_sub(1));
    let pad = width.saturating_sub(display_width(&text));
    format!("{text}{:pad$}", "", pad = pad)
}

pub(super) fn padded(text: &str, width: usize) -> String {
    let text = truncate(text, width);
    let pad = width.saturating_sub(display_width(&text));
    format!("{text}{:pad$}", "", pad = pad)
}

pub(super) fn focused_cell(text: &str, width: usize, focused: bool) -> String {
    let room = width.saturating_sub(1);
    let text = focused_text(text, room, focused);
    let pad = width.saturating_sub(display_width(&text));
    format!("{text}{:pad$}", "", pad = pad)
}

pub(super) fn focused_text(text: &str, width: usize, focused: bool) -> String {
    if focused {
        marquee(text, width)
    } else {
        truncate(text, width)
    }
}

/// A looping, Unicode-safe window over a long title.
pub(super) fn marquee(text: &str, width: usize) -> String {
    if width > 0 && display_width(text) > width { MARQUEE_ACTIVE.set(true); }
    let tick = (MARQUEE_START
        .get_or_init(Instant::now)
        .elapsed()
        .as_millis()
        / 200) as usize;
    marquee_at(text, width, tick)
}

pub(super) fn marquee_at(text: &str, width: usize, tick: usize) -> String {
    let total = display_width(text);
    if width == 0 {
        return String::new();
    }
    if total <= width {
        return text.to_string();
    }

    let travel = total - width;
    let cycle = MARQUEE_HOLD + travel + MARQUEE_HOLD + travel;
    let tick = tick % cycle;
    let offset = match tick {
        tick if tick < MARQUEE_HOLD => 0,
        tick if tick < MARQUEE_HOLD + travel => tick - MARQUEE_HOLD + 1,
        tick if tick < MARQUEE_HOLD + travel + MARQUEE_HOLD => travel,
        tick => travel.saturating_sub(tick - (MARQUEE_HOLD + travel + MARQUEE_HOLD) + 1),
    };
    column_window(text, offset, width)
}

pub(super) fn column_window(text: &str, offset: usize, width: usize) -> String {
    let mut out = String::new();
    let mut at = 0;
    let mut used = 0;
    for c in text.chars() {
        let size = char_width(c);
        if at + size <= offset {
            at += size;
            continue;
        }
        if at < offset {
            let clipped = (at + size - offset).min(width.saturating_sub(used));
            out.push_str(&" ".repeat(clipped));
            used += clipped;
            at += size;
            continue;
        }
        if used + size > width {
            break;
        }
        out.push(c);
        used += size;
        at += size;
    }
    out
}

/// Truncates to `max` *terminal columns*, adding an ellipsis when it cuts.
///
/// Columns rather than characters, because they are not the same thing: a CJK
/// glyph occupies two cells, so counting characters lets a Japanese title
/// overflow its column and shove everything after it out of alignment. This is
/// the same measure ratatui uses to lay out what it draws.
///
/// Truncation is also per character, never per byte -- slicing a `&str` by byte
/// index would panic in the middle of a codepoint.
pub(super) fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if display_width(s) <= max {
        return s.to_string();
    }

    // Leave a column for the ellipsis, which is itself one cell wide.
    let budget = max.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = char_width(c);
        if used + w > budget {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    // At most `max` columns, not exactly: a wide glyph can leave one column
    // unfilled, and padding it out is `cell`'s job, not this function's.
    out
}

pub(super) fn display_width(s: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(s)
}

/// Control characters and other zero-width oddities are treated as one column
/// so that a hostile title cannot make a row measure less than it draws.
pub(super) fn char_width(c: char) -> usize {
    unicode_width::UnicodeWidthChar::width(c)
        .unwrap_or(1)
        .max(1)
}
