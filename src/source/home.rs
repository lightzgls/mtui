//! The landing page: YouTube Music's own home feed, as shelves of cards.
//!
//! Same corpus and the same internal API as [`crate::source::innertube`], and
//! the same bargain: this is what YouTube Music's web client asks for when it
//! opens, so it can change without notice. Nothing here is load-bearing --
//! an unrecognised response yields no shelves, the landing page says so, and
//! every other way into the program still works.
//!
//! Signed in, the feed is the user's own: "Listen again", "Quick picks", the
//! mixes built from their history. That needs the access token to travel with
//! the request, which is why this runs on the library thread rather than the
//! source thread -- the session lives there. Signed out, YouTube answers with a
//! generic feed of two shelves, which is too thin to open a program on, so
//! `explore` is folded in behind it.
//!
//! Two shapes of card come back and both are kept:
//!
//! - `musicTwoRowItemRenderer` -- the picture cards. A song, an album or a
//!   playlist, distinguished by whether the endpoint plays or browses.
//! - `musicResponsiveListItemRenderer` -- the list rows ("Quick picks",
//!   "Trending"). Always playable.

use anyhow::{Context, Result, bail};
use serde_json::Value;

use super::auth::Http;
use super::innertube::{MUSIC_CLIENT_NAME, MUSIC_CLIENT_VERSION, flex_column, parse_duration};
use super::sapisid;
use super::{Track, UNKNOWN_ARTIST};
use crate::config::Cookies;

const ORIGIN: &str = "https://music.youtube.com";
const BROWSE_URL: &str = "https://music.youtube.com/youtubei/v1/browse";

/// The watch queue. One call returns a whole radio station seeded from a single
/// song, which is what "Quick picks" is built out of -- and, in
/// [`crate::source::watch`], what the player page shows as "Up next".
pub(super) const NEXT_URL: &str = "https://music.youtube.com/youtubei/v1/next";

/// The home feed, personalised when the request carries a session.
const HOME_ID: &str = "FEmusic_home";

/// Trending songs and new music videos. Only fetched to pad a thin home feed;
/// see [`MIN_SHELVES`].
const EXPLORE_ID: &str = "FEmusic_explore";

/// Below this the feed is not worth showing on its own. A signed-out home is
/// two shelves of playlists and no songs at all, which is a landing page that
/// cannot be played from.
const MIN_SHELVES: usize = 3;

/// Bounds on what is kept. The explore response alone is over a megabyte of
/// JSON; these are what keep a landing page from holding more of it than fits
/// on any screen it will be drawn on.
const MAX_SHELVES: usize = 12;
const MAX_CARDS: usize = 24;

/// Ceiling on the rows taken from an opened playlist or album, matching the
/// library's own limit on the same thing.
const MAX_TRACKS: usize = 200;

/// Cards put on a shelf built here. A screenful is three or four; this is deep
/// enough to scroll through and shallow enough that a shelf is not a list.
const SHELF_DEPTH: usize = 16;

/// Likes below which the built shelves stop being worth showing. "Forgotten
/// favorites" taken from a library of six is the same six as "Listen again".
const MIN_LIKES_FOR_OLD: usize = 24;

/// One horizontal row of the landing page.
#[derive(Debug, Clone)]
pub struct Shelf {
    pub title: String,
    pub cards: Vec<Card>,
}

/// One card in a shelf.
#[derive(Debug, Clone)]
pub struct Card {
    pub title: String,
    /// The second line, as YouTube Music already writes it: "Song • Radiohead",
    /// "Album • TEMPOREX", "Vie Channel • 2.6M views". Kept joined rather than
    /// split into fields, because what the fields mean varies by card and a
    /// guess that is wrong looks worse than the string YouTube chose.
    pub subtitle: String,
    pub target: Target,
}

/// What pressing Enter on a card does.
#[derive(Debug, Clone)]
pub enum Target {
    /// A song or video: resolve and play it.
    Play { video_id: String },
    /// An album, playlist or artist: fetch its tracks and show them as a list.
    Open { browse_id: String },
}

impl Card {
    pub fn is_playable(&self) -> bool {
        matches!(self.target, Target::Play { .. })
    }

    /// The card as a track, when it stands for one.
    ///
    /// Thinner than a track from a listing: a card carries a display subtitle
    /// rather than fields, so the album and the length are simply not knowable
    /// from here. Both are `None` rather than guessed at -- the player page
    /// fills them in from the watch queue a moment later.
    pub fn track(&self) -> Option<Track> {
        let Target::Play { video_id } = &self.target else {
            return None;
        };
        Some(Track {
            id: video_id.clone(),
            title: self.title.clone(),
            uploader: artist(&self.subtitle).unwrap_or(UNKNOWN_ARTIST).to_string(),
            duration: None,
            album: None,
            playlist_item_id: None,
        })
    }

    /// A card for a track we already hold, for the shelves built from the
    /// user's own library rather than from a feed.
    fn from_track(track: &Track) -> Self {
        Self {
            title: track.title.clone(),
            // The same `•`-joined shape YouTube writes, so a built shelf and a
            // fetched one read identically.
            subtitle: match &track.album {
                Some(album) => format!("{} • {album}", track.uploader),
                None => track.uploader.clone(),
            },
            target: Target::Play {
                video_id: track.id.clone(),
            },
        }
    }
}

/// The landing page, in whatever shape YouTube will give us.
///
/// Three tiers, and every one of them falls through rather than failing: the
/// user's real feed if a cookie is saved and YouTube still honours it, the
/// anonymous feed otherwise, and `explore` folded in when what came back is too
/// thin to open the program on.
///
/// Only the first tier returns "Listen again", "Heard in Shorts" and the rest
/// of the shelves built from what this account has actually listened to.
/// Without a cookie those are approximated instead, from the liked songs the
/// Data API will still hand over -- see [`personal`].
pub fn fetch(http: &Http, cookies: Option<&Cookies>) -> Result<Vec<Shelf>> {
    // Swallowed on purpose. A cookie YouTube no longer honours is worth
    // reporting once the user asks for something that needs it, not on the
    // launch of a program that has a perfectly good generic feed to show.
    let mut shelves = cookies
        .and_then(|cookies| browse(http, Some(cookies), HOME_ID).ok())
        .map(|json| parse_shelves(&json))
        .unwrap_or_default();

    if shelves.is_empty() {
        shelves = parse_shelves(&browse(http, None, HOME_ID)?);
    }

    if shelves.len() < MIN_SHELVES
        && let Ok(json) = browse(http, None, EXPLORE_ID)
    {
        // Titles rather than ids: the two endpoints can return the same shelf,
        // and a landing page that lists "Trending" twice looks broken.
        let seen: Vec<String> = shelves.iter().map(|s| s.title.clone()).collect();
        shelves.extend(
            parse_shelves(&json)
                .into_iter()
                .filter(|shelf| !seen.contains(&shelf.title)),
        );
    }

    if shelves.is_empty() {
        bail!("YouTube Music returned no home feed");
    }
    shelves.truncate(MAX_SHELVES);
    Ok(shelves)
}

/// Tracks behind a card that browses rather than plays: an album, a playlist,
/// or an artist page.
///
/// The three answer with different trees -- an album nests its rows one way, a
/// playlist another, and either may arrive in a one- or two-column layout -- so
/// the rows are collected by walking for them rather than by following a path
/// that is only correct for one of the three.
pub fn tracks(http: &Http, browse_id: &str) -> Result<Vec<Track>> {
    let json = browse(http, None, browse_id)?;

    let mut rows = Vec::new();
    collect(&json, "musicResponsiveListItemRenderer", &mut rows);

    let tracks: Vec<Track> = rows.into_iter().filter_map(parse_row).take(MAX_TRACKS).collect();
    if tracks.is_empty() {
        bail!("nothing playable came back for this one");
    }
    Ok(tracks)
}

/// One `browse` call against YouTube Music's internal API.
pub(super) fn browse(http: &Http, cookies: Option<&Cookies>, browse_id: &str) -> Result<Value> {
    post(
        http,
        BROWSE_URL,
        cookies,
        serde_json::json!({ "browseId": browse_id }),
    )
    .with_context(|| format!("could not load {browse_id}"))
}

/// One InnerTube call, signed with the user's cookie when there is one.
///
/// The context travels with every request and has to match what a real client
/// would send; `extra` is whatever the particular endpoint wants on top.
pub(super) fn post(http: &Http, url: &str, cookies: Option<&Cookies>, extra: Value) -> Result<Value> {
    let mut body = serde_json::json!({
        "context": {
            "client": {
                "clientName": MUSIC_CLIENT_NAME,
                "clientVersion": MUSIC_CLIENT_VERSION,
                "hl": "en",
            }
        }
    });
    if let Some(fields) = extra.as_object() {
        for (key, value) in fields {
            body[key] = value.clone();
        }
    }
    let body = serde_json::to_vec(&body)?;

    let mut request = http
        .client()
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body);

    // All three headers or none: Google checks the signature against the origin
    // it was signed for, and a signed request with no cookie behind it is
    // refused exactly as an unsigned one with a cookie would be.
    if let Some(cookies) = cookies {
        let now = sapisid::unix_now();
        request = request
            .header(reqwest::header::COOKIE, cookies.header())
            .header(reqwest::header::ORIGIN, ORIGIN)
            .header(
                reqwest::header::AUTHORIZATION,
                sapisid::authorization(cookies.sapisid(), ORIGIN, now),
            );
    }

    let (status, raw) = http.send(request)?;
    if !(200..300).contains(&status) {
        bail!("YouTube Music refused the request: HTTP {status}");
    }
    Ok(serde_json::from_slice(&raw)?)
}

/// Shelves built from what the account has liked, for when there is no cookie
/// to ask YouTube for the real ones.
///
/// Approximations, and named as such in the comments if not on screen: YouTube
/// builds these from watch history, which no API this program can reach exposes
/// -- the Data API's history playlist has returned empty since 2016. Likes are
/// the closest thing an OAuth session can see to "music this person chose".
///
/// `likes` is expected most-recently-liked first, which is the order the Data
/// API returns them in.
pub fn personal(http: &Http, likes: &[Track]) -> Vec<Shelf> {
    let mut shelves = Vec::new();
    if likes.is_empty() {
        return shelves;
    }

    shelves.push(Shelf {
        title: "Listen again".to_string(),
        cards: likes.iter().take(SHELF_DEPTH).map(Card::from_track).collect(),
    });

    // Rotated by the hour rather than randomly: no RNG is linked into this
    // program, an hour is long enough that the page is stable across a session,
    // and it is short enough that the shelf is not the same one every day.
    let seed = &likes[(sapisid::unix_now() / 3600) as usize % likes.len()];

    // The queue YouTube would play after this song. Its first entry is the seed
    // itself, which the user already knows they like.
    if let Ok(json) = post(
        http,
        NEXT_URL,
        None,
        serde_json::json!({
            "videoId": seed.id,
            "playlistId": format!("RDAMVM{}", seed.id),
        }),
    ) {
        let cards: Vec<Card> = queue(&json)
            .into_iter()
            .filter(|card| !matches!(&card.target, Target::Play { video_id } if *video_id == seed.id))
            .take(SHELF_DEPTH)
            .collect();
        if !cards.is_empty() {
            shelves.push(Shelf {
                title: "Quick picks".to_string(),
                cards,
            });
        }

        // The "Related" tab of that same response, which costs one further
        // call and is where YouTube keeps "You might also like".
        if let Some(id) = related_id(&json) {
            shelves.extend(similar_to(http, &id, &seed.title));
        }
    }

    // Only when there are enough likes for the old ones to be different songs
    // from the recent ones.
    if likes.len() >= MIN_LIKES_FOR_OLD {
        shelves.push(Shelf {
            title: "Forgotten favorites".to_string(),
            cards: likes
                .iter()
                .rev()
                .take(SHELF_DEPTH)
                .map(Card::from_track)
                .collect(),
        });
    }

    shelves
}

/// The "Similar to X" shelf, from the related page of a seed track.
fn similar_to(http: &Http, browse_id: &str, seed: &str) -> Option<Shelf> {
    let json = browse(http, None, browse_id).ok()?;
    let shelf = parse_shelves(&json)
        .into_iter()
        .find(|shelf| !shelf.cards.is_empty())?;
    Some(Shelf {
        // Named for the seed rather than kept as YouTube's own "You might also
        // like": on a page of shelves, what a recommendation was made from is
        // the part that says why it is there.
        title: format!("Similar to {seed}"),
        cards: shelf.cards,
    })
}

/// The browse id of the "Related" tab of a watch response.
fn related_id(json: &Value) -> Option<String> {
    let mut tabs = Vec::new();
    collect(json, "tabRenderer", &mut tabs);
    tabs.iter()
        .find(|tab| tab["title"].as_str() == Some("Related"))?
        .pointer("/endpoint/browseEndpoint/browseId")?
        .as_str()
        .map(str::to_string)
}

/// The tracks of a watch queue, as cards.
fn queue(json: &Value) -> Vec<Card> {
    let mut rows = Vec::new();
    collect(json, "playlistPanelVideoRenderer", &mut rows);

    rows.into_iter()
        .filter_map(|row| {
            Some(Card {
                title: row.pointer("/title/runs").and_then(runs_text)?,
                // "Artist • Album • Year", already joined for display.
                subtitle: row
                    .pointer("/longBylineText/runs")
                    .and_then(runs_text)
                    .unwrap_or_default(),
                target: Target::Play {
                    video_id: row["videoId"].as_str()?.to_string(),
                },
            })
        })
        .collect()
}

/// Pulls every named shelf out of a browse response.
///
/// Walked for rather than pointed at, for the same reason the search parser is
/// untyped: the path to a shelf differs between the one- and two-column layouts
/// YouTube serves, and both carry the same renderer at the end of it.
pub(super) fn parse_shelves(json: &Value) -> Vec<Shelf> {
    let mut carousels = Vec::new();
    collect(json, "musicCarouselShelfRenderer", &mut carousels);

    carousels
        .into_iter()
        .filter_map(|shelf| {
            // An unnamed shelf is one we cannot label, and an unlabelled row of
            // cards on the landing page says nothing about what it is.
            let title = shelf
                .pointer("/header/musicCarouselShelfBasicHeaderRenderer/title/runs")
                .and_then(runs_text)?;

            let cards: Vec<Card> = shelf
                .pointer("/contents")?
                .as_array()?
                .iter()
                .filter_map(parse_card)
                .take(MAX_CARDS)
                .collect();

            // Shelves of things that are neither playable nor browsable --
            // "Moods & genres" is a row of filter buttons -- come back empty.
            (!cards.is_empty()).then_some(Shelf { title, cards })
        })
        .collect()
}

fn parse_card(item: &Value) -> Option<Card> {
    let two_row = &item["musicTwoRowItemRenderer"];
    if two_row.is_object() {
        return Some(Card {
            title: two_row.pointer("/title/runs").and_then(runs_text)?,
            subtitle: two_row
                .pointer("/subtitle/runs")
                .and_then(runs_text)
                .unwrap_or_default(),
            target: target(&two_row["navigationEndpoint"])?,
        });
    }

    let row = &item["musicResponsiveListItemRenderer"];
    if row.is_object() {
        return Some(Card {
            title: flex_column(row, 0)?,
            subtitle: flex_column(row, 1).unwrap_or_default(),
            target: Target::Play {
                video_id: video_id(row)?,
            },
        });
    }

    None
}

/// What a card's endpoint does, if it does either.
fn target(endpoint: &Value) -> Option<Target> {
    if let Some(id) = endpoint.pointer("/watchEndpoint/videoId").and_then(Value::as_str) {
        return Some(Target::Play {
            video_id: id.to_string(),
        });
    }
    let browse_id = endpoint
        .pointer("/browseEndpoint/browseId")
        .and_then(Value::as_str)?;
    Some(Target::Open {
        browse_id: browse_id.to_string(),
    })
}

/// The video id of a list row, from wherever this particular shelf put it.
///
/// Three places, and which one is used varies by shelf rather than by anything
/// about the row: a playlist listing carries it in `playlistItemData`, the
/// trending shelf only in the play button drawn over its thumbnail.
fn video_id(row: &Value) -> Option<String> {
    const PATHS: [&str; 3] = [
        "/playlistItemData/videoId",
        "/overlay/musicItemThumbnailOverlayRenderer/content/musicPlayButtonRenderer\
         /playNavigationEndpoint/watchEndpoint/videoId",
        "/navigationEndpoint/watchEndpoint/videoId",
    ];
    PATHS
        .iter()
        .find_map(|path| row.pointer(path).and_then(Value::as_str))
        .map(str::to_string)
}

/// Turns one list row into a track.
///
/// Not [`crate::source::innertube`]'s search row parser, because the columns do
/// not mean the same things: a playlist row puts the artist in its own column
/// and the duration in a fixed column at the end, where a search row joins
/// artist, album and duration into one string.
fn parse_row(row: &Value) -> Option<Track> {
    let id = video_id(row)?;
    let title = flex_column(row, 0)?;

    // "Artist • 2.6M views" on a video row, a bare artist on a song row.
    let details = flex_column(row, 1).unwrap_or_default();
    let fields: Vec<&str> = details
        .split('•')
        .map(str::trim)
        .filter(|field| !field.is_empty())
        .collect();

    Some(Track {
        id,
        title,
        uploader: fields.first().copied().unwrap_or(UNKNOWN_ARTIST).to_string(),
        // The fixed column is where a playlist listing puts the length. Falling
        // back to the last joined field covers the rows that carry no fixed
        // column at all; a field that is a view count simply fails to parse,
        // which is the same as not being there.
        duration: fixed_column(row, 0)
            .as_deref()
            .and_then(parse_duration)
            .or_else(|| fields.last().copied().and_then(parse_duration)),
        album: flex_column(row, 2).filter(|album| !album.is_empty()),
        // These rows are not the user's to remove; only a Data API listing
        // knows an id that could be.
        playlist_item_id: None,
    })
}

/// The duration column of a list row, which is fixed rather than flexible
/// because it is the one column YouTube never lets the layout drop.
fn fixed_column(row: &Value, index: usize) -> Option<String> {
    row.pointer(&format!(
        "/fixedColumns/{index}/musicResponsiveListItemFixedColumnRenderer/text/runs"
    ))
    .and_then(runs_text)
}

/// Joins an array of text runs into the string YouTube would have displayed.
pub(super) fn runs_text(runs: &Value) -> Option<String> {
    let text: String = runs
        .as_array()?
        .iter()
        .filter_map(|run| run["text"].as_str())
        .collect();
    (!text.is_empty()).then_some(text)
}

/// Type markers YouTube prefixes a subtitle with. Not artists, so not part of
/// what is playing.
const TYPES: [&str; 10] = [
    "Song", "Video", "Album", "Single", "EP", "Playlist", "Chart", "Artist", "Podcast", "Episode",
];

/// The performer named in a card's subtitle, if one is.
///
/// A subtitle is a `•`-joined mix of a type marker, an artist and a count, in
/// an order that varies. Dropping what is recognisably not an artist leaves the
/// artist, and leaves nothing at all for a subtitle that never named one --
/// which is the honest answer for a playlist described by its view count.
fn artist(subtitle: &str) -> Option<&str> {
    subtitle
        .split('•')
        .map(str::trim)
        .find(|field| {
            !field.is_empty()
                && !TYPES.contains(field)
                && !field.ends_with(" views")
                && !field.ends_with(" plays")
                && !field.ends_with(" songs")
        })
}

/// Collects every value stored under `key`, at any depth.
pub(super) fn collect<'a>(json: &'a Value, key: &str, out: &mut Vec<&'a Value>) {
    match json {
        Value::Object(map) => {
            for (found, value) in map {
                if found == key {
                    out.push(value);
                } else {
                    collect(value, key, out);
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|item| collect(item, key, out)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// A picture card, as the home feed sends them.
    fn card(title: &str, subtitle: &str, endpoint: Value) -> Value {
        serde_json::json!({
            "musicTwoRowItemRenderer": {
                "title": { "runs": [ { "text": title } ] },
                "subtitle": { "runs": [ { "text": subtitle } ] },
                "navigationEndpoint": endpoint,
            }
        })
    }

    fn shelf(title: &str, cards: Vec<Value>) -> Value {
        serde_json::json!({
            "musicCarouselShelfRenderer": {
                "header": {
                    "musicCarouselShelfBasicHeaderRenderer": {
                        "title": { "runs": [ { "text": title } ] }
                    }
                },
                "contents": cards,
            }
        })
    }

    #[test]
    fn reads_a_shelf_of_song_cards() {
        let json = serde_json::json!({
            "contents": { "whatever": [ shelf("Listen again", vec![
                card("Creep", "Song • Radiohead", serde_json::json!({
                    "watchEndpoint": { "videoId": "XFkzRNyygfk" }
                })),
            ]) ] }
        });

        let shelves = parse_shelves(&json);
        assert_eq!(shelves.len(), 1);
        assert_eq!(shelves[0].title, "Listen again");

        let card = &shelves[0].cards[0];
        assert_eq!(card.title, "Creep");
        assert_eq!(card.subtitle, "Song • Radiohead");
        assert!(card.is_playable());
        // The subtitle names the type before the artist, and only the artist
        // is part of what plays.
        assert_eq!(card.track().unwrap().label(), "Creep — Radiohead");
    }

    #[test]
    fn an_album_card_browses_rather_than_plays() {
        let json = shelf(
            "Albums for you",
            vec![card(
                "Currents",
                "Album • Tame Impala",
                serde_json::json!({ "browseEndpoint": { "browseId": "MPREb_abc" } }),
            )],
        );

        let card = &parse_shelves(&json)[0].cards[0];
        assert!(!card.is_playable());
        match &card.target {
            Target::Open { browse_id } => assert_eq!(browse_id, "MPREb_abc"),
            Target::Play { .. } => panic!("an album is not a video"),
        }
    }

    /// The layout is walked for rather than pointed at, because YouTube serves
    /// the same shelves under two different browse-result renderers.
    #[test]
    fn shelves_are_found_however_deeply_they_are_nested() {
        let inner = shelf(
            "Quick picks",
            vec![card(
                "Is It True",
                "Song • Tame Impala",
                serde_json::json!({ "watchEndpoint": { "videoId": "aaaaaaaaaaa" } }),
            )],
        );
        let one_column = serde_json::json!({
            "contents": { "singleColumnBrowseResultsRenderer": { "tabs": [ { "tabRenderer": {
                "content": { "sectionListRenderer": { "contents": [ inner.clone() ] } }
            } } ] } }
        });
        let two_column = serde_json::json!({
            "contents": { "twoColumnBrowseResultsRenderer": { "secondaryContents": {
                "sectionListRenderer": { "contents": [ inner ] }
            } } }
        });

        for json in [one_column, two_column] {
            let shelves = parse_shelves(&json);
            assert_eq!(shelves.len(), 1, "shelf not found");
            assert_eq!(shelves[0].title, "Quick picks");
        }
    }

    #[test]
    fn a_shelf_with_nothing_playable_is_dropped() {
        // "Moods & genres" is a row of filter buttons, not of cards.
        let json = shelf(
            "Moods & genres",
            vec![serde_json::json!({
                "musicNavigationButtonRenderer": { "buttonText": { "runs": [ { "text": "Chill" } ] } }
            })],
        );
        assert!(parse_shelves(&json).is_empty());
    }

    #[test]
    fn an_unnamed_shelf_is_dropped() {
        // Nothing on the landing page can say what it is, so it is noise.
        let mut json = shelf("x", vec![]);
        json["musicCarouselShelfRenderer"]["header"] = Value::Null;
        assert!(parse_shelves(&json).is_empty());
    }

    #[test]
    fn an_unrecognised_response_yields_no_shelves() {
        // Which is what makes the landing page say so rather than fail.
        assert!(parse_shelves(&serde_json::json!({})).is_empty());
        assert!(parse_shelves(&serde_json::json!({ "contents": 5 })).is_empty());
    }

    /// One list row, as a playlist listing sends them: artist in its own
    /// column, duration in the fixed one.
    fn row(id: &str, columns: &[&str], duration: Option<&str>) -> Value {
        let text = |value: &str| {
            serde_json::json!({
                "musicResponsiveListItemFlexColumnRenderer": {
                    "text": { "runs": [ { "text": value } ] }
                }
            })
        };
        let mut row = serde_json::json!({
            "playlistItemData": { "videoId": id },
            "flexColumns": columns.iter().map(|c| text(c)).collect::<Vec<_>>(),
        });
        if let Some(duration) = duration {
            row["fixedColumns"] = serde_json::json!([ {
                "musicResponsiveListItemFixedColumnRenderer": {
                    "text": { "runs": [ { "text": duration } ] }
                }
            } ]);
        }
        row
    }

    #[test]
    fn reads_a_playlist_row() {
        let track = parse_row(&row(
            "JhulBGMA7G4",
            &["Harder, Better, Faster, Stronger", "Daft Punk", "Discovery"],
            Some("3:47"),
        ))
        .expect("row should parse");

        assert_eq!(track.id, "JhulBGMA7G4");
        assert_eq!(track.uploader, "Daft Punk");
        assert_eq!(track.album.as_deref(), Some("Discovery"));
        assert_eq!(track.duration, Some(Duration::from_secs(227)));
        // These rows are not the user's to remove.
        assert!(track.playlist_item_id.is_none());
    }

    #[test]
    fn a_view_count_is_not_mistaken_for_a_duration() {
        // The trending shelf carries "Channel • 2.6M views" and no fixed
        // column. Neither field is a length, so the row is honestly LIVE-less
        // rather than three million seconds long.
        let track = parse_row(&row("abcdefghijk", &["Some video", "Vie Channel • 2.6M views"], None))
            .expect("row should parse");
        assert_eq!(track.duration, None);
        assert_eq!(track.uploader, "Vie Channel");
    }

    /// The trending shelf hides the video id under the play button drawn over
    /// its thumbnail, and nowhere else.
    #[test]
    fn finds_a_video_id_in_the_thumbnail_overlay() {
        let mut row = row("x", &["Title", "Artist"], None);
        row["playlistItemData"] = Value::Null;
        row["overlay"] = serde_json::json!({
            "musicItemThumbnailOverlayRenderer": { "content": { "musicPlayButtonRenderer": {
                "playNavigationEndpoint": { "watchEndpoint": { "videoId": "l_uzEREOKfo" } }
            } } }
        });
        assert_eq!(video_id(&row).as_deref(), Some("l_uzEREOKfo"));
    }

    #[test]
    fn a_row_with_no_video_is_skipped_rather_than_listed() {
        // An album header sharing the shelf: nothing to play on Enter.
        let mut row = row("x", &["Discovery", "Daft Punk"], None);
        row["playlistItemData"] = Value::Null;
        assert!(parse_row(&row).is_none());
    }

    #[test]
    fn a_label_drops_the_type_marker_and_the_counts() {
        assert_eq!(artist("Song • Radiohead"), Some("Radiohead"));
        assert_eq!(artist("Album • TEMPOREX"), Some("TEMPOREX"));
        assert_eq!(artist("Tame Impala • 68M plays • The Slow Rush"), Some("Tame Impala"));
        // A playlist described only by how many people played it names nobody,
        // and "Creep — 37K views" would be worse than a bare title.
        assert_eq!(artist("37K views"), None);
        assert_eq!(artist(""), None);
    }

    /// The shelves that only a personalised feed carries.
    const PERSONAL: [&str; 5] = [
        "Listen again",
        "Quick picks",
        "Forgotten favorites",
        "Heard in Shorts",
        "Similar to",
    ];

    /// Whether the saved cookie still buys the real feed.
    ///
    /// Skipped rather than failed with no cookie saved: it is optional, and a
    /// checkout without one is not a broken checkout.
    ///
    /// `cargo test cookie_home -- --ignored --nocapture`
    #[test]
    #[ignore = "hits the live YouTube Music API with the saved cookie"]
    fn cookie_home_against_the_live_api() {
        let Some(cookies) = Cookies::load().expect("cookies.txt should parse") else {
            println!("no cookies.txt saved -- nothing to check");
            return;
        };

        let http = Http::new().expect("client should build");
        let json = browse(&http, Some(&cookies), HOME_ID).expect("the browse should answer");
        let shelves = parse_shelves(&json);

        println!("{} shelves with the cookie attached:", shelves.len());
        for shelf in &shelves {
            println!(
                "  {:<44} {:>2} cards, {} playable",
                shelf.title,
                shelf.cards.len(),
                shelf.cards.iter().filter(|c| c.is_playable()).count()
            );
        }

        let found: Vec<&str> = PERSONAL
            .iter()
            .copied()
            .filter(|name| shelves.iter().any(|s| s.title.starts_with(name)))
            .collect();
        println!("\npersonal shelves: {found:?}");

        // A cookie that authenticated returns shelves built from this account's
        // listening. Getting the generic feed back means YouTube ignored it,
        // which is what an expired cookie looks like -- and is the one outcome
        // worth failing on, because everything else about the run looks fine.
        assert!(
            !found.is_empty(),
            "the cookie was accepted but the feed is not personalised -- it has probably expired"
        );
    }

    /// Records what OAuth does here, so nobody spends an afternoon rediscovering
    /// it. The tokens work perfectly against the Data API and are refused by
    /// InnerTube, which is not a distinction any documentation draws.
    ///
    /// `cargo test oauth_is_refused -- --ignored --nocapture`
    #[test]
    #[ignore = "hits the live YouTube Music API with the saved sign-in"]
    fn oauth_is_refused_by_innertube() {
        use super::super::auth::Session;

        let http = Http::new().expect("client should build");
        let Some(mut session) = Session::load().ok().flatten() else {
            println!("not signed in -- nothing to check");
            return;
        };
        let token = session
            .access_token(&http)
            .expect("the saved sign-in should refresh")
            .to_string();

        let request = http
            .client()
            .post(BROWSE_URL)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
            .body(
                serde_json::to_vec(&serde_json::json!({
                    "browseId": HOME_ID,
                    "context": { "client": {
                        "clientName": MUSIC_CLIENT_NAME,
                        "clientVersion": MUSIC_CLIENT_VERSION,
                        "hl": "en",
                    } }
                }))
                .unwrap(),
            );

        let (status, _) = http.send(request).expect("the request should complete");
        println!("Bearer-authenticated InnerTube browse: HTTP {status}");
        assert_eq!(
            status, 400,
            "OAuth started working against InnerTube -- if this ever passes, the \
             cookie file stops being the only way to a personalised feed"
        );
    }

    /// Hits the live API, like the resolver's and the search's own live tests.
    /// A failure here means the landing page fell back or came up empty, not
    /// that the build is broken.
    ///
    /// `cargo test --release home_feed -- --ignored --nocapture`
    #[test]
    #[ignore = "hits the live YouTube Music API"]
    fn home_feed_against_the_live_api() {
        use std::time::Instant;

        let http = Http::new().expect("client should build");
        let start = Instant::now();
        let shelves = fetch(&http, None).expect("the home feed should come back");
        println!("home: {:.2}s, {} shelves", start.elapsed().as_secs_f64(), shelves.len());

        assert!(!shelves.is_empty());
        for shelf in &shelves {
            println!("\n{} ({} cards)", shelf.title, shelf.cards.len());
            for card in shelf.cards.iter().take(4) {
                println!(
                    "  {:<44.42} {:<34.32} {}",
                    card.title,
                    card.subtitle,
                    if card.is_playable() { "play" } else { "open" }
                );
            }
            assert!(!shelf.cards.is_empty(), "{} is empty", shelf.title);
        }

        // A landing page that cannot be played from is not one.
        assert!(
            shelves.iter().any(|s| s.cards.iter().any(Card::is_playable)),
            "no shelf carried anything playable"
        );
    }
}

#[cfg(test)]
mod live_personal {
    use super::*;
    use crate::source::library::{LIKED_ID, Library};

    /// The shelves built from the signed-in account's likes, end to end.
    ///
    /// `cargo test built_shelves -- --ignored --nocapture`
    #[test]
    #[ignore = "hits the live APIs with the saved sign-in"]
    fn built_shelves_against_the_live_account() {
        use std::time::Instant;

        let mut library = Library::new().expect("library should build");
        if !library.is_signed_in() {
            println!("not signed in -- nothing to build from");
            return;
        }

        let start = Instant::now();
        let likes = library.tracks(LIKED_ID, 100).expect("liked songs should load");
        println!("{} liked songs in {:.2}s", likes.len(), start.elapsed().as_secs_f64());

        let start = Instant::now();
        let shelves = personal(library.http(), &likes);
        println!("{} shelves in {:.2}s\n", shelves.len(), start.elapsed().as_secs_f64());

        for shelf in &shelves {
            println!("{} ({} cards)", shelf.title, shelf.cards.len());
            for card in shelf.cards.iter().take(4) {
                println!("   {:<44.42} {:.34}", card.title, card.subtitle);
            }
        }

        assert!(!shelves.is_empty(), "a signed-in account should build shelves");
        for shelf in &shelves {
            assert!(!shelf.cards.is_empty(), "{} is empty", shelf.title);
        }
    }
}
