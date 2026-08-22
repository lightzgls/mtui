//! The player page: everything YouTube Music shows *beside* a track while it
//! plays -- the queue it will play next, the lyrics, the comments, and what it
//! would recommend afterwards.
//!
//! Four panels, and deliberately four separate fetches. Only the queue is
//! wanted the instant a track starts, because it is what decides what plays
//! when this one ends; the other three are one HTTPS call each, made the first
//! time the user actually opens that tab. Fetching all of them on every play
//! would be three round trips spent on panels most plays never look at.
//!
//! The queue is also the one panel that is never finished with. A radio has no
//! last page, only a token for the next one, so [`fetch`] hands back where the
//! rest of it lives and [`continue_queue`] redeems that as the queue is played
//! down. When a station finally has nothing left -- a playlist that ends, a
//! radio out of material -- [`seeded_page`] builds a new one out of the play
//! journal rather than letting the music stop. Which of the two answered is not
//! something the caller has to care about: both hand back a [`QueuePage`].
//!
//! Same bargain as [`crate::source::home`]: these are internal endpoints, so
//! everything here degrades to an empty panel rather than an error. A tab that
//! comes back with nothing says so and the music keeps playing. The queue holds
//! to that too -- every way of extending it can fail, and all any failure costs
//! is a queue that ends where it used to end anyway.
//!
//! Comments are the exception to "same corpus": YouTube Music does not serve
//! them, so they come from youtube.com proper, as its web client asks for them
//! -- one call for a continuation token and a second to redeem it.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::Value;

use super::auth::Http;
use super::home::{self, Shelf};
use super::innertube::parse_duration;
use super::journal::Journal;
use super::sapisid;
use super::{Track, UNKNOWN_ARTIST};

/// youtube.com's own watch endpoint. Not the music one: comments live here.
const YOUTUBE_NEXT_URL: &str = "https://www.youtube.com/youtubei/v1/next";

/// The plain web client, which is the one served a comment section. These two
/// travel together -- see [`crate::source::innertube`] on mismatched identities.
const WEB_CLIENT_NAME: &str = "WEB";
const WEB_CLIENT_VERSION: &str = "2.20241127.01.00";

/// Ceiling on the rows taken from *one* response.
///
/// No longer a bound on the queue itself, which is now paged indefinitely and
/// is bounded by the window [`crate::app`] keeps around the playing track. This
/// is the narrower job of not letting a single response of an unexpected shape
/// hand back an unbounded number of rows.
///
/// Measured 2026-08-11: a first page is 50 tracks and each continuation after
/// it around 49, so this sits above what YouTube actually sends rather than
/// truncating it.
const MAX_QUEUE: usize = 60;

/// Seeds considered when building a station from the play journal.
///
/// More than one because the best-scoring candidate may be a track the user
/// has just been playing, which is exactly what must not be seeded from. Deep
/// enough to find one that is not, shallow enough that it is still drawn from
/// the top of what they listen to. See [`seeded_page`].
const SEED_CANDIDATES: usize = 8;

/// Comments held for one track. The first page of the API returns twenty; this
/// is a bound on what a continuation could add rather than a target.
const MAX_COMMENTS: usize = 50;

/// Lyrics longer than this are not lyrics -- they are a description field that
/// happened to land in the same renderer.
const MAX_LYRICS: usize = 8_000;

/// Ceiling on the timed lines kept for one track. A song runs to well under a
/// hundred; this is a bound on a response that has stopped being a song.
///
/// Shared with [`crate::source::lrclib`], which parses the same lines out of a
/// different source: two bounds on one panel would be two numbers to keep in
/// step, and the panel does not care which provider filled it.
pub(super) const MAX_TIMED_LINES: usize = 400;

/// The client version served lyrics *with their timings*.
///
/// Deliberately not [`crate::source::innertube::MUSIC_CLIENT_VERSION`], which
/// every other call here pins and shares. The lyrics browse is the one endpoint
/// whose answer depends on how new the client claims to be: asked as the pinned
/// version it returns the same words as a flat block of text, and asked as this
/// one it returns them line by line with a cue range on each. Both shapes are
/// parsed below, so a version Google eventually stops serving costs the
/// highlighting and nothing else -- which is why this is a local exception to
/// the one-identity rule rather than a bump of the shared constant, whose job
/// is to keep search, browse and the queue from drifting apart.
const TIMED_LYRICS_CLIENT_VERSION: &str = "1.20250122.01.00";

/// The queue a track plays inside, and where to find the rest of its page.
///
/// The browse ids are handed back rather than followed, because following them
/// is a round trip each and neither panel is on screen yet. The continuation is
/// held back for a different reason: there is nothing wrong with the queue that
/// arrived, and the page after it is not wanted until the queue has been played
/// most of the way down.
#[derive(Debug, Clone, Default)]
pub struct Watch {
    /// What YouTube calls the queue: "Let It Happen Mix", "Currents", or the
    /// playlist the track was started from.
    pub queue_title: String,
    /// The queue itself, starting with the track that seeded it.
    pub queue: Vec<Track>,
    /// Browse id of the "Lyrics" tab, when this track has one. Instrumentals
    /// and most videos do not.
    pub lyrics_id: Option<String>,
    /// Browse id of the "Related" tab.
    pub related_id: Option<String>,
    /// What to redeem for the rest of the queue. See [`continue_queue`].
    ///
    /// `None` for a queue with no more pages -- a short playlist -- and for one
    /// whose token we no longer recognise, which costs the endless queue and
    /// nothing else.
    pub continuation: Option<String>,
}

/// A further page of a queue, and where the page after it lives.
#[derive(Debug, Clone, Default)]
pub struct QueuePage {
    pub tracks: Vec<Track>,
    /// `None` at the end of a finite queue. A playlist does run out, even
    /// though a radio does not.
    pub continuation: Option<String>,
    /// What the queue should now call itself, when this page came from a
    /// different station than the one before it.
    ///
    /// `None` for a continuation, which is more of the same station and must
    /// not rename it. `Some` only for [`seeded_page`], where the tracks after
    /// this point genuinely belong to something else -- and a panel still
    /// headed "Never Gonna Give You Up Mix" while playing a station built from
    /// somebody's listening history is a panel that is lying.
    pub title: Option<String>,
}

/// Lyrics, and whoever YouTube licensed them from.
#[derive(Debug, Clone)]
pub struct Lyrics {
    /// The whole thing as one block, which is all there is to show for a track
    /// whose lyrics were never timed.
    pub text: String,
    /// "Source: Musixmatch". Shown because the tab is otherwise a wall of text
    /// with no indication it came from anywhere.
    pub source: Option<String>,
    /// The same words split into the lines they are sung as, each with the
    /// moment it starts. Empty when YouTube published no timings for this
    /// track, which is the common case outside its own catalogue.
    pub timed: Vec<TimedLine>,
}

/// One line of lyrics and the moment the singer reaches it.
///
/// The cue range YouTube sends also states where the line *ends*, which is not
/// kept: for all but the gaps it is the next line's start, and through a gap
/// the line just sung is the one that should stay marked anyway. Nothing here
/// can answer a question the start does not, and an unread field is one more
/// thing to keep true.
#[derive(Debug, Clone)]
pub struct TimedLine {
    pub text: String,
    pub start: Duration,
}

impl Lyrics {
    /// Lyrics built from timed lines, whichever source produced them.
    ///
    /// The block is derived here rather than at the point of use so that the
    /// two cannot disagree about how the lines are joined: a narrow panel may
    /// still want to draw the whole thing as text, and it should be the same
    /// text either way.
    ///
    /// An empty credit is treated as no credit: the panel draws the line it is
    /// given, and a blank one is a row of the song's height spent saying
    /// nothing.
    pub(super) fn from_timed(timed: Vec<TimedLine>, source: Option<&str>) -> Self {
        Self {
            text: timed
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            source: source
                .map(str::trim)
                .filter(|source| !source.is_empty())
                .map(str::to_string),
            timed,
        }
    }

    /// Which line is being sung at `position`, if any.
    ///
    /// The *last* line to have started rather than the one whose range covers
    /// the position: through an instrumental break the line just sung stays
    /// marked, which is what a listener reading along expects, and what YouTube
    /// Music itself does. `None` only before the first line and for a track
    /// with no timings at all.
    pub fn singing(&self, position: Duration) -> Option<usize> {
        self.timed.iter().rposition(|line| line.start <= position)
    }
}

/// One top-level comment. Replies are counted but not fetched: each thread is
/// a further round trip, and a terminal music player is not a comment reader.
#[derive(Debug, Clone)]
pub struct Comment {
    pub author: String,
    /// "4 months ago", as YouTube already wrote it.
    pub published: String,
    pub text: String,
    /// "10K", already abbreviated by YouTube. A string rather than a number
    /// because that is what comes back, and re-deriving "10K" from 10,431 would
    /// only be a way to disagree with the site.
    pub likes: String,
    pub replies: String,
}

/// A comment section: its total, and as much of the first page as we kept.
#[derive(Debug, Clone, Default)]
pub struct Comments {
    /// "8,407 Comments". Empty when YouTube did not say.
    pub total: String,
    pub items: Vec<Comment>,
}

/// The watch queue for `video_id`.
///
/// `RDAMVM<id>` is the radio YouTube Music seeds from a single song, which is
/// what its own player does when a track is played from search rather than from
/// inside a playlist. Without it the response carries the one track and nothing
/// to play afterwards.
pub fn fetch(http: &Http, video_id: &str) -> Result<Watch> {
    let json = home::post(
        http,
        home::NEXT_URL,
        None,
        serde_json::json!({
            "videoId": video_id,
            "playlistId": format!("RDAMVM{video_id}"),
        }),
    )?;

    let watch = Watch {
        queue_title: queue_title(&json).unwrap_or_default(),
        queue: queue(&json),
        lyrics_id: tab_id(&json, "Lyrics"),
        related_id: tab_id(&json, "Related"),
        continuation: queue_token(&json),
    };

    // A response with no queue at all is one whose shape we no longer
    // recognise; the caller shows an empty panel rather than a wrong one.
    if watch.queue.is_empty() {
        bail!("YouTube Music returned no queue for this track");
    }
    Ok(watch)
}

/// The next page of a queue, and the token for the page after it.
///
/// This is what lets "Up next" run on indefinitely: a radio has no last page,
/// only a token for the next one, and redeeming them in turn is how the site
/// itself never runs out. One round trip buys around fifty tracks -- measured,
/// not estimated -- so at the rate a queue is consumed this is a single call
/// every two and a half hours of music.
///
/// Asked as the music client, like [`fetch`] and unlike [`comments`]: these are
/// the same rows [`queue`] parses, and a continuation redeemed under a client
/// that was not issued it comes back empty rather than refused.
pub fn continue_queue(http: &Http, token: &str) -> Result<QueuePage> {
    let json = home::post(
        http,
        home::NEXT_URL,
        None,
        serde_json::json!({ "continuation": token }),
    )?;

    let tracks = queue(&json);
    // Distinguished from "no token" by the caller: a page that came back empty
    // is the end of the radio, and there is nothing further to ask for either
    // way. Reported as an error so that a response whose shape we have stopped
    // recognising does not read as a queue that politely ended.
    if tracks.is_empty() {
        bail!("YouTube Music returned no further tracks for this queue");
    }
    Ok(QueuePage {
        continuation: queue_token(&json),
        tracks,
        // More of the station already playing, so it keeps its name.
        title: None,
    })
}

/// A fresh station, seeded from what this user actually listens to.
///
/// The fallback for when [`continue_queue`] has nothing left to give: a
/// playlist that genuinely ended, a radio that ran out of material, or a token
/// YouTube has stopped honouring. Rather than let the music stop, a new station
/// is started from the play journal and the queue carries on into it.
///
/// The seed is a track the user keeps *playing*, not one they once liked --
/// see [`crate::source::journal::Taste::seeds`], which also drops anything they
/// skip more often than they finish and keeps each seed to a different artist.
/// `rotation` steps through those candidates, so a second call after a station
/// that led nowhere builds a different one instead of the same one again.
///
/// Deliberately asked with no like list. Likes need a session and this runs on
/// the page thread, but that is not the reason: a like is something people do
/// once and forget, and what should play next is better predicted by what they
/// keep coming back to. The journal knows that and the like list does not.
pub fn seeded_page(http: &Http, journal: &Journal, rotation: usize) -> Result<QueuePage> {
    let taste = journal.taste(&[], sapisid::unix_now());
    // A first run, or close to it. There is no opinion to build a station from
    // and inventing one would be worse than the queue ending: the user would
    // get a station built from the two songs they have tried so far.
    if !taste.is_informed() {
        bail!("not enough listening history yet to build a station from");
    }

    // Something loved but not *just* played. Seeding from a song someone has
    // had on all morning returns that morning's songs, which is the one way a
    // station like this reliably looks broken -- the same reason the landing
    // page holds its radios off what it has recently played.
    let Some(seed) = taste
        .seeds(SEED_CANDIDATES, rotation, &HashSet::new())
        .into_iter()
        .next()
        .map(|ranked| ranked.track.clone())
    else {
        bail!("everything worth building a station from has just been played");
    };

    let json = home::post(
        http,
        home::NEXT_URL,
        None,
        serde_json::json!({
            "videoId": seed.id,
            "playlistId": format!("RDAMVM{}", seed.id),
        }),
    )?;

    // The station's first entry is the seed, which is a track the user already
    // knows well -- the same exclusion the landing page's radios make.
    let tracks: Vec<Track> = queue(&json)
        .into_iter()
        .filter(|track| track.id != seed.id)
        .collect();
    if tracks.is_empty() {
        bail!("the station seeded from {} came back empty", seed.label());
    }

    Ok(QueuePage {
        continuation: queue_token(&json),
        tracks,
        // A different station, so it says so. Falls back to naming the seed
        // when YouTube did not name it, which is still the truth about where
        // these tracks came from.
        title: Some(queue_title(&json).unwrap_or_else(|| format!("Station from {}", seed.label()))),
    })
}

/// The lyrics behind the browse id [`fetch`] handed back.
///
/// One call, two possible shapes back. Asked as a client new enough to render
/// them, YouTube answers with the lines and their cue ranges, which is what
/// lets the panel follow the singer; asked about a track it has no timings for
/// -- a cover, a live take, most of what is not in its own catalogue -- it
/// answers the older way, with the words in one block. The timed shape is tried
/// first and the block is the fallback, so the tab has lyrics in it either way
/// and only the highlighting depends on which arrived.
pub fn lyrics(http: &Http, browse_id: &str) -> Result<Lyrics> {
    let json = home::browse_as(http, None, browse_id, TIMED_LYRICS_CLIENT_VERSION)?;

    timed_lyrics(&json)
        .or_else(|| block_lyrics(&json))
        .ok_or_else(|| anyhow::anyhow!("no lyrics are published for this track"))
}

/// Lyrics line by line, each with the moment it is sung.
///
/// The lines arrive beside the render tree rather than inside it, under a model
/// the web client hands to its own lyrics component, so they are found by
/// walking for the array rather than by a path through renderers that would be
/// correct for exactly one layout.
fn timed_lyrics(json: &Value) -> Option<Lyrics> {
    let mut found = Vec::new();
    home::collect(json, "timedLyricsData", &mut found);

    let timed: Vec<TimedLine> = found
        .first()?
        .as_array()?
        .iter()
        .filter_map(parse_timed_line)
        .take(MAX_TIMED_LINES)
        .collect();
    if timed.is_empty() {
        return None;
    }

    let mut sources = Vec::new();
    home::collect(json, "sourceMessage", &mut sources);
    let source = sources.first().and_then(|source| source.as_str());

    Some(Lyrics::from_timed(timed, source))
}

/// One line and the start of its cue range.
///
/// A line with no start is dropped rather than placed at zero: it would mark
/// itself as the one being sung for the whole of the intro, which is worse than
/// the line simply not being there.
fn parse_timed_line(raw: &Value) -> Option<TimedLine> {
    Some(TimedLine {
        text: raw["lyricLine"].as_str()?.to_string(),
        start: Duration::from_millis(millis(&raw["cueRange"]["startTimeMilliseconds"])?),
    })
}

/// A time in milliseconds, however this response chose to write it.
///
/// A 64-bit field is a *string* in Google's JSON encoding of protobuf, which is
/// what these are -- but the number is accepted too, because a field that only
/// ever parses one way is a field nobody notices has changed shape.
fn millis(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
}

/// The older shape: every word in one description shelf, and no timings.
fn block_lyrics(json: &Value) -> Option<Lyrics> {
    let mut shelves = Vec::new();
    home::collect(json, "musicDescriptionShelfRenderer", &mut shelves);
    let shelf = shelves.first()?;

    let mut text = shelf
        .pointer("/description/runs")
        .and_then(home::runs_text)?;
    text.truncate(
        text.char_indices()
            .nth(MAX_LYRICS)
            .map_or(text.len(), |(at, _)| at),
    );

    Some(Lyrics {
        text,
        source: shelf.pointer("/footer/runs").and_then(home::runs_text),
        timed: Vec::new(),
    })
}

/// The "Related" tab, as the same shelves of cards the landing page is built
/// from -- which is exactly what it is.
pub fn related(http: &Http, browse_id: &str) -> Result<Vec<Shelf>> {
    let shelves = home::parse_shelves(&home::browse(http, None, browse_id)?);
    if shelves.is_empty() {
        bail!("nothing related came back for this track");
    }
    Ok(shelves)
}

/// The comment section for `video_id`, in two calls.
///
/// The first fetches the watch page, which does not carry comments -- only a
/// continuation token standing where they will go. The second redeems it. This
/// is how the site itself loads them, and there is no endpoint that skips the
/// first step: the token is signed and per-video.
pub fn comments(http: &Http, video_id: &str) -> Result<Comments> {
    let page = post_web(http, serde_json::json!({ "videoId": video_id }))?;

    let Some(token) = comment_token(&page) else {
        // YouTube puts its own reason where the comments would have gone, and
        // it is a better one than we could infer: "Comments are turned off",
        // or "Restricted Mode has hidden comments for this video" -- which is
        // a fact about the network the user is on, not about the track, and
        // sending them to look at the wrong thing would be worse than silence.
        bail!(
            "{}",
            notice(&page).unwrap_or_else(|| "no comments came back for this track".to_string())
        );
    };
    let json = post_web(http, serde_json::json!({ "continuation": token }))?;

    let items = parse_comments(&json);
    if items.is_empty() {
        bail!("no comments came back for this track");
    }
    Ok(Comments {
        total: comment_count(&json).unwrap_or_default(),
        items,
    })
}

/// One call against youtube.com's InnerTube, as its web client.
///
/// Separate from [`home::post`] because the client identity differs, and that
/// is not a detail: the music client is not served a comment section at all.
fn post_web(http: &Http, extra: Value) -> Result<Value> {
    let mut body = serde_json::json!({
        "context": {
            "client": {
                "clientName": WEB_CLIENT_NAME,
                "clientVersion": WEB_CLIENT_VERSION,
                "hl": "en",
            }
        }
    });
    if let Some(fields) = extra.as_object() {
        for (key, value) in fields {
            body[key] = value.clone();
        }
    }

    let request = http
        .client()
        .post(YOUTUBE_NEXT_URL)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(serde_json::to_vec(&body)?);

    let (status, raw) = http.send(request)?;
    if !(200..300).contains(&status) {
        bail!("YouTube refused the request: HTTP {status}");
    }
    Ok(serde_json::from_slice(&raw)?)
}

/// The queue's name: "Let It Happen Mix", or the playlist it was started from.
///
/// The queue header is where a radio names itself, and it puts the name in its
/// *subtitle* -- its title is the fixed words "Playing from", which the panel
/// draws for itself. Some responses instead name the queue on the panel holding
/// the rows, which is the fallback.
fn queue_title(json: &Value) -> Option<String> {
    let mut headers = Vec::new();
    home::collect(json, "musicQueueHeaderRenderer", &mut headers);
    let named = headers
        .iter()
        .find_map(|header| home::runs_text(&header["subtitle"]["runs"]));
    if named.is_some() {
        return named;
    }

    let mut panels = Vec::new();
    home::collect(json, "playlistPanelRenderer", &mut panels);
    panels.iter().find_map(|panel| {
        let title = &panel["title"];
        // Three shapes for the same field, depending on whether YouTube had a
        // link to put in it.
        title
            .as_str()
            .map(str::to_string)
            .or_else(|| title["simpleText"].as_str().map(str::to_string))
            .or_else(|| home::runs_text(&title["runs"]))
    })
}

/// Whatever YouTube put where the comments should have been.
///
/// A comment section that will not load is not an error state at YouTube's end
/// -- it renders a sentence explaining itself, and that sentence is the most
/// accurate thing available to show the user.
fn notice(json: &Value) -> Option<String> {
    let mut sections = Vec::new();
    home::collect(json, "itemSectionRenderer", &mut sections);

    let section = sections
        .iter()
        .find(|section| section["sectionIdentifier"].as_str() == Some("comment-item-section"))?;

    let mut messages = Vec::new();
    home::collect(section, "messageRenderer", &mut messages);
    messages
        .iter()
        .find_map(|message| home::runs_text(&message["text"]["runs"]))
}

/// The queue rows, as tracks.
fn queue(json: &Value) -> Vec<Track> {
    let mut rows = Vec::new();
    home::collect(json, "playlistPanelVideoRenderer", &mut rows);

    let mut tracks: Vec<Track> = Vec::new();
    for row in rows {
        let Some(track) = parse_queue_row(row) else {
            continue;
        };
        // A radio can legitimately repeat a track further down, but the same id
        // twice in a row is the seed appearing in both the header and the list,
        // and a queue that lists what is playing twice reads as a bug.
        if tracks.iter().any(|held| held.id == track.id) {
            continue;
        }
        tracks.push(track);
        if tracks.len() >= MAX_QUEUE {
            break;
        }
    }
    tracks
}

fn parse_queue_row(row: &Value) -> Option<Track> {
    // "Artist • Album • 1.2M views", already joined for display. Split back
    // apart rather than trusted run by run, for the same reason the search
    // parser does it: a collaboration puts a separator inside the artist.
    let byline = row
        .pointer("/longBylineText/runs")
        .and_then(home::runs_text)
        .unwrap_or_default();
    let fields: Vec<&str> = byline
        .split('•')
        .map(str::trim)
        .filter(|field| !field.is_empty())
        .collect();

    Some(Track {
        id: row["videoId"].as_str()?.to_string(),
        title: row.pointer("/title/runs").and_then(home::runs_text)?,
        uploader: fields
            .first()
            .copied()
            .unwrap_or(UNKNOWN_ARTIST)
            .to_string(),
        // The queue states a length of its own; there is nothing to fall back
        // to here, and a row without one is a livestream.
        duration: row
            .pointer("/lengthText/runs")
            .and_then(home::runs_text)
            .as_deref()
            .and_then(parse_duration),
        // Only shown when the byline named one between the artist and whatever
        // trails it -- counted from the front, since the tail varies.
        album: (fields.len() >= 3)
            .then(|| fields[1].to_string())
            .filter(|album| !album.is_empty()),
        artist_ref: row
            .pointer("/longBylineText/runs")
            .and_then(home::artist_ref),
        // A radio queue is nobody's playlist, so there is no row to remove.
        playlist_item_id: None,
    })
}

/// The browse id of a named tab on the watch page.
fn tab_id(json: &Value, name: &str) -> Option<String> {
    let mut tabs = Vec::new();
    home::collect(json, "tabRenderer", &mut tabs);

    tabs.iter()
        .find(|tab| tab["title"].as_str() == Some(name))?
        .pointer("/endpoint/browseEndpoint/browseId")?
        .as_str()
        .map(str::to_string)
}

/// The token that buys the next page of the queue.
///
/// Scoped to the panel holding the rows, the way [`comment_token`] is scoped to
/// the comment section. A watch response carries continuations for several
/// things it could page -- the comment section, the related shelf -- and the
/// one that means "more queue" is identified by the panel it sits inside rather
/// than by being the first token found anywhere in the tree.
///
/// Two panels, because the first page and the pages after it are not the same
/// renderer: [`fetch`] gets a `playlistPanelRenderer`, and what
/// [`continue_queue`] gets back is a `playlistPanelContinuation` holding the
/// same rows. Both are searched here so that one parser serves both calls.
fn queue_token(json: &Value) -> Option<String> {
    let mut panels = Vec::new();
    for key in ["playlistPanelRenderer", "playlistPanelContinuation"] {
        home::collect(json, key, &mut panels);
    }
    panels.iter().find_map(|panel| panel_token(panel))
}

/// The continuation token inside one queue panel, however it is spelled.
///
/// Three spellings for one field, all of them live. A radio names its token
/// `nextRadioContinuationData`, a playlist names it `nextContinuationData`, and
/// the newer render tree sends a `continuationCommand` beside the rows instead
/// of either. All three are read: a queue that stops at the end of its first
/// page is the exact failure this exists to prevent, and which spelling arrived
/// is not something the caller should have to care about.
fn panel_token(panel: &Value) -> Option<String> {
    for key in ["nextRadioContinuationData", "nextContinuationData"] {
        let mut found = Vec::new();
        home::collect(panel, key, &mut found);
        if let Some(token) = found.iter().find_map(|data| data["continuation"].as_str()) {
            return Some(token.to_string());
        }
    }

    let mut commands = Vec::new();
    home::collect(panel, "continuationCommand", &mut commands);
    commands
        .iter()
        .find_map(|command| command["token"].as_str())
        .map(str::to_string)
}

/// The continuation token standing where the comments will go.
///
/// Found by walking rather than by path: the watch page moves its sections
/// around, and the token is identified by the section it belongs to rather than
/// by where that section happens to sit.
fn comment_token(json: &Value) -> Option<String> {
    let mut sections = Vec::new();
    home::collect(json, "itemSectionRenderer", &mut sections);

    sections
        .iter()
        .find(|section| section["sectionIdentifier"].as_str() == Some("comment-item-section"))
        .and_then(|section| {
            let mut tokens = Vec::new();
            home::collect(section, "continuationCommand", &mut tokens);
            tokens
                .first()
                .and_then(|command| command["token"].as_str())
                .map(str::to_string)
        })
}

/// "8,407 Comments", as the header states it.
fn comment_count(json: &Value) -> Option<String> {
    let mut headers = Vec::new();
    home::collect(json, "commentsHeaderRenderer", &mut headers);
    headers
        .first()
        .and_then(|header| home::runs_text(&header["countText"]["runs"]))
}

/// Pulls the comments out of a continuation response.
///
/// Two shapes, both live. YouTube now sends the text in a flat entity batch
/// beside the render tree, but still sends the older nested renderer for some
/// responses -- so the entities are read first and the renderers are the
/// fallback rather than either being trusted alone.
fn parse_comments(json: &Value) -> Vec<Comment> {
    let mut payloads = Vec::new();
    home::collect(json, "commentEntityPayload", &mut payloads);

    let mut comments: Vec<Comment> = payloads
        .iter()
        .filter_map(|payload| parse_entity(payload))
        .take(MAX_COMMENTS)
        .collect();

    if comments.is_empty() {
        let mut renderers = Vec::new();
        home::collect(json, "commentRenderer", &mut renderers);
        comments = renderers
            .iter()
            .filter_map(|renderer| parse_renderer(renderer))
            .take(MAX_COMMENTS)
            .collect();
    }
    comments
}

/// One comment from the entity batch.
fn parse_entity(payload: &Value) -> Option<Comment> {
    let properties = &payload["properties"];
    // Anything deeper is a reply, which belongs under a thread we are not
    // drawing -- showing them flat would put answers above their questions.
    if properties["replyLevel"].as_u64().unwrap_or(0) != 0 {
        return None;
    }

    let toolbar = &payload["toolbar"];
    Some(Comment {
        author: payload
            .pointer("/author/displayName")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        published: properties["publishedTime"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        text: properties
            .pointer("/content/content")
            .and_then(Value::as_str)?
            .to_string(),
        // Which of the two fields carries the count depends on whether *we*
        // liked it, and one of them is empty rather than absent.
        likes: ["likeCountNotliked", "likeCountLiked"]
            .iter()
            .find_map(|field| toolbar[field].as_str().filter(|count| !count.is_empty()))
            .unwrap_or_default()
            .to_string(),
        replies: toolbar["replyCount"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
    })
}

/// One comment from the older nested renderer.
fn parse_renderer(renderer: &Value) -> Option<Comment> {
    Some(Comment {
        author: renderer
            .pointer("/authorText/simpleText")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        published: renderer
            .pointer("/publishedTimeText/runs")
            .and_then(home::runs_text)
            .unwrap_or_default(),
        text: renderer
            .pointer("/contentText/runs")
            .and_then(home::runs_text)?,
        likes: renderer
            .pointer("/voteCount/simpleText")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        replies: renderer["replyCount"]
            .as_u64()
            .filter(|count| *count > 0)
            .map(|count| count.to_string())
            .unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// One queue row, as the watch response sends them.
    fn row(id: &str, title: &str, byline: &str, length: &str) -> Value {
        let runs = |text: &str| serde_json::json!({ "runs": [ { "text": text } ] });
        serde_json::json!({
            "playlistPanelVideoRenderer": {
                "videoId": id,
                "title": runs(title),
                "longBylineText": runs(byline),
                "lengthText": runs(length),
            }
        })
    }

    #[test]
    fn reads_a_queue_row() {
        let json = serde_json::json!({ "contents": [ row(
            "aBcDeFgHiJk",
            "Let It Happen",
            "Tame Impala • Currents • 2015",
            "7:48",
        ) ] });

        let queue = queue(&json);
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].id, "aBcDeFgHiJk");
        assert_eq!(queue[0].title, "Let It Happen");
        assert_eq!(queue[0].uploader, "Tame Impala");
        assert_eq!(queue[0].album.as_deref(), Some("Currents"));
        assert_eq!(queue[0].duration, Some(Duration::from_secs(468)));
    }

    #[test]
    fn a_queue_row_keeps_its_linked_artist() {
        let mut json = row(
            "x",
            "Let It Happen",
            "Tame Impala • Currents • 2015",
            "7:48",
        );
        json["playlistPanelVideoRenderer"]["longBylineText"]["runs"] = serde_json::json!([
            {
                "text": "Tame Impala",
                "navigationEndpoint": { "browseEndpoint": {
                    "browseId": "UCGz-artist",
                    "browseEndpointContextSupportedConfigs": {
                        "browseEndpointContextMusicConfig": {
                            "pageType": "MUSIC_PAGE_TYPE_ARTIST"
                        }
                    }
                } }
            },
            { "text": " • Currents • 2015" }
        ]);

        let track = queue(&json).remove(0);
        assert_eq!(track.artist_ref.unwrap().endpoint.browse_id, "UCGz-artist");
    }

    #[test]
    fn a_row_with_no_album_keeps_its_artist() {
        let json = row("x", "Da Funk", "Daft Punk • 4.2M views", "5:35");
        let queue = queue(&json);
        assert_eq!(queue[0].uploader, "Daft Punk");
        assert_eq!(queue[0].album, None);
    }

    #[test]
    fn the_seed_is_not_listed_twice() {
        // The seed appears in the queue and again in the header of some
        // responses; a queue that lists what is playing twice reads as a bug.
        let json = serde_json::json!({ "contents": [
            row("a", "Let It Happen", "Tame Impala", "7:48"),
            row("a", "Let It Happen", "Tame Impala", "7:48"),
            row("b", "The Moment", "Tame Impala", "4:16"),
        ] });

        let queue = queue(&json);
        assert_eq!(queue.len(), 2);
        assert_eq!(queue[1].id, "b");
    }

    /// A radio spells its token one way and a playlist another, and the newer
    /// render tree spells it a third. Whichever arrives, the queue has to be
    /// able to ask for more -- that is the whole of the endless queue.
    #[test]
    fn reads_the_queue_token_in_every_spelling() {
        let panel = |continuations: Value| serde_json::json!({ "playlistPanelRenderer": { "continuations": continuations } });

        let radio = panel(serde_json::json!([
            { "nextRadioContinuationData": { "continuation": "RADIO_TOKEN" } }
        ]));
        assert_eq!(queue_token(&radio).as_deref(), Some("RADIO_TOKEN"));

        let playlist = panel(serde_json::json!([
            { "nextContinuationData": { "continuation": "LIST_TOKEN" } }
        ]));
        assert_eq!(queue_token(&playlist).as_deref(), Some("LIST_TOKEN"));

        let newer = serde_json::json!({ "playlistPanelRenderer": { "contents": [
            { "continuationItemRenderer": { "continuationEndpoint": {
                "continuationCommand": { "token": "COMMAND_TOKEN" }
            } } }
        ] } });
        assert_eq!(queue_token(&newer).as_deref(), Some("COMMAND_TOKEN"));
    }

    /// The page after the first is a different renderer holding the same rows.
    /// One parser has to serve both, or the queue pages exactly once.
    #[test]
    fn reads_a_continuation_page() {
        let json = serde_json::json!({ "continuationContents": {
            "playlistPanelContinuation": {
                "contents": [ row("b", "The Moment", "Tame Impala", "4:16") ],
                "continuations": [
                    { "nextRadioContinuationData": { "continuation": "NEXT_PAGE" } }
                ],
            }
        } });

        let tracks = queue(&json);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].id, "b");
        assert_eq!(queue_token(&json).as_deref(), Some("NEXT_PAGE"));
    }

    /// A token that belongs to something else on the page is not a queue token.
    /// The comment section carries one on every watch response, and redeeming
    /// it as a queue would page the comments into "Up next".
    #[test]
    fn ignores_a_token_outside_the_queue_panel() {
        let json = serde_json::json!({
            "contents": [ row("a", "Let It Happen", "Tame Impala", "7:48") ],
            "itemSectionRenderer": {
                "sectionIdentifier": "comment-item-section",
                "continuations": [
                    { "nextContinuationData": { "continuation": "COMMENTS" } }
                ],
            },
        });

        assert_eq!(queue_token(&json), None);
    }

    /// The end of a finite queue: rows, and nothing to ask for after them.
    #[test]
    fn a_queue_with_no_next_page_has_no_token() {
        let json = serde_json::json!({ "playlistPanelRenderer": {
            "contents": [ row("a", "Let It Happen", "Tame Impala", "7:48") ]
        } });

        assert_eq!(queue_token(&json), None);
    }

    #[test]
    fn finds_the_tabs_by_name() {
        let json = serde_json::json!({ "tabs": [
            { "tabRenderer": { "title": "Up next" } },
            { "tabRenderer": {
                "title": "Lyrics",
                "endpoint": { "browseEndpoint": { "browseId": "MPLYt_lyrics" } }
            } },
            { "tabRenderer": {
                "title": "Related",
                "endpoint": { "browseEndpoint": { "browseId": "MPTRt_related" } }
            } },
        ] });

        assert_eq!(tab_id(&json, "Lyrics").as_deref(), Some("MPLYt_lyrics"));
        assert_eq!(tab_id(&json, "Related").as_deref(), Some("MPTRt_related"));
        // "Up next" carries no browse id: it is already in the response.
        assert_eq!(tab_id(&json, "Up next"), None);
        assert_eq!(tab_id(&json, "Comments"), None);
    }

    #[test]
    fn names_the_queue_from_its_header() {
        // The header's *title* is the fixed words "Playing from"; the name of
        // the queue is its subtitle. Taking the title would label every queue
        // in the program "Playing from".
        let json = serde_json::json!({ "musicQueueHeaderRenderer": {
            "title": { "runs": [ { "text": "Playing from" } ] },
            "subtitle": { "runs": [ { "text": "Let It Happen Mix" } ] },
        } });
        assert_eq!(queue_title(&json).as_deref(), Some("Let It Happen Mix"));
    }

    #[test]
    fn falls_back_to_the_name_on_the_panel() {
        let json = serde_json::json!({
            "playlistPanelRenderer": { "title": "Currents" }
        });
        assert_eq!(queue_title(&json).as_deref(), Some("Currents"));

        let nested = serde_json::json!({
            "playlistPanelRenderer": { "title": { "runs": [ { "text": "Discovery" } ] } }
        });
        assert_eq!(queue_title(&nested).as_deref(), Some("Discovery"));
    }

    /// A response carrying lyrics line by line, with `start` written however
    /// the caller asks for it.
    ///
    /// The words are invented: nothing in the parsing depends on what a line
    /// says, and a fixture of somebody's real lyric would be a copy of it
    /// sitting in the repository for no benefit to the test.
    fn timed_response(starts: Vec<Value>) -> Value {
        let lines: Vec<Value> = starts
            .into_iter()
            .enumerate()
            .map(|(n, start)| {
                serde_json::json!({
                    "lyricLine": format!("sung line {n}"),
                    "cueRange": { "startTimeMilliseconds": start },
                })
            })
            .collect();

        // Nested well below the top level, as the real response nests it inside
        // the element model the web client hands its lyrics component: the
        // parser walks for the key rather than following a path, and a fixture
        // that put it at the root would not exercise that.
        serde_json::json!({ "contents": { "elementRenderer": { "model": {
            "timedLyricsModel": { "lyricsData": {
                "timedLyricsData": lines,
                "sourceMessage": "Source: Musixmatch",
            } }
        } } } })
    }

    #[test]
    fn reads_lyrics_line_by_line_with_the_moment_each_is_sung() {
        let json = timed_response(vec![
            serde_json::json!(0),
            serde_json::json!(4_500),
            serde_json::json!(9_250),
        ]);

        let lyrics = timed_lyrics(&json).expect("a timed response should parse");
        assert_eq!(lyrics.timed.len(), 3);
        assert_eq!(lyrics.timed[1].text, "sung line 1");
        assert_eq!(lyrics.timed[1].start, Duration::from_millis(4_500));
        assert_eq!(lyrics.source.as_deref(), Some("Source: Musixmatch"));
        // The block is kept alongside the lines so a caller that wants the
        // whole thing does not have to decide how to join them.
        assert_eq!(lyrics.text, "sung line 0\nsung line 1\nsung line 2");
    }

    #[test]
    fn a_time_is_read_whether_it_arrives_as_a_number_or_a_string() {
        // Google's JSON encoding of protobuf writes 64-bit fields as strings,
        // so the same field can arrive either way from the same endpoint.
        assert_eq!(millis(&serde_json::json!(4_500)), Some(4_500));
        assert_eq!(millis(&serde_json::json!("4500")), Some(4_500));
        assert_eq!(millis(&serde_json::json!("not a number")), None);
        assert_eq!(millis(&Value::Null), None);

        let json = timed_response(vec![serde_json::json!("0"), serde_json::json!("7000")]);
        let lyrics = timed_lyrics(&json).expect("stringified times should parse");
        assert_eq!(lyrics.timed[1].start, Duration::from_secs(7));
    }

    #[test]
    fn a_line_with_no_time_is_dropped_rather_than_placed_at_the_start() {
        // Placed at zero it would mark itself as the line being sung for the
        // whole of the intro, which is worse than it not being there.
        let json = timed_response(vec![
            serde_json::json!(0),
            Value::Null,
            serde_json::json!(9_000),
        ]);

        let lyrics = timed_lyrics(&json).expect("the timed lines should still parse");
        assert_eq!(lyrics.timed.len(), 2);
        assert_eq!(lyrics.timed[1].start, Duration::from_secs(9));
    }

    #[test]
    fn a_response_with_no_timings_falls_back_to_the_block() {
        let json = serde_json::json!({ "musicDescriptionShelfRenderer": {
            "description": { "runs": [ { "text": "sung line 0\nsung line 1" } ] },
            "footer": { "runs": [ { "text": "Source: Musixmatch" } ] },
        } });

        assert!(
            timed_lyrics(&json).is_none(),
            "there are no timings in the older shape"
        );
        let lyrics = block_lyrics(&json).expect("the block should still parse");
        assert_eq!(lyrics.text, "sung line 0\nsung line 1");
        assert_eq!(lyrics.source.as_deref(), Some("Source: Musixmatch"));
        assert!(
            lyrics.timed.is_empty(),
            "the block shape carries no timings to highlight from"
        );

        // An empty array is not timings either, and must not shadow the block.
        let empty = timed_response(Vec::new());
        assert!(timed_lyrics(&empty).is_none());
    }

    #[test]
    fn the_line_being_sung_is_the_last_one_to_have_started() {
        let lyrics = timed_lyrics(&timed_response(vec![
            serde_json::json!(1_000),
            serde_json::json!(5_000),
            serde_json::json!(9_000),
        ]))
        .expect("a timed response should parse");

        // Before the first line there is nothing to mark: an intro should not
        // highlight words nobody is singing yet.
        assert_eq!(lyrics.singing(Duration::ZERO), None);
        assert_eq!(lyrics.singing(Duration::from_millis(999)), None);
        // A line is sung from the instant it starts.
        assert_eq!(lyrics.singing(Duration::from_secs(1)), Some(0));
        assert_eq!(lyrics.singing(Duration::from_millis(4_999)), Some(0));
        assert_eq!(lyrics.singing(Duration::from_secs(5)), Some(1));
        // Past the last line it stays marked rather than clearing: through an
        // outro the line just sung is what a reader is still looking at.
        assert_eq!(lyrics.singing(Duration::from_secs(600)), Some(2));
    }

    #[test]
    fn a_track_with_no_timings_never_marks_a_line() {
        let lyrics = Lyrics {
            text: "sung line 0".to_string(),
            source: None,
            timed: Vec::new(),
        };
        assert_eq!(lyrics.singing(Duration::ZERO), None);
        assert_eq!(lyrics.singing(Duration::from_secs(90)), None);
    }

    #[test]
    fn an_empty_comment_section_explains_itself_in_youtubes_own_words() {
        // What comes back when the network enforces Restricted Mode, which is
        // not a fact about the track -- and a guess of "comments are turned
        // off" would send the user looking at the wrong thing entirely.
        let json = serde_json::json!({ "contents": [ { "itemSectionRenderer": {
            "sectionIdentifier": "comment-item-section",
            "contents": [ { "messageRenderer": { "text": { "runs": [
                { "text": "Restricted Mode has hidden comments for this video." }
            ] } } } ],
        } } ] });

        assert_eq!(comment_token(&json), None);
        assert_eq!(
            notice(&json).as_deref(),
            Some("Restricted Mode has hidden comments for this video.")
        );
        // A section that simply has no message leaves the caller to say so.
        assert_eq!(notice(&serde_json::json!({ "contents": [] })), None);
    }

    #[test]
    fn reads_comments_from_the_entity_batch() {
        let json = serde_json::json!({
            "frameworkUpdates": { "entityBatchUpdate": { "mutations": [
                { "payload": { "commentEntityPayload": {
                    "properties": {
                        "content": { "content": "Don't search for best part. Just let it happen" },
                        "publishedTime": "4 months ago",
                        "replyLevel": 0,
                    },
                    "author": { "displayName": "@SpeartonFromOrder" },
                    "toolbar": { "likeCountNotliked": "10K", "replyCount": "67" },
                } } },
                // A reply: it belongs under a thread we do not draw, and flat
                // it would sit above the comment it answers.
                { "payload": { "commentEntityPayload": {
                    "properties": {
                        "content": { "content": "so true" },
                        "publishedTime": "3 months ago",
                        "replyLevel": 1,
                    },
                    "author": { "displayName": "@someone" },
                    "toolbar": {},
                } } },
            ] } }
        });

        let comments = parse_comments(&json);
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].author, "@SpeartonFromOrder");
        assert_eq!(comments[0].likes, "10K");
        assert_eq!(comments[0].replies, "67");
        assert_eq!(comments[0].published, "4 months ago");
    }

    #[test]
    fn falls_back_to_the_older_comment_renderer() {
        let json = serde_json::json!({ "contents": [ { "commentRenderer": {
            "authorText": { "simpleText": "@LOO_DOO" },
            "contentText": { "runs": [ { "text": "waiting six whole minutes" } ] },
            "publishedTimeText": { "runs": [ { "text": "2 years ago" } ] },
            "voteCount": { "simpleText": "18K" },
            "replyCount": 104,
        } } ] });

        let comments = parse_comments(&json);
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].author, "@LOO_DOO");
        assert_eq!(comments[0].text, "waiting six whole minutes");
        assert_eq!(comments[0].replies, "104");
    }

    #[test]
    fn finds_the_comment_continuation() {
        let json = serde_json::json!({ "contents": [
            { "itemSectionRenderer": { "sectionIdentifier": "related-items" } },
            { "itemSectionRenderer": {
                "sectionIdentifier": "comment-item-section",
                "contents": [ { "continuationItemRenderer": { "continuationEndpoint": {
                    "continuationCommand": { "token": "Eg0SC2FCY0RlRmdI" }
                } } } ],
            } },
        ] });

        assert_eq!(comment_token(&json).as_deref(), Some("Eg0SC2FCY0RlRmdI"));
        // Comments turned off: the section is simply absent.
        assert_eq!(comment_token(&serde_json::json!({ "contents": [] })), None);
    }

    #[test]
    fn an_unrecognised_response_yields_nothing() {
        // Which is how every caller here knows to show an empty panel rather
        // than a wrong one.
        assert!(queue(&serde_json::json!({})).is_empty());
        assert!(parse_comments(&serde_json::json!({ "contents": 5 })).is_empty());
        assert_eq!(queue_title(&serde_json::json!({})), None);
    }

    /// Confirms the player page still parses against the live endpoints.
    ///
    /// Ignored by default for the same reason the resolver's live test is: it
    /// depends on a third party, and every failure here degrades to an empty
    /// panel rather than a broken build.
    ///
    /// `cargo test --release live_player_page -- --ignored --nocapture`
    #[test]
    #[ignore = "hits the live YouTube APIs"]
    fn live_player_page() {
        let id = std::env::var("MTUI_VIDEO_ID").unwrap_or_else(|_| "H4tG8jHOxJk".into());
        let http = Http::new().expect("client should build");

        // A network that forces Restricted Mode answers with no queue at all
        // for a flagged track, which looks exactly like a parser that has
        // stopped working. Naming the way out here saves the next person the
        // same afternoon: `nslookup www.youtube.com`, and 216.239.38.x means
        // the track is being withheld rather than the response misread.
        let watch = fetch(&http, &id).unwrap_or_else(|why| {
            panic!("no queue came back for {id}: {why:#}\nif this network forces Restricted Mode, try MTUI_VIDEO_ID with an unflagged track")
        });
        println!(
            "queue: {} ({} tracks)",
            watch.queue_title,
            watch.queue.len()
        );
        for track in watch.queue.iter().take(5) {
            println!("  {:<40.38} {:>8}", track.label(), track.duration_str());
        }
        assert!(!watch.queue.is_empty());

        // The endless queue, walked twice. Two pages is what distinguishes a
        // token that works from one that is merely present: the first page
        // proves the token on the watch response parses, and the second proves
        // the token *inside a continuation* does -- which is the one the queue
        // depends on for every page after this, and the one that arrives under
        // a different renderer.
        match watch.continuation.as_deref() {
            Some(token) => {
                let page = continue_queue(&http, token).expect("no further page came back");
                println!("+{} tracks from the continuation", page.tracks.len());
                for track in page.tracks.iter().take(3) {
                    println!("  {:<40.38} {:>8}", track.label(), track.duration_str());
                }
                assert!(!page.tracks.is_empty());

                match page.continuation.as_deref() {
                    Some(next) => {
                        let third = continue_queue(&http, next).expect("no third page came back");
                        println!("+{} tracks from the page after it", third.tracks.len());
                        assert!(!third.tracks.is_empty());
                    }
                    // A radio should always offer another page. A playlist
                    // genuinely ends, so this is reported rather than asserted.
                    None => println!("the second page offered no continuation"),
                }
            }
            None => println!("no queue continuation -- the queue is finite"),
        }

        match watch.lyrics_id.as_deref().map(|id| lyrics(&http, id)) {
            Some(Ok(l)) => println!("lyrics: {} chars, {:?}", l.text.len(), l.source),
            Some(Err(e)) => println!("lyrics failed: {e:#}"),
            None => println!("no lyrics tab"),
        }
        match watch.related_id.as_deref().map(|id| related(&http, id)) {
            Some(Ok(shelves)) => println!("related: {} shelves", shelves.len()),
            Some(Err(e)) => println!("related failed: {e:#}"),
            None => println!("no related tab"),
        }
        match comments(&http, &id) {
            Ok(c) => println!("comments: {} / {} kept", c.total, c.items.len()),
            Err(e) => println!("comments failed: {e:#}"),
        }
    }

    /// The fallback, against the live endpoint and this machine's own journal.
    ///
    /// Two rotations, because the point of the rotation is that a second
    /// attempt builds a *different* station -- a fallback that rescues the
    /// queue with the same tracks every time has not rescued anything.
    ///
    /// Reports rather than asserts when there is no history to build from: a
    /// machine that has not played anything yet is the cold-start case this is
    /// documented to decline, not a failure.
    ///
    /// `cargo test --release live_seeded_station -- --ignored --nocapture`
    #[test]
    #[ignore = "hits the live YouTube APIs"]
    fn live_seeded_station() {
        let http = Http::new().expect("client should build");
        let journal = Journal::load();

        let mut first = Vec::new();
        for rotation in 0..2 {
            match seeded_page(&http, &journal, rotation) {
                Ok(page) => {
                    println!(
                        "rotation {rotation}: {:?} ({} tracks)",
                        page.title,
                        page.tracks.len()
                    );
                    for track in page.tracks.iter().take(4) {
                        println!("  {:<40.38} {:>8}", track.label(), track.duration_str());
                    }
                    assert!(!page.tracks.is_empty());
                    // The station has to be pageable from here, or the queue is
                    // rescued once and then stops again a page later.
                    assert!(
                        page.continuation.is_some(),
                        "a seeded station must hand back a way to continue it"
                    );

                    let ids: Vec<String> =
                        page.tracks.iter().map(|track| track.id.clone()).collect();
                    if rotation == 0 {
                        first = ids;
                    } else {
                        let shared = ids.iter().filter(|id| first.contains(id)).count();
                        println!("  {shared} of {} shared with rotation 0", ids.len());
                        assert!(
                            shared < ids.len(),
                            "a second rotation that rebuilds the same station rescues nothing"
                        );
                    }
                }
                Err(why) => println!("rotation {rotation}: no station -- {why:#}"),
            }
        }
    }
}
