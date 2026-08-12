//! Direct resolution through YouTube's own player API.
//!
//! This is the fast path. `yt-dlp` is a compatibility layer for 1800+ sites and
//! charges for all of it on every play: ~1.3 s to boot Python and import the
//! machinery before a single byte of network, then a fresh process with no
//! warm connection, and possibly a JavaScript runtime to solve the nsig
//! challenge. Measured end to end at ~4.2 s.
//!
//! Asking YouTube's own client API the same question takes ~0.2 s, because it
//! is one HTTPS POST on a pooled connection. The resolver's client identities
//! are chosen because they are served plain `url` fields rather than
//! `signatureCipher`, so nothing has to be deciphered and no JS runtime is
//! involved.
//!
//! The catch, and the reason [`crate::source::youtube`] is still here: this is
//! an internal API. It can change without notice, and it refuses things yt-dlp
//! handles -- age-gated, region-locked, or PO-token-gated videos. So every
//! failure here falls back to yt-dlp rather than surfacing. The worst case is
//! exactly the old behaviour plus one cheap round trip.

use std::time::Duration;

use anyhow::{Context, Result, bail};

#[cfg(test)]
use super::StreamUrl;
use super::Track;

/// Kept short on purpose. This is a speculative fast path; if it is not clearly
/// winning, the fallback is right there and will do the job properly.
const TIMEOUT: Duration = Duration::from_secs(5);

/// YouTube *Music*'s search, which is a different corpus to YouTube's.
///
/// Plain `ytsearch` returns whatever the site has: reaction videos, hour-long
/// mix compilations, full concert uploads, duplicate reuploads from channels
/// that do not own the track. This endpoint returns songs, with the artist and
/// album as structured fields rather than guesswork on a title string.
const SEARCH_URL: &str = "https://music.youtube.com/youtubei/v1/search";

/// The "Songs" search filter, base64 protobuf as YouTube Music's own client
/// sends it. Without it the response also carries albums, artists, playlists
/// and podcast episodes -- none of which we can hand to a decoder.
const SONGS_FILTER: &str = "EgWKAQIIAWoKEAoQCRADEAQQBQ%3D%3D";

/// Shared with [`crate::source::home`], which talks to the same corpus through
/// the same client. Two identities drifting apart is how one of them starts
/// getting refused for reasons the other cannot reproduce.
pub(super) const MUSIC_CLIENT_NAME: &str = "WEB_REMIX";
pub(super) const MUSIC_CLIENT_VERSION: &str = "1.20241127.01.00";

pub struct InnerTube {
    client: reqwest::Client,
    /// Held rather than built per call, so the connection pool and TLS session
    /// survive between tracks. That is most of the difference between a ~0.8 s
    /// cold call and a ~0.2 s warm one.
    runtime: tokio::runtime::Runtime,
}

impl InnerTube {
    pub fn new() -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("could not start a runtime for the player API")?;
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .build()
            .context("could not build the player API client")?;
        Ok(Self { client, runtime })
    }

    /// Resolves a video id to a directly streamable audio URL.
    ///
    /// Errors are ordinary here rather than exceptional: callers are expected
    /// to fall back to yt-dlp, not to report this to the user.
    #[cfg(test)]
    pub fn resolve(&self, video_id: &str) -> Result<StreamUrl> {
        mtui_resolver::resolve_player(&self.runtime, &self.client, None, video_id)
    }

    /// Searches YouTube Music for songs.
    ///
    /// Same bargain as [`Self::resolve`]: fast and much better targeted, but an
    /// internal API, so a caller is expected to fall back to yt-dlp rather than
    /// report a failure.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Track>> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }

        let body = serde_json::json!({
            "query": query,
            "params": SONGS_FILTER,
            "context": {
                "client": {
                    "clientName": MUSIC_CLIENT_NAME,
                    "clientVersion": MUSIC_CLIENT_VERSION,
                    "hl": "en",
                }
            }
        });
        let body = serde_json::to_vec(&body).context("could not encode the search request")?;

        let raw = self.runtime.block_on(async {
            self.client
                .post(SEARCH_URL)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body)
                .send()
                .await?
                .error_for_status()?
                .bytes()
                .await
        })?;

        let json: serde_json::Value =
            serde_json::from_slice(&raw).context("search returned unexpected JSON")?;

        let tracks = parse_search(&json, limit);
        if tracks.is_empty() {
            bail!("search returned nothing usable");
        }
        Ok(tracks)
    }
}

/// Walks the search response, which is a deeply nested render tree.
///
/// Navigated as untyped JSON on purpose. Mirroring this shape as structs would
/// be some two hundred lines describing a layout YouTube reserves the right to
/// change, and every one of those lines would be a way for a rename to become a
/// hard failure. Here, anything unrecognised simply yields no tracks, which the
/// caller reads as "fall back to yt-dlp".
fn parse_search(json: &serde_json::Value, limit: usize) -> Vec<Track> {
    let sections = json
        .pointer("/contents/tabbedSearchResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents")
        .and_then(serde_json::Value::as_array);
    let Some(sections) = sections else {
        return Vec::new();
    };

    sections
        .iter()
        .filter_map(|section| {
            section
                .pointer("/musicShelfRenderer/contents")
                .and_then(serde_json::Value::as_array)
        })
        .flatten()
        .filter_map(|item| parse_track(&item["musicResponsiveListItemRenderer"]))
        .take(limit)
        .collect()
}

fn parse_track(item: &serde_json::Value) -> Option<Track> {
    // Absent on rows that are not playable individually, such as an album
    // header that shares the shelf.
    let id = item
        .pointer("/playlistItemData/videoId")?
        .as_str()?
        .to_string();

    let title = flex_column(item, 0)?;
    // Second column is "Artist • Album • Duration", already joined for display
    // by YouTube. Splitting it back apart is more reliable than trusting each
    // run to be exactly one field.
    let details = flex_column(item, 1).unwrap_or_default();
    let fields: Vec<&str> = details.split('•').map(str::trim).collect();

    let uploader = fields.first().filter(|s| !s.is_empty()).copied();
    // Counted from the end, never by fixed index: singles carry no album, and
    // an artist credit can itself contain a separator.
    let duration = fields.last().copied().and_then(parse_duration);
    let album = (fields.len() >= 3)
        .then(|| fields[fields.len() - 2])
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    Some(Track {
        id,
        title,
        uploader: uploader.unwrap_or(super::UNKNOWN_ARTIST).to_string(),
        duration,
        album,
        // A search result is not a playlist row.
        playlist_item_id: None,
    })
}

/// Joins the text runs of one display column into a single string.
///
/// Shared with [`crate::source::home`]: the flexible-column shape is the same
/// wherever YouTube Music draws a list row, whatever the row happens to mean.
pub(super) fn flex_column(item: &serde_json::Value, index: usize) -> Option<String> {
    let runs = item
        .pointer(&format!(
            "/flexColumns/{index}/musicResponsiveListItemFlexColumnRenderer/text/runs"
        ))?
        .as_array()?;

    let text: String = runs.iter().filter_map(|run| run["text"].as_str()).collect();
    (!text.is_empty()).then_some(text)
}

/// Parses `M:SS` or `H:MM:SS` into a duration. Anything else is `None`, which
/// the UI renders as `LIVE`.
pub(super) fn parse_duration(text: &str) -> Option<Duration> {
    let mut secs: u64 = 0;
    let mut fields = 0;
    for field in text.trim().split(':') {
        secs = secs
            .checked_mul(60)?
            .checked_add(field.trim().parse().ok()?)?;
        fields += 1;
    }
    (2..=3).contains(&fields).then(|| Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration("3:47"), Some(Duration::from_secs(227)));
        assert_eq!(parse_duration(" 9:05 "), Some(Duration::from_secs(545)));
        assert_eq!(parse_duration("1:02:05"), Some(Duration::from_secs(3725)));
    }

    #[test]
    fn rejects_things_that_are_not_durations() {
        // A bare number is a play count or a year, not a length.
        assert_eq!(parse_duration("2013"), None);
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("Random Access Memories"), None);
        assert_eq!(parse_duration("1:2:3:4"), None);
    }

    /// Rebuilds the shape of one search row, so the pointer paths above are
    /// covered without a network call.
    fn row(id: &str, title: &str, details: &str) -> serde_json::Value {
        let column = |text: &str| {
            serde_json::json!({
                "musicResponsiveListItemFlexColumnRenderer": {
                    "text": { "runs": [ { "text": text } ] }
                }
            })
        };
        serde_json::json!({
            "playlistItemData": { "videoId": id },
            "flexColumns": [ column(title), column(details) ]
        })
    }

    #[test]
    fn reads_a_song_row() {
        let track = parse_track(&row(
            "JhulBGMA7G4",
            "Harder, Better, Faster, Stronger",
            "Daft Punk • Discovery • 3:47",
        ))
        .expect("row should parse");

        assert_eq!(track.id, "JhulBGMA7G4");
        assert_eq!(track.title, "Harder, Better, Faster, Stronger");
        assert_eq!(track.uploader, "Daft Punk");
        assert_eq!(track.album.as_deref(), Some("Discovery"));
        assert_eq!(track.duration, Some(Duration::from_secs(227)));
    }

    #[test]
    fn reads_a_single_with_no_album() {
        // Singles drop the album, so the duration moves column. Taking both
        // from the end rather than a fixed index is what makes this work.
        let track = parse_track(&row("abc", "Da Funk", "Daft Punk • 5:35")).expect("should parse");
        assert_eq!(track.uploader, "Daft Punk");
        assert_eq!(track.album, None);
        assert_eq!(track.duration, Some(Duration::from_secs(335)));
    }

    #[test]
    fn reads_an_album_past_a_multi_part_artist_credit() {
        // A collaboration puts a separator inside the artist field, so a
        // fixed index would mistake half the credit for the album.
        let track = parse_track(&row(
            "xyz",
            "Get Lucky",
            "Daft Punk • Pharrell Williams • Random Access Memories • 6:10",
        ))
        .expect("should parse");
        assert_eq!(track.album.as_deref(), Some("Random Access Memories"));
        assert_eq!(track.duration, Some(Duration::from_secs(370)));
    }

    #[test]
    fn labels_carry_the_artist_but_never_a_placeholder() {
        let track = parse_track(&row("a", "Veridis Quo", "Daft Punk • Discovery • 5:46")).unwrap();
        assert_eq!(track.label(), "Veridis Quo — Daft Punk");

        // Second column missing entirely: better a bare title than "— unknown".
        let mut bare = row("b", "Mystery Track", "");
        bare["flexColumns"][1] = serde_json::Value::Null;
        assert_eq!(parse_track(&bare).unwrap().label(), "Mystery Track");
    }

    #[test]
    fn skips_rows_that_are_not_playable() {
        // No videoId: an album or artist header sharing the shelf.
        let mut item = row("x", "Discovery", "Album • Daft Punk");
        item["playlistItemData"] = serde_json::Value::Null;
        assert!(parse_track(&item).is_none());
    }

    #[test]
    fn unrecognised_response_yields_nothing() {
        // Which is how the caller knows to fall back to yt-dlp.
        assert!(parse_search(&serde_json::json!({}), 20).is_empty());
        assert!(parse_search(&serde_json::json!({ "contents": 5 }), 20).is_empty());
    }

    /// Confirms the fast path is both working and actually fast against the
    /// live API, and that a warm connection is cheaper than a cold one.
    ///
    /// Ignored by default: needs the network, and it is checking a third party
    /// we do not control. A failure here is a signal to look at whether the
    /// client identity above still satisfies YouTube -- not a broken build,
    /// since every failure falls back to yt-dlp.
    #[test]
    #[ignore = "hits the live YouTube player API"]
    fn resolves_against_the_live_api() {
        use std::time::Instant;

        let tube = InnerTube::new().expect("client should build");

        // Both kinds matter. The second is a YouTube Music "art track", which
        // is what our own search returns and which most player clients refuse
        // outright -- so a regression here would quietly send every play from
        // the search results back to the 4 s yt-dlp path.
        for (id, kind) in [
            ("dQw4w9WgXcQ", "ordinary video"),
            ("JhulBGMA7G4", "art track"),
        ] {
            let start = Instant::now();
            match tube.resolve(id) {
                Ok(stream) => {
                    println!("{kind:<15} {id}: {:.2}s", start.elapsed().as_secs_f64());
                    assert!(stream.url.starts_with("https://"), "expected an https URL");
                    assert!(stream.is_valid(), "a freshly resolved URL should be valid");
                }
                Err(e) => panic!("{kind} {id} failed: {e:#}"),
            }
        }
    }

    /// Checks the live search returns songs rather than whatever YouTube has.
    ///
    /// Ignored for the same reason as the resolver's live test: it depends on a
    /// third party, and a failure means the fallback took over, not that the
    /// build is broken.
    #[test]
    #[ignore = "hits the live YouTube Music API"]
    fn searches_against_the_live_api() {
        use std::time::Instant;

        let tube = InnerTube::new().expect("client should build");
        let start = Instant::now();
        let tracks = tube.search("daft punk", 10).expect("search failed");
        println!(
            "search: {:.2}s, {} tracks",
            start.elapsed().as_secs_f64(),
            tracks.len()
        );

        assert!(!tracks.is_empty(), "expected some songs");
        for track in &tracks {
            // `{:.N}` truncates by characters, not bytes, so a multi-byte
            // title cannot be sliced mid-codepoint here.
            println!(
                "  {:<36.34} {:<30.28} {:<28.26} {:>8}",
                track.title,
                track.uploader,
                track.album.as_deref().unwrap_or("-"),
                track.duration_str()
            );
            assert_eq!(track.id.len(), 11, "every row should carry a video id");
            assert!(!track.title.is_empty());
            // Songs have a length; a row without one is a live stream, which
            // the songs filter should have excluded.
            assert!(track.duration.is_some(), "{} has no duration", track.title);
        }
    }
}
