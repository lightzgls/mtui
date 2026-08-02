//! What the terminal can draw, established once at startup.
//!
//! Sixel is not something a program may simply assume: a terminal without it
//! prints the escape sequence as literal garbage across the screen, which is a
//! far worse failure than a coarse picture. So we ask, and fall back to the
//! half-block renderer if the answer is no or never comes.
//!
//! Two questions go out together, both from DEC's original terminal protocol:
//! `ESC [ c` (primary device attributes) answers with a list of features in
//! which `4` means sixel, and `ESC [ 16 t` answers with the pixel size of one
//! character cell. The second is what lets an image be sized to fill a whole
//! number of columns exactly.

use std::io::{self, IsTerminal, Write};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

/// How long to wait for a terminal that may never answer. Paid once, before the
/// UI is up, and only by terminals that stay silent.
const BUDGET: Duration = Duration::from_millis(250);

/// Assumed cell size when a sixel-capable terminal will not report one.
///
/// Deliberately small. Guessing under the truth makes the image smaller than
/// its pane, which merely looks conservative; guessing over it makes the image
/// spill into the columns next door and corrupt the layout.
const ASSUMED_CELL: (u16, u16) = (8, 16);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Graphics {
    /// Whether covers can be drawn as real pixels.
    pub sixel: bool,
    /// Pixel size of one character cell, as `(width, height)`.
    pub cell: (u16, u16),
}

impl Graphics {
    /// Half-blocks only. Also what every failure path returns.
    pub fn blocks() -> Self {
        Self {
            sixel: false,
            cell: ASSUMED_CELL,
        }
    }
}

/// Asks the terminal what it can do.
///
/// `MTUI_GRAPHICS=blocks` or `=sixel` overrides the answer, which is the escape
/// hatch for a terminal that lies in either direction.
pub fn detect() -> Graphics {
    match std::env::var("MTUI_GRAPHICS").as_deref() {
        Ok("blocks") => return Graphics::blocks(),
        Ok("sixel") => {
            let (_, cell) = parse(&ask().unwrap_or_default());
            return Graphics {
                sixel: true,
                cell: cell.unwrap_or(ASSUMED_CELL),
            };
        }
        _ => {}
    }

    let Some(reply) = ask() else {
        return Graphics::blocks();
    };
    let (sixel, cell) = parse(&reply);
    Graphics {
        sixel,
        cell: cell.unwrap_or(ASSUMED_CELL),
    }
}

/// Writes both queries and collects whatever comes back within [`BUDGET`].
///
/// Raw mode is required: cooked input would hold the reply until a newline that
/// is never coming. Returns `None` when stdout is not a terminal at all.
fn ask() -> Option<String> {
    if !io::stdout().is_terminal() {
        return None;
    }
    enable_raw_mode().ok()?;
    let reply = collect();
    let _ = disable_raw_mode();
    reply
}

fn collect() -> Option<String> {
    let mut out = io::stdout();
    out.write_all(b"\x1b[c\x1b[16t").ok()?;
    out.flush().ok()?;

    let deadline = Instant::now() + BUDGET;
    let mut reply = String::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || !event::poll(left).unwrap_or(false) {
            break;
        }
        // Terminal replies arrive on the input stream as if typed. Presses
        // only: Windows reports releases too, which would double every
        // character of the reply.
        if let Ok(Event::Key(key)) = event::read()
            && key.is_press()
        {
            match key.code {
                KeyCode::Esc => reply.push('\x1b'),
                KeyCode::Char(c) => reply.push(c),
                _ => {}
            }
        }
        // Both replies are in: attributes end at `c`, the cell report at `t`.
        if reply.contains('c') && reply.contains('t') {
            break;
        }
    }

    (!reply.is_empty()).then_some(reply)
}

/// Pulls sixel support and cell size out of the raw reply.
///
/// Kept separate from the I/O so the fiddly part is testable: terminals pad
/// these replies with attributes we do not care about, and the two answers can
/// arrive in either order, or not at all.
fn parse(reply: &str) -> (bool, Option<(u16, u16)>) {
    // `ESC [ ? 62;1;4;6 c` -- attribute 4 is sixel.
    let sixel = after(reply, "\x1b[?")
        .and_then(|rest| rest.split('c').next())
        .is_some_and(|attrs| attrs.split(';').any(|a| a == "4"));

    // `ESC [ 6 ; <height> ; <width> t`, height first.
    let cell = after(reply, "\x1b[6;")
        .and_then(|rest| rest.split('t').next())
        .and_then(|report| {
            let mut parts = report.split(';');
            let height: u16 = parts.next()?.parse().ok()?;
            let width: u16 = parts.next()?.parse().ok()?;
            (width > 0 && height > 0).then_some((width, height))
        });

    (sixel, cell)
}

fn after<'a>(haystack: &'a str, needle: &str) -> Option<&'a str> {
    haystack.split_once(needle).map(|(_, rest)| rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_sixel_support_and_cell_size() {
        // Shaped like what Windows Terminal answers.
        let (sixel, cell) = parse("\x1b[?61;4;6;7;14;21;22;23;24;28;32;42c\x1b[6;20;10t");
        assert!(sixel);
        assert_eq!(cell, Some((10, 20)));
    }

    #[test]
    fn reads_replies_in_either_order() {
        let (sixel, cell) = parse("\x1b[6;16;8t\x1b[?62;4;22c");
        assert!(sixel);
        assert_eq!(cell, Some((8, 16)));
    }

    #[test]
    fn absent_attribute_four_means_no_sixel() {
        let (sixel, cell) = parse("\x1b[?62;22c\x1b[6;18;9t");
        assert!(!sixel, "no attribute 4 in the reply");
        assert_eq!(cell, Some((9, 18)), "cell size is still usable");
    }

    #[test]
    fn a_number_merely_containing_four_is_not_attribute_four() {
        // `14` and `42` must not be mistaken for `4`.
        let (sixel, _) = parse("\x1b[?62;14;42;40c");
        assert!(!sixel);
    }

    #[test]
    fn missing_or_partial_replies_degrade() {
        assert_eq!(parse(""), (false, None));
        // Attributes only: sixel is known, cell size is not.
        assert_eq!(parse("\x1b[?62;4c"), (true, None));
        // Cell report only.
        assert_eq!(parse("\x1b[6;20;10t"), (false, Some((10, 20))));
        // Truncated numbers must not panic or half-parse.
        assert_eq!(parse("\x1b[6;20t"), (false, None));
        assert_eq!(parse("\x1b[6;;t"), (false, None));
    }

    #[test]
    fn a_zero_cell_size_is_rejected() {
        // A terminal reporting zeroes would divide the image maths by one of
        // them; treat it as no answer at all.
        assert_eq!(parse("\x1b[6;0;0t"), (false, None));
    }
}
