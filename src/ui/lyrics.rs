//! Lyrics layout, highlighting, and follow position.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{NowPlaying, Panel, Tab};
use crate::player::Snapshot;

use super::player::{message, waiting_on};
use super::text::{bounded_wrap, centred_offset};

/// Columns the lyrics panel keeps to the left of every line, for the mark
/// against the one being sung. Held even when nothing is marked, so that a line
/// becoming the current one does not shift the whole panel sideways.
pub(super) const LYRIC_GUTTER: usize = 2;

/// The lyrics, wrapped to the panel and scrolled by line.
///
/// Returns the number of lines the wrap produced, and -- while the panel is
/// following the singer -- the offset it put itself at, which the caller writes
/// back to the cursor the keys move. Only a wrap done here can be counted, and
/// only a count can be turned into a line to scroll to, which is why both
/// numbers come back from the drawing rather than being worked out before it.
pub(super) fn render_lyrics(
    frame: &mut Frame,
    now: &NowPlaying,
    snap: &Snapshot,
    area: Rect,
    space_lines: bool,
) -> (usize, Option<usize>) {
    let area = Rect::new(area.x + u16::from(area.width > 8), area.y, area.width.saturating_sub(if area.width > 8 { 2 } else { 0 }), area.height);
    let width = (area.width as usize).saturating_sub(LYRIC_GUTTER + 1);
    let Panel::Ready(lyrics) = &now.lyrics else {
        message(frame, waiting_on(&now.lyrics), area);
        return (0, None);
    };

    // Which line the track is on, for a track whose lyrics came back timed.
    // `None` covers both "not timed" and "not there yet", and neither wants a
    // line marked.
    let singing = lyrics.singing(snap.position);

    // Space separate lyric lines when there is room, while keeping wrapped
    // lines together and preserving the source's own verse breaks.
    let lyrics_rows: Box<dyn Iterator<Item = (&str, Option<usize>)>> = if lyrics.timed.is_empty() {
        Box::new(lyrics.text.lines().map(|line| (line, None)))
    } else {
        Box::new(lyrics.timed.iter().enumerate().map(|(index, line)| (line.text.as_str(), Some(index))))
    };
    let mut lines: Vec<(String, Option<usize>, bool)> = Vec::new();
    for (text, timing) in lyrics_rows {
        if text.trim().is_empty() {
            lines.push((String::new(), timing, true));
        } else {
            if space_lines && lines.last().is_some_and(|(text, _, _)| !text.is_empty()) {
                lines.push((String::new(), None, false));
            }
            lines.extend(bounded_wrap(text, width).into_iter().enumerate()
                .map(|(row, text)| (text, timing, row == 0)));
        }
    }

    let viewport = area.height as usize;
    // Centred rather than scrolled to the top: what has just been sung is as
    // much of the reason to read along as what is coming, and a line pinned to
    // the top edge shows none of it.
    let following = singing.filter(|_| now.follow_lyrics).map(|at| {
        let first = lines
            .iter()
            .position(|(_, from, _)| *from == Some(at))
            .unwrap_or(0);
        centred_offset(first, viewport, lines.len())
    });
    let offset =
        following.unwrap_or_else(|| now.cursor[Tab::Lyrics.index()].min(lines.len().saturating_sub(viewport)));

    let drawn: Vec<Line> = lines
        .iter()
        .skip(offset)
        .take(viewport)
        .map(|(text, from, first)| {
            let line = lyric_line(text, false, *from, singing, *first);
            if now.lyrics_expanded { line.alignment(ratatui::layout::Alignment::Center) } else { line }
        })
        .collect();

    frame.render_widget(Paragraph::new(drawn), area);
    (lines.len(), following)
}

/// One drawn row of the lyrics panel, styled for where the track has got to.
///
/// Two styles only: the lyric being sung, and everything else -- the rest of
/// the song, sung or not yet, and the source credit with it. One thing on the
/// panel is bright and nothing else is, so there is nothing to work out.
///
/// The dim end is [`Color::DarkGray`] rather than [`Color::Gray`] because grey
/// against white is not the contrast it looks like in source: `Gray` is ANSI 7
/// and `White` is ANSI 15, and on a good many terminal themes those are the
/// same colour, which leaves the sung line distinguished by its boldness alone.
/// `DarkGray` is ANSI 8, and 8-against-15 is the widest gap the sixteen-colour
/// palette has.
///
/// The dimming is conditional on there being something to contrast *with*. A
/// track with no timings never has a line being sung, and dimming all of it
/// would leave a panel that is only harder to read than before -- so with
/// nothing sung the words stay at ordinary body grey.
///
/// A marker as well as a colour, because on a terminal that renders bold as
/// bright and nothing else, colour alone is not a reliable way to say which
/// line of twenty is the one. The two are decided separately below -- the
/// colour belongs to every row of the sung lyric, the marker only to its first.
pub(super) fn lyric_line<'a>(
    text: &str,
    credit: bool,
    from: Option<usize>,
    singing: Option<usize>,
    first: bool,
) -> Line<'a> {
    // The credit is about the panel rather than the song, and is never the line
    // being sung. `from.is_some()` is what stops a row of an *untimed* track
    // counting: there both it and `singing` are `None`, and `None == None`.
    let sung = !credit && from.is_some() && from == singing;

    let style = if sung {
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else if credit || singing.is_some() {
        Style::default().fg(Color::DarkGray)
    } else {
        // Nothing is being sung: an untimed track, or a timed one still in its
        // intro. Dimming buys contrast against a bright line, and with no
        // bright line it buys nothing -- it would just leave the whole panel
        // harder to read than it was before any of this.
        Style::default().fg(Color::Gray)
    };

    // Once per lyric rather than once per row: a lyric that wraps is still one
    // line being sung, and a bar down each of its rows reads as several.
    let marker = if sung && first { "▌" } else { " " };

    Line::from(vec![
        Span::styled(format!("{marker} "), style),
        Span::styled(text.to_string(), style),
    ])
}
