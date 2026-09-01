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
//! source thread -- the session lives there. Signed out, YouTube answers with
//! the generic form of the same `FEmusic_home` feed.
//!
//! Two shapes of card come back and both are kept:
//!
//! - `musicTwoRowItemRenderer` -- the picture cards. A song, an album or a
//!   playlist, distinguished by whether the endpoint plays or browses.
//! - `musicResponsiveListItemRenderer` -- the list rows ("Quick picks",
//!   "Trending"). Always playable.

#[cfg(test)]
use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use super::cover;
use super::http::Http;
use super::innertube::{
    MUSIC_CLIENT_NAME, MUSIC_CLIENT_VERSION, flex_column, flex_runs, parse_duration,
};
#[cfg(test)]
use super::journal::Journal;
use super::sapisid;
use super::{ArtistRef, BrowseEndpoint, Track, UNKNOWN_ARTIST};
use crate::config::Cookies;

const ORIGIN: &str = "https://music.youtube.com";
const BROWSE_URL: &str = "https://music.youtube.com/youtubei/v1/browse";
#[cfg(test)]
const SEARCH_URL: &str = "https://music.youtube.com/youtubei/v1/search";

/// YouTube Music's filtered-search token for community-created playlists.
/// This is the filter used by its web client, rather than a text suffix that
/// merely hopes ordinary search will rank playlists first.
#[cfg(test)]
const COMMUNITY_PLAYLISTS_FILTER: &str = "EgeKAQQoAEABagwQDhAKEAMQBBAJEAU%3D";

/// The watch queue. One call returns a whole radio station seeded from a single
/// song, which is what "Quick picks" is built out of -- and, in
/// [`crate::source::watch`], what the player page shows as "Up next".
pub(super) const NEXT_URL: &str = "https://music.youtube.com/youtubei/v1/next";

/// The home feed, personalised when the request carries a session.
const HOME_ID: &str = "FEmusic_home";

/// Home is startup work. A dead endpoint must yield to the anonymous fallback
/// promptly.
const HOME_TIMEOUT: Duration = Duration::from_secs(5);

/// Every shelf is kept in YouTube's order. Cards remain bounded so one unusually
/// deep carousel cannot grow the session indefinitely.
const MAX_CARDS: usize = 24;

/// Ceiling on the rows taken from an opened playlist or album, matching the
/// library's own limit on the same thing.
const MAX_TRACKS: usize = 200;

/// Cards put on a shelf built here. A screenful is three or four; this is deep
/// enough to scroll through and shallow enough that a shelf is not a list.
#[cfg(test)]
const SHELF_DEPTH: usize = 16;
/// A tighter shelf leaves familiar history for Quick Picks instead of putting
/// every known track above it and leaving only strangers to recommend.
#[cfg(test)]
const LISTEN_AGAIN_DEPTH: usize = 12;

/// Likes below which the built shelves stop being worth showing. "Forgotten
/// favorites" taken from a library of six is the same six as "Listen again".
#[cfg(test)]
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
    /// Where the card's artwork lives, exactly as YouTube gave it.
    ///
    /// Held as the URL rather than the picture: a feed is twelve shelves of
    /// twenty-four, and fetching three hundred images to draw a dozen of them
    /// is the opposite of what this program is for. [`crate::art`] fetches the
    /// ones actually on screen and forgets them again.
    ///
    /// `None` for a card YouTube sent no thumbnail with, which the renderer
    /// draws as a card without a picture rather than as a gap.
    pub art: Option<String>,
    /// How long the track runs, when the card is a list row and YouTube sent
    /// the column that says so.
    ///
    /// Absent on every picture card, and that is not a gap to be filled: a
    /// two-row item can be an album or a playlist, and there is no single
    /// duration for one. The renderer draws this when it is there and drops the
    /// badge when it is not, rather than printing a guess.
    pub duration: Option<std::time::Duration>,
    /// Lead artist link when the card's metadata carried one. Kept separately
    /// from the card target because an album can browse itself while still
    /// naming an artist the page actions can open.
    pub artist_ref: Option<ArtistRef>,
    pub target: Target,
}

/// What pressing Enter on a card does.
#[derive(Debug, Clone)]
pub enum Target {
    /// A song or video: resolve and play it.
    Play { video_id: String },
    /// An album or playlist: fetch its tracks and show them as a list.
    Open { endpoint: BrowseEndpoint },
    /// A Music artist has a mixed page rather than a flat track listing.
    Artist { artist: ArtistRef },
}

impl Card {
    pub fn is_playable(&self) -> bool {
        matches!(self.target, Target::Play { .. })
    }

    /// What the card stands for, as the word YouTube prefixed its subtitle with
    /// -- "Song", "Album", "Playlist".
    ///
    /// Read off the subtitle rather than inferred from the target, because the
    /// target only says play-or-browse and every browsable card would come back
    /// the same word. Absent when YouTube wrote no marker, which is common on
    /// list rows and is why this is drawn as a badge that can be missing rather
    /// than a column that must be filled.
    pub fn kind(&self) -> Option<&str> {
        self.subtitle
            .split('•')
            .map(str::trim)
            .find(|field| TYPES.contains(field))
    }

    /// What the subtitle says once the type marker has been taken out of it, so
    /// a card drawing the marker as its own badge does not also print it in the
    /// line underneath.
    pub fn detail(&self) -> String {
        let kind = self.kind();
        self.subtitle
            .split('•')
            .map(str::trim)
            .filter(|field| {
                !field.is_empty() && Some(*field) != kind && parse_duration(field).is_none()
            })
            .collect::<Vec<_>>()
            .join(" • ")
    }

    /// Identifies the card's artwork in the art cache.
    ///
    /// The target rather than the URL: the same picture is served under URLs
    /// that differ in their size suffix, and the same song appearing on two
    /// shelves should cost one fetch rather than two.
    pub fn art_key(&self) -> &str {
        match &self.target {
            Target::Play { video_id } => video_id,
            Target::Open { endpoint } => &endpoint.browse_id,
            Target::Artist { artist } => &artist.endpoint.browse_id,
        }
    }

    /// The card as a track, when it stands for one.
    ///
    /// Thinner than a track from a listing: a card carries a display subtitle
    /// rather than fields, so the album is simply not knowable from here, and
    /// the length only when the card came from a list row that named one.
    /// Neither is guessed at -- the player page fills them in from the watch
    /// queue a moment later.
    pub fn track(&self) -> Option<Track> {
        let Target::Play { video_id } = &self.target else {
            return None;
        };
        Some(Track {
            id: video_id.clone(),
            title: self.title.clone(),
            uploader: artist(&self.subtitle).unwrap_or(UNKNOWN_ARTIST).to_string(),
            duration: self.duration,
            album: None,
            artist_ref: self.artist_ref.clone(),
        })
    }

    /// A card for a track we already hold, for the shelves built from the
    /// user's own library rather than from a feed.
    #[cfg(test)]
    fn from_track(track: &Track) -> Self {
        Self {
            title: track.title.clone(),
            // The same `•`-joined shape YouTube writes, so a built shelf and a
            // fetched one read identically.
            subtitle: match &track.album {
                Some(album) => format!("{} • {album}", track.uploader),
                None => track.uploader.clone(),
            },
            // No JSON to read a thumbnail out of -- these shelves are built from
            // the play journal, which stores what was played and not what it
            // looked like. A video's thumbnail is derivable from its id, so a
            // built card is no poorer than a fetched one.
            art: Some(cover::thumb_url(&track.id)),
            duration: track.duration,
            artist_ref: track.artist_ref.clone(),
            target: Target::Play {
                video_id: track.id.clone(),
            },
        }
    }
}

/// The landing page, in whatever shape YouTube will give us.
///
/// The personalised and anonymous feeds race each other. The first usable one
/// wins, so a stale session cannot sit in front of a perfectly good public page.
///
/// Only the first tier returns "Listen again", "Heard in Shorts" and the rest
/// of the shelves built from what this account has actually listened to.
/// Without a cookie those are approximated instead, from the liked songs the
/// Data API will still hand over -- see [`personal`].
#[cfg(test)]
pub fn fetch(http: &Http, cookies: Option<&Cookies>) -> Result<(Vec<Shelf>, bool)> {
    if let Some(cookies) = cookies
        && let Some(shelves) = fetch_personalised(http, cookies)?
    {
        return Ok((shelves, true));
    }

    Ok((fetch_public(http)?, false))
}

#[cfg(test)]
pub fn fetch_public(http: &Http) -> Result<Vec<Shelf>> {
    let shelves = parse_shelves(&browse(http, None, HOME_ID)?);
    if shelves.is_empty() {
        bail!("YouTube Music returned no home feed");
    }
    Ok(shelves)
}

pub fn fetch_personalised(http: &Http, cookies: &Cookies) -> Result<Option<Vec<Shelf>>> {
    let shelves = parse_shelves(&browse(http, Some(cookies), HOME_ID)?);
    Ok(is_personalised(&shelves).then_some(shelves))
}

pub fn is_personalised(shelves: &[Shelf]) -> bool {
    const PERSONAL: [&str; 4] = [
        "Quick picks",
        "Listen again",
        "Heard in Shorts",
        "Similar to",
    ];
    PERSONAL
        .iter()
        .any(|name| shelves.iter().any(|shelf| shelf.title.starts_with(name)))
}

/// Tracks behind a collection card that browses rather than plays.
///
/// Albums and playlists answer with different trees, and either may arrive in
/// a one- or two-column layout, so rows are found by walking. Artist pages use
/// their dedicated mixed-content parser instead.
pub fn tracks_endpoint(http: &Http, endpoint: &BrowseEndpoint) -> Result<Vec<Track>> {
    let json = browse_endpoint(http, None, endpoint)?;

    let mut rows = Vec::new();
    collect(&json, "musicResponsiveListItemRenderer", &mut rows);

    let tracks: Vec<Track> = rows
        .into_iter()
        .filter_map(parse_row)
        .take(MAX_TRACKS)
        .collect();
    if tracks.is_empty() {
        bail!("nothing playable came back for this one");
    }
    Ok(tracks)
}

/// One `browse` call against YouTube Music's internal API.
pub(super) fn browse(http: &Http, cookies: Option<&Cookies>, browse_id: &str) -> Result<Value> {
    browse_endpoint(http, cookies, &BrowseEndpoint::new(browse_id))
}

/// One browse call that preserves the endpoint's optional parameters.
pub(super) fn browse_endpoint(
    http: &Http,
    cookies: Option<&Cookies>,
    endpoint: &BrowseEndpoint,
) -> Result<Value> {
    browse_endpoint_as(http, cookies, endpoint, MUSIC_CLIENT_VERSION)
}

/// [`browse`], as a stated version of the music client.
///
/// Only [`crate::source::watch::lyrics`] asks for anything but
/// [`MUSIC_CLIENT_VERSION`], and it does so for one capability that the pinned
/// version is not served -- see the constant beside it.
pub(super) fn browse_as(
    http: &Http,
    cookies: Option<&Cookies>,
    browse_id: &str,
    client_version: &str,
) -> Result<Value> {
    browse_endpoint_as(
        http,
        cookies,
        &BrowseEndpoint::new(browse_id),
        client_version,
    )
}

fn browse_endpoint_as(
    http: &Http,
    cookies: Option<&Cookies>,
    endpoint: &BrowseEndpoint,
    client_version: &str,
) -> Result<Value> {
    let request = browse_request(http, cookies, endpoint, client_version)?;
    let (status, raw) = http.send(request)?;
    if !(200..300).contains(&status) {
        bail!("YouTube Music refused the request: HTTP {status}");
    }
    Ok(serde_json::from_slice(&raw)?)
}

fn browse_request(
    http: &Http,
    cookies: Option<&Cookies>,
    endpoint: &BrowseEndpoint,
    client_version: &str,
) -> Result<reqwest::RequestBuilder> {
    let mut extra = serde_json::json!({ "browseId": endpoint.browse_id });
    if let Some(params) = &endpoint.params {
        extra["params"] = Value::String(params.clone());
    }
    post_request_as(http, BROWSE_URL, cookies, client_version, extra)
        .with_context(|| format!("could not prepare {}", endpoint.browse_id))
}

/// One InnerTube call, signed with the user's cookie when there is one.
///
/// The context travels with every request and has to match what a real client
/// would send; `extra` is whatever the particular endpoint wants on top.
pub(super) fn post(
    http: &Http,
    url: &str,
    cookies: Option<&Cookies>,
    extra: Value,
) -> Result<Value> {
    post_as(http, url, cookies, MUSIC_CLIENT_VERSION, extra)
}

/// [`post`], as a stated version of the music client.
fn post_as(
    http: &Http,
    url: &str,
    cookies: Option<&Cookies>,
    client_version: &str,
    extra: Value,
) -> Result<Value> {
    let request = post_request_as(http, url, cookies, client_version, extra)?;
    let (status, raw) = http.send(request)?;
    if !(200..300).contains(&status) {
        bail!("YouTube Music refused the request: HTTP {status}");
    }
    Ok(serde_json::from_slice(&raw)?)
}

fn post_request_as(
    http: &Http,
    url: &str,
    cookies: Option<&Cookies>,
    client_version: &str,
    extra: Value,
) -> Result<reqwest::RequestBuilder> {
    let mut body = serde_json::json!({
        "context": {
            "client": {
                "clientName": MUSIC_CLIENT_NAME,
                "clientVersion": client_version,
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
    if extra["browseId"].as_str() == Some(HOME_ID) {
        request = request.timeout(HOME_TIMEOUT);
    }

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

    Ok(request)
}

/// Radio stations blended into "Quick picks".
///
/// The single biggest thing wrong with the shelf this replaces: one seed is one
/// station, and one station is one mood, so a page built from it read the same
/// however much the user's taste varied. Four seeds by four different artists
/// cost four round trips -- they run while a landing page is already on screen,
/// so nobody is waiting on them -- and produce a shelf that looks like a person.
#[cfg(test)]
const SEEDS: usize = 4;

/// Shelves built from this account's listening, for when there is no cookie to
/// ask YouTube for the real ones.
///
/// Ranked by [`crate::source::journal`] rather than taken in like-order. The
/// distinction matters more than it sounds: YouTube builds its own shelves from
/// watch history, which no API this program can reach exposes -- the Data API's
/// history playlist has returned empty since 2016 -- so what stands in for it is
/// the history MTUI keeps of its own plays, scored for how often, how recently,
/// and how far through. Likes fold in as a prior rather than as the whole
/// answer, which is what keeps a page from being the like list in four orders.
///
/// Until the journal has something to say, the old behaviour is exactly what
/// happens: `taste` reports itself uninformed, and every shelf falls back to
/// likes. A first run looks the way it always did and improves from there.
///
/// `likes` is expected most-recently-liked first, which is the order the Data
/// API returns them in; `rotation` steps the seeds along so a refresh gives a
/// different page.
#[cfg(test)]
pub fn personal(http: &Http, likes: &[Track], journal: &Journal, rotation: usize) -> Vec<Shelf> {
    let now = sapisid::unix_now();
    let taste = journal.taste(likes, now);
    let informed = taste.is_informed();
    let has_plays = taste.has_plays();
    // Change the initial page daily as well as on explicit refresh, without
    // persisting another counter solely to vary recommendations.
    let rotation = rotation.wrapping_add((now / 86_400) as usize);

    let mut shelves = Vec::new();
    // Nothing played and nothing liked: there is genuinely no user here to
    // build a page for, and the generic feed behind this is the right answer.
    if likes.is_empty() && !has_plays {
        return shelves;
    }

    // Every id the page may not use again. Shelves are built in order and each
    // one skips what the ones above it took, because the same song in three
    // shelves is the other half of why this page felt repetitive -- a small
    // library guarantees the overlap, and nothing was checking for it.
    //
    // Seeded with what was played in the last few hours, so that rule covers the
    // radios too: a station seeded from a song someone has been playing all
    // morning returns that morning's songs, and recommending those back is the
    // thing that makes a shelf look like it is not paying attention.
    let mut seen: HashSet<String> = taste.recent().clone();

    let listen_again: Vec<Card> = if has_plays {
        let fresh: Vec<Card> = taste
            .listen_again(LISTEN_AGAIN_DEPTH, rotation)
            .into_iter()
            .map(|ranked| Card::from_track(&ranked.track))
            .collect();
        if fresh.is_empty() {
            taste
                .played(LISTEN_AGAIN_DEPTH)
                .into_iter()
                .map(|ranked| Card::from_track(&ranked.track))
                .collect()
        } else {
            fresh
        }
    } else {
        likes
            .iter()
            .take(SHELF_DEPTH)
            .map(Card::from_track)
            .collect()
    };
    let listen_title = if has_plays {
        "Listen again"
    } else {
        "Liked songs"
    };
    push(&mut shelves, &mut seen, listen_title, listen_again);

    // The radios. Seeds come from the ranking when there is one and from the
    // likes when there is not, but the shelf is built the same way either way.
    let seeds: Vec<Track> = if has_plays {
        let ranked: Vec<Track> = taste
            .seeds(SEEDS, rotation, &seen)
            .into_iter()
            .map(|ranked| ranked.track.clone())
            .collect();
        if ranked.is_empty() {
            taste
                .played(SEEDS)
                .into_iter()
                .map(|ranked| ranked.track.clone())
                .collect()
        } else {
            ranked
        }
    } else {
        Vec::new()
    };

    let mut picks: Vec<Vec<Card>> = Vec::new();
    let mut similar: Option<Shelf> = None;
    for seed in &seeds {
        let response = post(
            http,
            NEXT_URL,
            None,
            serde_json::json!({
                "videoId": seed.id,
                "playlistId": format!("RDAMVM{}", seed.id),
            }),
        );

        let mut cards = vec![Card::from_track(seed)];
        if let Ok(json) = response {
            cards.extend(
                queue(&json)
                .into_iter()
                // The station repeats the seed. We add the richer local card
                // once ourselves so every recommendation has visible context.
                .filter(|card| !matches!(&card.target, Target::Play { video_id } if *video_id == seed.id))
                .take(3),
            );

            // "Similar to X" is built from the first seed that offers a Related
            // tab, and only from one: it names a song, and a shelf named after four
            // of them would be named after none.
            if similar.is_none()
                && let Some(id) = related_id(&json)
            {
                similar = similar_to(http, &id, &seed.title);
            }
        }
        picks.push(cards);
    }

    push(&mut shelves, &mut seen, "Quick picks", interleave(picks));

    if let Some(shelf) = similar {
        push(&mut shelves, &mut seen, &shelf.title.clone(), shelf.cards);
    }

    let forgotten: Vec<Card> = if informed {
        let from_history: Vec<Card> = taste
            .forgotten(SHELF_DEPTH, now)
            .into_iter()
            .map(|ranked| Card::from_track(&ranked.track))
            .collect();
        if from_history.is_empty() && likes.len() >= MIN_LIKES_FOR_OLD {
            likes
                .iter()
                .rev()
                .take(SHELF_DEPTH)
                .map(Card::from_track)
                .collect()
        } else if from_history.is_empty() {
            taste
                .least_recent(SHELF_DEPTH)
                .into_iter()
                .map(|ranked| Card::from_track(&ranked.track))
                .collect()
        } else {
            from_history
        }
    } else if likes.len() >= MIN_LIKES_FOR_OLD {
        // The old approximation, kept for the cold-start path only: with no
        // journal there is no way to tell what has gone cold, and the far end
        // of the like list is the least-bad guess at it.
        likes
            .iter()
            .rev()
            .take(SHELF_DEPTH)
            .map(Card::from_track)
            .collect()
    } else {
        Vec::new()
    };
    push(&mut shelves, &mut seen, "Forgotten favorites", forgotten);

    shelves
}

/// Puts the four primary Home sections first without disturbing YouTube's
/// ordering among everything else it returned.
#[cfg(test)]
pub fn order_shelves(shelves: &mut [Shelf]) {
    shelves.sort_by_key(|shelf| match shelf.title.as_str() {
        "Quick picks" | "From your listening" => 0,
        "Listen again" | "Liked songs" => 1,
        "Forgotten favorites" => 2,
        "Community playlists for you" => 3,
        _ => 4,
    });
}

/// Adds a shelf, dropping the cards already used above it.
///
/// A shelf that is left with too little to be worth a row is dropped entirely
/// rather than shown short -- two cards under a heading looks like something
/// failed, which on this page it has not.
#[cfg(test)]
fn push(shelves: &mut Vec<Shelf>, seen: &mut HashSet<String>, title: &str, cards: Vec<Card>) {
    const MIN_CARDS: usize = 3;

    // Filtered against `seen` without writing to it, because a shelf that turns
    // out to be too thin is not shown -- and marking its cards as used would
    // then withhold them from the shelf below, which might have had room.
    let mut within = HashSet::new();
    let cards: Vec<Card> = cards
        .into_iter()
        .filter(|card| match &card.target {
            Target::Play { video_id } => {
                !seen.contains(video_id) && within.insert(video_id.clone())
            }
            // Browsable collections and artists are not songs and cannot
            // collide with them.
            Target::Open { .. } | Target::Artist { .. } => true,
        })
        .take(SHELF_DEPTH)
        .collect();

    if cards.len() < MIN_CARDS {
        return;
    }
    for card in &cards {
        if let Target::Play { video_id } = &card.target {
            seen.insert(video_id.clone());
        }
    }
    shelves.push(Shelf {
        title: title.to_string(),
        cards,
    });
}

/// Rounds through the stations, taking one card from each in turn.
///
/// Concatenating them instead would put the whole of the first seed's radio at
/// the front, which is the same shelf as before with three more hidden off the
/// right-hand edge. Interleaving is what makes the first screenful -- the only
/// part most people see -- carry all four.
#[cfg(test)]
fn interleave(stations: Vec<Vec<Card>>) -> Vec<Card> {
    let deepest = stations.iter().map(Vec::len).max().unwrap_or(0);
    let mut cards = Vec::new();
    for index in 0..deepest {
        for station in &stations {
            if let Some(card) = station.get(index) {
                cards.push(card.clone());
            }
        }
    }
    cards
}

/// Up to `count` tracks by different artists, starting `rotation` in.
///
/// Kept for tests and possible library-only views. Local radio shelves no longer
/// use likes as seeds: a YouTube like is not evidence MTUI's user knows a song.
#[cfg(test)]
fn distinct_by_artist(tracks: &[Track], count: usize, rotation: usize) -> Vec<Track> {
    let mut artists = HashSet::new();
    let candidates: Vec<&Track> = tracks
        .iter()
        .filter(|track| artists.insert(track.uploader.to_lowercase()))
        .collect();

    if candidates.is_empty() {
        return Vec::new();
    }
    (0..count.min(candidates.len()))
        .map(|i| candidates[(rotation + i) % candidates.len()].clone())
        .collect()
}

/// The "Similar to X" shelf, from the related page of a seed track.
#[cfg(test)]
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
#[cfg(test)]
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
#[cfg(test)]
fn queue(json: &Value) -> Vec<Card> {
    let mut rows = Vec::new();
    collect(json, "playlistPanelVideoRenderer", &mut rows);

    rows.into_iter()
        .filter_map(|row| {
            let video_id = row["videoId"].as_str()?.to_string();
            Some(Card {
                title: row.pointer("/title/runs").and_then(runs_text)?,
                // "Artist • Album • Year", already joined for display.
                subtitle: row
                    .pointer("/longBylineText/runs")
                    .and_then(runs_text)
                    .unwrap_or_default(),
                art: art_url(row.pointer("/thumbnail"))
                    .or_else(|| Some(cover::thumb_url(&video_id))),
                duration: row
                    .pointer("/lengthText/runs")
                    .and_then(runs_text)
                    .as_deref()
                    .and_then(parse_duration),
                artist_ref: row.pointer("/longBylineText/runs").and_then(artist_ref),
                target: Target::Play { video_id },
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

/// The filtered search response is a vertical music shelf rather than one of
/// Home's carousels. Keep only its public playlist rows: YouTube occasionally
/// pads a filtered response with another category, and treating those rows as
/// playlists would make Enter open the wrong kind of page.
#[cfg(test)]
fn parse_community_playlists(json: &Value) -> Option<Shelf> {
    const TITLE: &str = "Community playlists for you";
    const MIN_CARDS: usize = 3;

    let mut shelves = Vec::new();
    collect(json, "musicShelfRenderer", &mut shelves);
    let shelf = shelves.into_iter().find(|shelf| {
        shelf
            .pointer("/title/runs")
            .and_then(runs_text)
            .is_some_and(|title| title.to_ascii_lowercase().contains("community playlist"))
    })?;

    let mut seen = HashSet::new();
    let cards: Vec<Card> = shelf
        .pointer("/contents")?
        .as_array()?
        .iter()
        .filter_map(parse_card)
        .filter_map(|mut card| match &card.target {
            Target::Open { endpoint }
                if endpoint.browse_id.starts_with("VL")
                    && seen.insert(endpoint.browse_id.clone()) =>
            {
                if card.kind().is_none() {
                    card.subtitle = if card.subtitle.is_empty() {
                        "Playlist".to_string()
                    } else {
                        format!("Playlist • {}", card.subtitle)
                    };
                }
                Some(card)
            }
            _ => None,
        })
        .take(SHELF_DEPTH)
        .collect();

    (cards.len() >= MIN_CARDS).then(|| Shelf {
        title: TITLE.to_string(),
        cards,
    })
}

fn parse_card(item: &Value) -> Option<Card> {
    let two_row = &item["musicTwoRowItemRenderer"];
    if two_row.is_object() {
        let title = two_row.pointer("/title/runs").and_then(runs_text)?;
        let subtitle_runs = two_row.pointer("/subtitle/runs");
        let subtitle = subtitle_runs.and_then(runs_text).unwrap_or_default();
        let artist_hint = subtitle.split('•').any(|field| field.trim() == "Artist");
        return Some(Card {
            artist_ref: subtitle_runs.and_then(artist_ref),
            target: target(&two_row["navigationEndpoint"], &title, artist_hint)?,
            title,
            duration: subtitle.split('•').map(str::trim).find_map(parse_duration),
            subtitle,
            art: art_url(two_row.pointer("/thumbnailRenderer")),
        });
    }

    let row = &item["musicResponsiveListItemRenderer"];
    if row.is_object() {
        let title = flex_column(row, 0)?;
        let subtitle = flex_column(row, 1).unwrap_or_default();
        let artist_hint = subtitle.split('•').any(|field| field.trim() == "Artist");
        let target = target(&row["navigationEndpoint"], &title, artist_hint)
            .or_else(|| {
                row.pointer(
                    "/flexColumns/0/musicResponsiveListItemFlexColumnRenderer/text/runs/0/navigationEndpoint",
                )
                .and_then(|endpoint| target(endpoint, &title, artist_hint))
            })
            .or_else(|| video_id(row).map(|video_id| Target::Play { video_id }))?;
        return Some(Card {
            title,
            duration: fixed_column(row, 0)
                .as_deref()
                .and_then(parse_duration)
                .or_else(|| subtitle.split('•').map(str::trim).find_map(parse_duration)),
            artist_ref: flex_runs(row, 1).and_then(artist_ref),
            subtitle,
            // A list row carries a thumbnail too, but not always: falling back
            // to the video's own means a "Quick picks" row is never the one
            // card on the page drawn without a picture.
            art: art_url(row.pointer("/thumbnail")).or_else(|| match &target {
                Target::Play { video_id } => Some(cover::thumb_url(video_id)),
                Target::Open { .. } | Target::Artist { .. } => None,
            }),
            target,
        });
    }

    None
}

/// The artwork URL to fetch out of a thumbnail renderer, taking the smallest
/// size big enough for the tiles the landing page draws.
///
/// Smallest-that-fits rather than largest, and the URL is taken exactly as
/// YouTube wrote it. Both matter. A card is drawn a few dozen pixels across, so
/// the 544px copy is a quarter of a megabyte spent to throw 95% of it away --
/// and rewriting the size suffix to ask for a different one is not the shortcut
/// it appears to be: Google's image CDN answers an invented size with a 500,
/// and takes half a minute to do it.
pub(super) fn art_url(renderer: Option<&Value>) -> Option<String> {
    /// Longest edge worth fetching, in image pixels. Comfortably above the
    /// biggest tile a terminal cell grid can draw -- see [`crate::art::EDGE`].
    const WANTED: u64 = crate::art::EDGE as u64;

    let mut sizes: Vec<(u64, &str)> = renderer?
        .pointer("/musicThumbnailRenderer/thumbnail/thumbnails")?
        .as_array()?
        .iter()
        .filter_map(|thumb| {
            let width = thumb["width"].as_u64()?;
            Some((width, thumb["url"].as_str()?))
        })
        .collect();

    sizes.sort_by_key(|(width, _)| *width);
    // The largest is the fallback rather than the smallest: when every copy is
    // under `WANTED`, the one closest to it is the one with the most detail.
    let (_, url) = sizes
        .iter()
        .find(|(width, _)| *width >= WANTED)
        .or_else(|| sizes.last())?;
    Some((*url).to_string())
}

/// What a card's endpoint does, if it does either.
fn target(endpoint: &Value, label: &str, artist_hint: bool) -> Option<Target> {
    if let Some(id) = endpoint
        .pointer("/watchEndpoint/videoId")
        .and_then(Value::as_str)
    {
        return Some(Target::Play {
            video_id: id.to_string(),
        });
    }
    let route = browse_route(endpoint)?;
    if is_artist_endpoint(endpoint) || artist_hint {
        return Some(Target::Artist {
            artist: ArtistRef {
                name: label.to_string(),
                endpoint: route,
            },
        });
    }
    Some(Target::Open { endpoint: route })
}

fn browse_route(endpoint: &Value) -> Option<BrowseEndpoint> {
    Some(BrowseEndpoint {
        browse_id: endpoint
            .pointer("/browseEndpoint/browseId")?
            .as_str()?
            .to_string(),
        params: endpoint
            .pointer("/browseEndpoint/params")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

fn is_artist_endpoint(endpoint: &Value) -> bool {
    matches!(
        endpoint
            .pointer(
                "/browseEndpoint/browseEndpointContextSupportedConfigs/\
                 browseEndpointContextMusicConfig/pageType",
            )
            .and_then(Value::as_str),
        Some("MUSIC_PAGE_TYPE_ARTIST" | "MUSIC_PAGE_TYPE_USER_CHANNEL")
    )
}

/// First linked Music artist in a run list. Collaborations keep their complete
/// display credit; this chooses only the first stable route for navigation.
pub(super) fn artist_ref(runs: &Value) -> Option<ArtistRef> {
    runs.as_array()?.iter().find_map(|run| {
        let name = run["text"].as_str()?.trim();
        let endpoint = &run["navigationEndpoint"];
        if name.is_empty() || !is_artist_endpoint(endpoint) {
            return None;
        }
        Some(ArtistRef {
            name: name.to_string(),
            endpoint: browse_route(endpoint)?,
        })
    })
}

/// The video id of a list row, from wherever this particular shelf put it.
///
/// Three places, and which one is used varies by shelf rather than by anything
/// about the row: a playlist listing carries it in `playlistItemData`, the
/// trending shelf only in the play button drawn over its thumbnail.
pub(super) fn video_id(row: &Value) -> Option<String> {
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
        uploader: fields
            .first()
            .copied()
            .unwrap_or(UNKNOWN_ARTIST)
            .to_string(),
        // The fixed column is where a playlist listing puts the length. Falling
        // back to the last joined field covers the rows that carry no fixed
        // column at all; a field that is a view count simply fails to parse,
        // which is the same as not being there.
        duration: fixed_column(row, 0)
            .as_deref()
            .and_then(parse_duration)
            .or_else(|| fields.last().copied().and_then(parse_duration)),
        album: flex_column(row, 2).filter(|album| !album.is_empty()),
        artist_ref: flex_runs(row, 1).and_then(artist_ref),
        // These rows are not the user's to remove; only a Data API listing
        // knows an id that could be.
    })
}

/// The duration column of a list row, which is fixed rather than flexible
/// because it is the one column YouTube never lets the layout drop.
pub(super) fn fixed_column(row: &Value, index: usize) -> Option<String> {
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
    subtitle.split('•').map(str::trim).find(|field| {
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
    fn a_picture_card_keeps_its_timer_out_of_the_subtitle() {
        let json = shelf(
            "Listen again",
            vec![card(
                "Creep",
                "Song • Radiohead • 3:58",
                serde_json::json!({ "watchEndpoint": { "videoId": "XFkzRNyygfk" } }),
            )],
        );

        let card = &parse_shelves(&json)[0].cards[0];
        assert_eq!(card.duration, Some(Duration::from_secs(238)));
        assert_eq!(card.detail(), "Radiohead");
    }

    #[test]
    fn a_responsive_card_falls_back_to_a_timer_in_its_subtitle() {
        let wrapped = shelf(
            "Quick picks",
            vec![serde_json::json!({
                "musicResponsiveListItemRenderer": row(
                    "JhulBGMA7G4",
                    &["Harder, Better, Faster, Stronger", "Daft Punk • 3:47"],
                    None,
                )
            })],
        );

        let card = &parse_shelves(&wrapped)[0].cards[0];
        assert_eq!(card.duration, Some(Duration::from_secs(227)));
        assert_eq!(card.detail(), "Daft Punk");
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
            Target::Open { endpoint } => assert_eq!(endpoint.browse_id, "MPREb_abc"),
            Target::Play { .. } | Target::Artist { .. } => panic!("an album is not a video"),
        }
    }

    #[test]
    fn an_artist_card_keeps_its_semantic_route_and_params() {
        let endpoint = serde_json::json!({
            "browseEndpoint": {
                "browseId": "UCGz-artist",
                "params": "opaque-section-params",
                "browseEndpointContextSupportedConfigs": {
                    "browseEndpointContextMusicConfig": {
                        "pageType": "MUSIC_PAGE_TYPE_ARTIST"
                    }
                }
            }
        });
        let json = shelf(
            "Artists",
            vec![card("Tame Impala", "Artist • 30M subscribers", endpoint)],
        );

        let card = &parse_shelves(&json)[0].cards[0];
        let Target::Artist { artist } = &card.target else {
            panic!("an artist page was flattened into a collection");
        };
        assert_eq!(artist.name, "Tame Impala");
        assert_eq!(artist.endpoint.browse_id, "UCGz-artist");
        assert_eq!(
            artist.endpoint.params.as_deref(),
            Some("opaque-section-params")
        );
    }

    #[test]
    fn a_user_channel_page_type_keeps_its_artist_link() {
        let runs = serde_json::json!([ {
            "text": "MrSuicideSheep",
            "navigationEndpoint": {
                "browseEndpoint": {
                    "browseId": "UC5nc_ZtjKW1htCVZVRxlQAQ",
                    "browseEndpointContextSupportedConfigs": {
                        "browseEndpointContextMusicConfig": {
                            "pageType": "MUSIC_PAGE_TYPE_USER_CHANNEL"
                        }
                    }
                }
            }
        } ]);

        let artist = artist_ref(&runs).expect("a user channel is still an artist route");
        assert_eq!(artist.name, "MrSuicideSheep");
        assert_eq!(artist.endpoint.browse_id, "UC5nc_ZtjKW1htCVZVRxlQAQ");
    }

    #[test]
    fn an_artist_badge_is_the_fallback_when_page_type_is_missing() {
        let json = shelf(
            "Fans might also like",
            vec![card(
                "Metronomy",
                "Artist • 1.15M monthly audience",
                serde_json::json!({ "browseEndpoint": { "browseId": "UCmetronomy" } }),
            )],
        );

        assert!(matches!(
            parse_shelves(&json)[0].cards[0].target,
            Target::Artist { .. }
        ));
    }

    #[test]
    fn a_playable_card_keeps_its_linked_artist() {
        let json = shelf(
            "Songs",
            vec![serde_json::json!({ "musicTwoRowItemRenderer": {
                "title": { "runs": [ { "text": "Let It Happen" } ] },
                "subtitle": { "runs": [
                    { "text": "Song • " },
                    { "text": "Tame Impala", "navigationEndpoint": {
                        "browseEndpoint": {
                            "browseId": "UCGz-artist",
                            "browseEndpointContextSupportedConfigs": {
                                "browseEndpointContextMusicConfig": {
                                    "pageType": "MUSIC_PAGE_TYPE_ARTIST"
                                }
                            }
                        }
                    } }
                ] },
                "navigationEndpoint": { "watchEndpoint": { "videoId": "aBcDeFgHiJk" } }
            } })],
        );

        let track = parse_shelves(&json)[0].cards[0].track().unwrap();
        assert_eq!(
            track
                .artist_ref
                .as_ref()
                .map(|artist| artist.endpoint.browse_id.as_str()),
            Some("UCGz-artist")
        );
    }

    fn community_row(id: &str, title: &str, subtitle: &str) -> Value {
        let mut item = row("unused", &[title, subtitle], None);
        item["playlistItemData"] = Value::Null;
        item["navigationEndpoint"] = serde_json::json!({ "browseEndpoint": { "browseId": id } });
        serde_json::json!({ "musicResponsiveListItemRenderer": item })
    }

    #[test]
    fn reads_filtered_community_playlists_as_open_cards() {
        let json = serde_json::json!({
            "contents": { "sectionListRenderer": { "contents": [ {
                "musicShelfRenderer": {
                    "title": { "runs": [ { "text": "Community playlists" } ] },
                    "contents": [
                        community_row("VLone", "Night drive", "Alex • 42 songs"),
                        community_row("VLtwo", "Soft focus", "Mina • 31 songs"),
                        community_row("VLthree", "After hours", "Sam • 67 songs")
                    ]
                }
            } ] } }
        });

        let shelf = parse_community_playlists(&json).expect("community shelf should parse");
        assert_eq!(shelf.title, "Community playlists for you");
        assert_eq!(shelf.cards.len(), 3);
        assert_eq!(shelf.cards[0].title, "Night drive");
        assert_eq!(shelf.cards[0].kind(), Some("Playlist"));
        assert_eq!(shelf.cards[0].detail(), "Alex • 42 songs");
        match &shelf.cards[0].target {
            Target::Open { endpoint } => assert_eq!(endpoint.browse_id, "VLone"),
            Target::Play { .. } | Target::Artist { .. } => {
                panic!("a community playlist must browse")
            }
        }
    }

    #[test]
    fn community_playlist_results_are_filtered_deduplicated_and_need_a_row() {
        let json = serde_json::json!({
            "contents": [
                { "musicShelfRenderer": {
                    "title": { "runs": [ { "text": "Songs" } ] },
                    "contents": [
                        community_row("VLpadding", "Not the filtered shelf", "Somebody"),
                        community_row("VLpadding2", "Still padding", "Somebody"),
                        community_row("VLpadding3", "More padding", "Somebody")
                    ]
                } },
                { "musicShelfRenderer": {
                    "title": { "runs": [ { "text": "Community playlists" } ] },
                    "contents": [
                        community_row("VLone", "One", "A"),
                        community_row("VLone", "One again", "A"),
                        community_row("MPREalbum", "Not a playlist", "Album"),
                        community_row("VLtwo", "Two", "B")
                    ]
                } }
            ]
        });

        assert!(
            parse_community_playlists(&json).is_none(),
            "duplicates, albums, and padded shelves must not make a full row"
        );
    }

    #[test]
    fn primary_home_sections_have_one_stable_order() {
        let make = |title: &str| Shelf {
            title: title.to_string(),
            cards: vec![playable(title)],
        };
        let mut shelves = vec![
            make("New releases"),
            make("Community playlists for you"),
            make("Forgotten favorites"),
            make("Listen again"),
            make("Quick picks"),
            make("Albums for you"),
        ];

        order_shelves(&mut shelves);

        assert_eq!(
            shelves
                .iter()
                .map(|shelf| shelf.title.as_str())
                .collect::<Vec<_>>(),
            vec![
                "Quick picks",
                "Listen again",
                "Forgotten favorites",
                "Community playlists for you",
                "New releases",
                "Albums for you",
            ]
        );
    }

    /// Checks the filter token and the full search-to-browse path against the
    /// live catalogue without depending on a saved account.
    #[test]
    #[ignore = "hits the live YouTube Music API"]
    fn community_playlist_search_against_the_live_api() {
        let http = Http::new().expect("client should build");
        let json = post(
            &http,
            SEARCH_URL,
            None,
            serde_json::json!({
                "query": "Tame Impala The Less I Know the Better",
                "params": COMMUNITY_PLAYLISTS_FILTER,
            }),
        )
        .expect("community playlist search should answer");
        let shelf = parse_community_playlists(&json).expect("community playlists should parse");
        let Target::Open { endpoint } = &shelf.cards[0].target else {
            panic!("a community result should open a playlist");
        };

        assert!(endpoint.browse_id.starts_with("VL"));
        assert!(
            !tracks_endpoint(&http, endpoint)
                .expect("the first community playlist should open")
                .is_empty()
        );
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
    fn only_personal_inner_tube_sections_authenticate_home() {
        let generic = vec![Shelf {
            title: "Featured playlists for you".to_string(),
            cards: vec![playable("generic")],
        }];
        assert!(!is_personalised(&generic));

        for title in [
            "Quick picks",
            "Listen again",
            "Heard in Shorts",
            "Similar to A",
        ] {
            let shelves = vec![Shelf {
                title: title.to_string(),
                cards: vec![playable("personal")],
            }];
            assert!(is_personalised(&shelves), "{title} was not recognised");
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
    }

    #[test]
    fn a_view_count_is_not_mistaken_for_a_duration() {
        // The trending shelf carries "Channel • 2.6M views" and no fixed
        // column. Neither field is a length, so the row is honestly LIVE-less
        // rather than three million seconds long.
        let track = parse_row(&row(
            "abcdefghijk",
            &["Some video", "Vie Channel • 2.6M views"],
            None,
        ))
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

    /// A playable card, for the assembly tests below.
    fn playable(id: &str) -> Card {
        Card {
            title: format!("song {id}"),
            subtitle: "Song • Someone".to_string(),
            art: None,
            duration: None,
            artist_ref: None,
            target: Target::Play {
                video_id: id.to_string(),
            },
        }
    }

    #[test]
    fn stations_are_interleaved_rather_than_concatenated() {
        // The first screenful is the only part most people see, so it has to
        // carry all four stations rather than the whole of the first one.
        let cards = interleave(vec![
            vec![playable("a1"), playable("a2"), playable("a3")],
            vec![playable("b1"), playable("b2")],
            vec![playable("c1")],
        ]);

        let ids: Vec<&str> = cards
            .iter()
            .map(|card| match &card.target {
                Target::Play { video_id } => video_id.as_str(),
                Target::Open { .. } | Target::Artist { .. } => unreachable!(),
            })
            .collect();
        // Round by round, and a station that runs out is simply skipped rather
        // than padding the rounds after it.
        assert_eq!(ids, ["a1", "b1", "c1", "a2", "b2", "a3"]);
    }

    #[test]
    fn a_song_used_by_one_shelf_is_not_reused_by_the_next() {
        let mut shelves = Vec::new();
        let mut seen = HashSet::new();

        push(
            &mut shelves,
            &mut seen,
            "Listen again",
            vec![playable("a"), playable("b"), playable("c")],
        );
        // Two of these are already above, so what is left is too thin to be a
        // row and the shelf is dropped rather than shown with one card in it.
        push(
            &mut shelves,
            &mut seen,
            "Quick picks",
            vec![playable("a"), playable("b"), playable("d")],
        );
        assert_eq!(shelves.len(), 1, "a shelf of leftovers was kept");

        push(
            &mut shelves,
            &mut seen,
            "Similar to x",
            vec![playable("d"), playable("e"), playable("f"), playable("a")],
        );
        assert_eq!(shelves.len(), 2);
        assert_eq!(shelves[1].cards.len(), 3, "the duplicate survived");
    }

    #[test]
    fn a_radio_song_is_only_kept_once_within_its_shelf() {
        let mut shelves = Vec::new();
        let mut seen = HashSet::new();
        push(
            &mut shelves,
            &mut seen,
            "From your listening",
            vec![playable("a"), playable("a"), playable("b"), playable("c")],
        );

        assert_eq!(shelves.len(), 1);
        assert_eq!(shelves[0].cards.len(), 3);
    }

    #[test]
    fn albums_never_collide_with_songs() {
        // Two cards can carry the same id in different senses -- a browse id is
        // not a video id -- so only playable cards are deduplicated.
        let mut shelves = Vec::new();
        let mut seen = HashSet::new();
        let album = || Card {
            title: "Currents".to_string(),
            subtitle: "Album • Tame Impala".to_string(),
            art: None,
            duration: None,
            artist_ref: None,
            target: Target::Open {
                endpoint: BrowseEndpoint::new("MPREb_abc"),
            },
        };

        push(
            &mut shelves,
            &mut seen,
            "Albums",
            vec![album(), album(), album()],
        );
        assert_eq!(shelves[0].cards.len(), 3);
    }

    #[test]
    fn cold_start_seeds_spread_across_artists() {
        // With no journal there is nothing scored, so seeds come off the like
        // list -- but the one-artist-per-station rule still has to hold, or a
        // library dominated by one artist gives four copies of one station.
        let likes = vec![
            track("a", "Dominic Fike"),
            track("b", "Dominic Fike"),
            track("c", "Mr.Kitty"),
            track("d", "Crystal Castles"),
        ];

        let seeds = distinct_by_artist(&likes, 4, 0);
        assert_eq!(seeds.len(), 3, "one artist took two seeds");
        assert_eq!(seeds[0].id, "a");

        // Rotating steps along and wraps rather than running out.
        assert_eq!(distinct_by_artist(&likes, 1, 1)[0].id, "c");
        assert_eq!(distinct_by_artist(&likes, 1, 3)[0].id, "a");
        assert!(distinct_by_artist(&[], 4, 0).is_empty());
    }

    fn track(id: &str, artist: &str) -> Track {
        Track {
            id: id.to_string(),
            title: format!("song {id}"),
            uploader: artist.to_string(),
            duration: None,
            album: None,
            artist_ref: None,
        }
    }

    #[test]
    fn a_label_drops_the_type_marker_and_the_counts() {
        assert_eq!(artist("Song • Radiohead"), Some("Radiohead"));
        assert_eq!(artist("Album • TEMPOREX"), Some("TEMPOREX"));
        assert_eq!(
            artist("Tame Impala • 68M plays • The Slow Rush"),
            Some("Tame Impala")
        );
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
        // `available`, not `load`: a cookie read out of the browser has to buy
        // the same feed a hand-pasted one does, and testing only the pasted
        // path would leave the one almost everybody uses uncovered.
        let Some(cookies) = Cookies::available().expect("the saved cookies should parse") else {
            println!("no cookie saved or imported -- nothing to check");
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

    #[test]
    #[ignore = "hits the live YouTube Music API and image CDN"]
    fn visible_home_artwork_batch() {
        use std::time::Instant;

        let http = Http::new().expect("client should build");
        let cookies = Cookies::available()
            .expect("the saved cookies should parse")
            .expect("a saved YouTube Music session should exist");
        let shelves = fetch_personalised(&http, &cookies)
            .expect("home should answer")
            .unwrap_or_else(|| fetch_public(&http).expect("public home should answer"));
        let requests: Vec<(String, String)> = shelves
            .iter()
            .take(3)
            .flat_map(|shelf| shelf.cards.iter().take(4))
            .filter_map(|card| Some((card.art_key().to_string(), card.art.as_ref()?.clone())))
            .collect();

        let fetcher = cover::ArtFetcher::new().expect("art fetcher should build");
        let start = Instant::now();
        let total = requests.len();
        let mut loaded = 0;
        fetcher.fetch_many(requests, crate::art::EDGE, |_, art| {
            loaded += usize::from(art.is_some());
            true
        });
        println!(
            "art: {loaded}/{total} in {:.2}s",
            start.elapsed().as_secs_f64()
        );
        assert!(loaded > 0);
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
        let (shelves, personalised) = fetch(&http, None).expect("the home feed should come back");
        assert!(!personalised);
        println!(
            "home: {:.2}s, {} shelves",
            start.elapsed().as_secs_f64(),
            shelves.len()
        );

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

        // Signed-out Home can consist entirely of browsable playlists.
        assert!(
            shelves.iter().any(|shelf| !shelf.cards.is_empty()),
            "no shelf carried any cards"
        );
    }
}

#[cfg(test)]
mod live_personal {
    use super::*;

    /// The shelves built from this account's listening, end to end, against the
    /// real journal on this machine.
    ///
    /// Prints the page twice, at two rotations. That is the part worth looking
    /// at by eye: the complaint this whole path answers is that the page read
    /// the same every time, and two identical printouts here would mean it
    /// still does.
    ///
    /// `cargo test built_shelves -- --ignored --nocapture`
    #[test]
    #[ignore = "hits the live API using the local listening journal"]
    fn built_shelves_against_the_live_account() {
        use std::time::Instant;

        use crate::source::journal::Journal;

        let http = Http::new().expect("client should build");
        let journal = Journal::load();
        let likes = Vec::new();

        let mut pages = Vec::new();
        for rotation in [0, 1] {
            let start = Instant::now();
            let shelves = personal(&http, &likes, &journal, rotation);
            println!(
                "\n=== rotation {rotation}: {} shelves in {:.2}s ===",
                shelves.len(),
                start.elapsed().as_secs_f64()
            );

            for shelf in &shelves {
                println!("{} ({} cards)", shelf.title, shelf.cards.len());
                for card in shelf.cards.iter().take(4) {
                    println!("   {:<44.42} {:.34}", card.title, card.subtitle);
                }
                assert!(!shelf.cards.is_empty(), "{} is empty", shelf.title);
            }

            // No song may appear on the page twice, whatever it scored.
            let mut seen = HashSet::new();
            for shelf in &shelves {
                for card in &shelf.cards {
                    if let Target::Play { video_id } = &card.target {
                        assert!(
                            seen.insert(video_id.clone()),
                            "{video_id} appears on the page more than once"
                        );
                    }
                }
            }
            pages.push(seen);
        }

        if likes.is_empty() && pages[0].is_empty() {
            println!("\nno likes and an empty journal -- there is nothing to build a page from");
            return;
        }
        assert!(
            !pages[0].is_empty(),
            "the page came back with nothing playable"
        );

        // Not an assert: with one artist in the journal there is only one seed
        // to rotate through, and that is a thin history rather than a bug.
        let shared = pages[0].intersection(&pages[1]).count();
        println!(
            "\nrotation 0 and 1 share {shared} of {} songs",
            pages[0].len()
        );
    }
}
