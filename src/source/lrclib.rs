//! Timings from LRCLIB, for the tracks YouTube has none for.
//!
//! YouTube publishes line timings only for its own catalogue. A cover, a live
//! take, a rip, anything uploaded by somebody who is not a label -- which is a
//! great deal of what actually gets played -- comes back as a wall of text with
//! no timings in it, and the Lyrics tab has nothing to follow.
//!
//! LRCLIB (<https://lrclib.net>) is a free, unauthenticated database of exactly
//! the thing missing: lyrics in LRC format, where every line carries the moment
//! it is sung. It is asked only when YouTube has not already answered, and its
//! failure is never an error the user sees -- the tab falls back to the block of
//! text it would have shown anyway.
//!
//! Nothing here is cached. A lyrics fetch already happens once per track, and a
//! cache of song words is a cache that only grows.

use std::time::Duration;

use serde_json::Value;

use crate::source::UNKNOWN_ARTIST;
use crate::source::auth::Http;
use crate::source::watch::{MAX_TIMED_LINES, TimedLine};

/// The exact-match endpoint: artist, title and length, answering 404 unless it
/// holds that precise recording.
const GET_URL: &str = "https://lrclib.net/api/get";

/// The search endpoint, for when the exact match misses -- which it does
/// whenever our idea of the track's length disagrees with theirs by more than a
/// couple of seconds, and YouTube's durations are rounded to the second.
const SEARCH_URL: &str = "https://lrclib.net/api/search";

/// LRCLIB asks clients to say who they are, so that a misbehaving one can be
/// told apart from the rest rather than the whole service being tightened. Both
/// halves come from the manifest, so this cannot drift from the release it is.
const USER_AGENT: &str = concat!(
    "mtui/",
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("CARGO_PKG_REPOSITORY"),
    ")"
);

/// Results read from one search before giving up on it.
///
/// A search for a common title returns a great many recordings of it, and the
/// ones past the first handful are progressively less likely to be the track
/// playing. Taking the first with timings out of a few is a better bet than
/// scanning all of them for one that happens to match.
const MAX_CANDIDATES: usize = 8;

/// What LRCLIB needs to recognise a track.
///
/// Built from the player page rather than from the queue row, because it is the
/// playing track this is about, and normalised on the way in: what YouTube calls
/// the artist is a channel name, and a channel is not what LRCLIB indexes by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub artist: String,
    pub title: String,
    pub album: Option<String>,
    pub duration: Option<Duration>,
}

impl Query {
    /// A query for a track, or `None` when there is not enough to ask with.
    ///
    /// An unnamed artist is the case worth refusing outright: LRCLIB matches on
    /// artist and title together, and a title on its own matches whichever
    /// recording of it happens to be indexed first, which is a good way to show
    /// somebody the wrong song's words with confident timings against them.
    pub fn new(
        artist: &str,
        title: &str,
        album: Option<&str>,
        duration: Option<Duration>,
    ) -> Option<Self> {
        let artist = clean_artist(artist);
        let title = clean_title(title);
        if artist.is_empty() || title.is_empty() {
            return None;
        }
        Some(Self {
            artist,
            title,
            album: album.map(str::to_string).filter(|album| !album.is_empty()),
            duration,
        })
    }
}

/// The timings LRCLIB holds for a track, if it holds any.
///
/// Answers `None` for every kind of miss -- no match, no network, a shape that
/// has changed -- because there is nothing here a user could act on. This is a
/// second opinion asked for after the first one came back thin; if it has
/// nothing to add, the tab shows what it already had.
pub fn timed(http: &Http, query: &Query) -> Option<Vec<TimedLine>> {
    exact(http, query)
        .or_else(|| search(http, query))
        .filter(|lines| !lines.is_empty())
}

/// The exact-match lookup. Needs a duration, and LRCLIB allows it a couple of
/// seconds' slack either way.
fn exact(http: &Http, query: &Query) -> Option<Vec<TimedLine>> {
    let duration = query.duration?;
    let mut params = vec![
        ("artist_name", query.artist.clone()),
        ("track_name", query.title.clone()),
        ("duration", duration.as_secs().to_string()),
    ];
    if let Some(album) = &query.album {
        params.push(("album_name", album.clone()));
    }

    let json = get(http, GET_URL, &params)?;
    synced(&json)
}

/// The fallback: search by artist and title and take the first result that has
/// timings on it.
fn search(http: &Http, query: &Query) -> Option<Vec<TimedLine>> {
    let params = vec![
        ("artist_name", query.artist.clone()),
        ("track_name", query.title.clone()),
    ];

    let json = get(http, SEARCH_URL, &params)?;
    json.as_array()?
        .iter()
        .take(MAX_CANDIDATES)
        .find_map(synced)
}

/// One GET, with the query string appended and a non-2xx treated as a miss.
///
/// A 404 is the ordinary answer for a track LRCLIB has never heard of, so it is
/// not worth distinguishing from the rest: every failure here means the same
/// thing to the caller.
fn get(http: &Http, url: &str, params: &[(&str, String)]) -> Option<Value> {
    let query = form_urlencoded::Serializer::new(String::new())
        .extend_pairs(params.iter().map(|(key, value)| (*key, value.as_str())))
        .finish();

    let request = http
        .client()
        .get(format!("{url}?{query}"))
        .header(reqwest::header::USER_AGENT, USER_AGENT);

    let (status, raw) = http.send(request).ok()?;
    if !(200..300).contains(&status) {
        return None;
    }
    serde_json::from_slice(&raw).ok()
}

/// The timed lines out of one LRCLIB record.
///
/// `syncedLyrics` is null for a record that has only plain lyrics, which is a
/// record with nothing to offer here: YouTube already gave us the words, and
/// untimed words are what we are trying to improve on.
fn synced(record: &Value) -> Option<Vec<TimedLine>> {
    let lrc = record["syncedLyrics"].as_str()?;
    let lines = parse_lrc(lrc);
    (!lines.is_empty()).then_some(lines)
}

/// LRC into timed lines.
///
/// The format is one line of text per line of file, prefixed by one or more
/// `[mm:ss.xx]` stamps -- more than one when the same words are sung at several
/// points, which is how a repeated chorus is written. Metadata tags share the
/// bracket syntax (`[ar:]`, `[length:]`) and are skipped by simply not parsing
/// as a time, which needs no list of tag names to keep up to date.
fn parse_lrc(text: &str) -> Vec<TimedLine> {
    let mut lines: Vec<TimedLine> = Vec::new();

    for line in text.lines() {
        let mut rest = line.trim_start();
        let mut starts = Vec::new();

        // Peel the stamps off the front. Whatever is left is the words, blank
        // ones included: a gap between verses is worth keeping, and it is the
        // stamp rather than the text that makes a line real.
        while let Some(end) = rest.strip_prefix('[').and_then(|r| r.find(']')) {
            let Some(start) = parse_stamp(&rest[1..=end]) else {
                break;
            };
            starts.push(start);
            rest = rest[end + 2..].trim_start();
        }

        let text = rest.trim_end();
        for start in starts {
            lines.push(TimedLine {
                text: text.to_string(),
                start,
            });
        }
    }

    // A repeated chorus writes its later stamps against the first occurrence of
    // the words, so the file is not in time order and the panel walks it in
    // order. Sorted here rather than relied upon: `Lyrics::singing` takes the
    // last line to have started, which is only the right one if they ascend.
    lines.sort_by_key(|line| line.start);
    lines.truncate(MAX_TIMED_LINES);
    lines
}

/// `mm:ss`, `mm:ss.xx` or `mm:ss.xxx` into a duration.
///
/// Returns `None` for anything else, which is what makes a metadata tag skip
/// itself: `ar:Tame Impala` has a colon in it and parses as neither.
fn parse_stamp(stamp: &str) -> Option<Duration> {
    let (minutes, rest) = stamp.split_once(':')?;
    let minutes: u64 = minutes.trim().parse().ok()?;

    let (seconds, fraction) = match rest.split_once(['.', ':']) {
        Some((seconds, fraction)) => (seconds, fraction),
        None => (rest, ""),
    };
    let seconds: u64 = seconds.trim().parse().ok()?;

    // Hundredths as written by most files, thousandths by some. Read as a
    // fraction of a second rather than a fixed unit, so both are right.
    let millis = match fraction.trim() {
        "" => 0,
        digits if digits.chars().all(|c| c.is_ascii_digit()) => {
            let value: u64 = digits.parse().ok()?;
            let scale = 10u64.checked_pow(digits.len() as u32)?;
            value * 1_000 / scale
        }
        _ => return None,
    };

    Some(Duration::from_millis(
        (minutes * 60 + seconds) * 1_000 + millis,
    ))
}

/// A channel name reduced to the artist LRCLIB would index.
///
/// YouTube's auto-generated music channels are named "Tame Impala - Topic", and
/// its own metadata joins several fields with a bullet. Neither is a thing to
/// search a lyrics database for.
fn clean_artist(artist: &str) -> String {
    if artist == UNKNOWN_ARTIST {
        return String::new();
    }
    artist
        .split('•')
        .next()
        .unwrap_or("")
        .trim()
        .trim_end_matches("- Topic")
        .trim()
        .to_string()
}

/// A video title reduced to the song's name.
///
/// Uploads carry a qualifier on the end -- "(Official Video)", "[Lyrics]" --
/// which is about the upload rather than the song, and which stops an exact
/// match dead.
fn clean_title(title: &str) -> String {
    let mut title = title.trim();

    // Repeatedly, because "Song (Official Video) [HD]" is one title with two of
    // them. Only a *trailing* group is dropped: a bracket in the middle is
    // usually part of the name, as in "Chest Pain (I Love)".
    loop {
        let trimmed = title.trim_end();
        let Some(open) = trimmed
            .strip_suffix(')')
            .map(|body| (body, '('))
            .or_else(|| trimmed.strip_suffix(']').map(|body| (body, '[')))
            .and_then(|(body, open)| body.rfind(open))
        else {
            break;
        };

        let inside = &trimmed[open + 1..trimmed.len() - 1];
        if !is_qualifier(inside) {
            break;
        }
        title = trimmed[..open].trim_end();
    }

    title.trim().to_string()
}

/// Whether a bracketed group is about the upload rather than the song.
///
/// Matched on words rather than on the whole phrase, because the ways of
/// writing it are endless -- "Official Music Video", "Official HD Video",
/// "Lyric Video" -- while the words that give it away are few.
fn is_qualifier(inside: &str) -> bool {
    const WORDS: [&str; 9] = [
        "official",
        "video",
        "audio",
        "lyric",
        "lyrics",
        "visualizer",
        "visualiser",
        "hd",
        "hq",
    ];
    let inside = inside.trim();
    !inside.is_empty()
        && inside
            .split_whitespace()
            .all(|word| WORDS.contains(&word.to_ascii_lowercase().trim_matches('/')))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_stamp_at_every_precision_a_file_might_use() {
        assert_eq!(parse_stamp("00:00.00"), Some(Duration::ZERO));
        assert_eq!(parse_stamp("01:02"), Some(Duration::from_secs(62)));
        assert_eq!(parse_stamp("01:02.5"), Some(Duration::from_millis(62_500)));
        assert_eq!(parse_stamp("01:02.50"), Some(Duration::from_millis(62_500)));
        assert_eq!(
            parse_stamp("01:02.500"),
            Some(Duration::from_millis(62_500))
        );
        assert_eq!(
            parse_stamp("02:03.75"),
            Some(Duration::from_millis(123_750))
        );
        // Past an hour the minutes simply keep counting; nothing here needs a
        // third field.
        assert_eq!(parse_stamp("75:00"), Some(Duration::from_secs(4_500)));

        // Not times, and this is exactly how metadata tags skip themselves.
        assert_eq!(parse_stamp("ar:Tame Impala"), None);
        assert_eq!(parse_stamp("length:3:21"), None);
        assert_eq!(parse_stamp("nonsense"), None);
        assert_eq!(parse_stamp(""), None);
    }

    #[test]
    fn reads_a_line_and_its_time_from_an_lrc_file() {
        let lrc = "[ar:A Band]\n\
                   [ti:A Song]\n\
                   [length:03:21]\n\
                   [00:12.34]sung line 0\n\
                   [00:16.02]sung line 1\n";

        let lines = parse_lrc(lrc);
        assert_eq!(lines.len(), 2, "the tags are not lines: {lines:#?}");
        assert_eq!(lines[0].text, "sung line 0");
        assert_eq!(lines[0].start, Duration::from_millis(12_340));
        assert_eq!(lines[1].start, Duration::from_millis(16_020));
    }

    #[test]
    fn a_repeated_chorus_becomes_a_line_at_each_of_its_times() {
        // How LRC writes words sung more than once: several stamps, one copy of
        // the text. Drawn in order, so the file's order is not the panel's.
        let lrc = "[00:30.00]verse\n[00:10.00][01:20.00]chorus\n";

        let lines = parse_lrc(lrc);
        assert_eq!(lines.len(), 3);
        assert_eq!(
            lines
                .iter()
                .map(|line| (line.start.as_secs(), line.text.as_str()))
                .collect::<Vec<_>>(),
            vec![(10, "chorus"), (30, "verse"), (80, "chorus")],
            "ascending, because `singing` takes the last line to have started"
        );
    }

    #[test]
    fn a_gap_between_verses_is_kept_as_a_line_of_its_own() {
        // A stamp with no words is a real cue: it is what stops the last line
        // of a verse being marked through the instrumental after it.
        let lines = parse_lrc("[00:01.00]sung line 0\n[00:05.00]\n[00:20.00]sung line 1\n");
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1].text, "");
        assert_eq!(lines[1].start, Duration::from_secs(5));
    }

    #[test]
    fn a_file_longer_than_any_song_is_cut_off() {
        let lrc: String = (0..MAX_TIMED_LINES + 50)
            .map(|n| format!("[{:02}:{:02}.00]sung line {n}\n", n / 60, n % 60))
            .collect();
        assert_eq!(parse_lrc(&lrc).len(), MAX_TIMED_LINES);
    }

    #[test]
    fn text_with_no_stamps_in_it_is_not_lyrics() {
        assert!(parse_lrc("just some words\nand some more\n").is_empty());
        assert!(parse_lrc("").is_empty());
    }

    #[test]
    fn a_channel_name_is_reduced_to_the_artist() {
        assert_eq!(clean_artist("Tame Impala - Topic"), "Tame Impala");
        assert_eq!(clean_artist("Tame Impala"), "Tame Impala");
        assert_eq!(clean_artist("Daft Punk • 4.2M views"), "Daft Punk");
        // Nothing to search with: better no lyrics than confident timings
        // against somebody else's song.
        assert_eq!(clean_artist(UNKNOWN_ARTIST), "");
    }

    #[test]
    fn a_trailing_qualifier_is_dropped_from_the_title() {
        assert_eq!(
            clean_title("Let It Happen (Official Video)"),
            "Let It Happen"
        );
        assert_eq!(
            clean_title("Let It Happen [Official Audio]"),
            "Let It Happen"
        );
        assert_eq!(clean_title("Let It Happen (Lyrics)"), "Let It Happen");
        assert_eq!(clean_title("Song (Official Video) [HD]"), "Song");

        // A bracket that is part of the name stays: dropping it would turn a
        // title into one LRCLIB does not hold.
        assert_eq!(clean_title("Chest Pain (I Love)"), "Chest Pain (I Love)");
        assert_eq!(
            clean_title("Song (Live at Wembley)"),
            "Song (Live at Wembley)"
        );
        assert_eq!(clean_title("Let It Happen"), "Let It Happen");
    }

    #[test]
    fn a_query_needs_both_an_artist_and_a_title() {
        let query = Query::new(
            "Tame Impala - Topic",
            "Let It Happen (Official Video)",
            Some("Currents"),
            Some(Duration::from_secs(468)),
        )
        .expect("a named track should be askable");
        assert_eq!(query.artist, "Tame Impala");
        assert_eq!(query.title, "Let It Happen");
        assert_eq!(query.album.as_deref(), Some("Currents"));

        assert!(Query::new(UNKNOWN_ARTIST, "Let It Happen", None, None).is_none());
        assert!(Query::new("Tame Impala", "   ", None, None).is_none());
        // An empty album is not an album, and sending it narrows the match to
        // records that also have none.
        assert_eq!(
            Query::new("Tame Impala", "Let It Happen", Some(""), None)
                .unwrap()
                .album,
            None
        );
    }

    #[test]
    fn a_record_with_only_plain_lyrics_has_nothing_to_add() {
        // The words are already in hand from YouTube; it is the timings that
        // are missing, so a record without them is a miss.
        let plain = serde_json::json!({
            "syncedLyrics": Value::Null,
            "plainLyrics": "sung line 0\nsung line 1",
        });
        assert!(synced(&plain).is_none());

        let empty = serde_json::json!({ "syncedLyrics": "" });
        assert!(synced(&empty).is_none());

        let timed = serde_json::json!({ "syncedLyrics": "[00:01.00]sung line 0" });
        assert_eq!(synced(&timed).unwrap()[0].start, Duration::from_secs(1));
    }

    #[test]
    fn a_search_takes_the_first_result_that_has_timings() {
        let results = serde_json::json!([
            { "syncedLyrics": Value::Null, "plainLyrics": "no use" },
            { "syncedLyrics": "[00:02.00]sung line 0" },
            { "syncedLyrics": "[00:03.00]not this one" },
        ]);

        let found = results
            .as_array()
            .unwrap()
            .iter()
            .take(MAX_CANDIDATES)
            .find_map(synced)
            .expect("the second record has timings");
        assert_eq!(found[0].text, "sung line 0");
    }
}
