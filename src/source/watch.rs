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
//! Same bargain as [`crate::source::home`]: these are internal endpoints, so
//! everything here degrades to an empty panel rather than an error. A tab that
//! comes back with nothing says so and the music keeps playing.
//!
//! Comments are the exception to "same corpus": YouTube Music does not serve
//! them, so they come from youtube.com proper, as its web client asks for them
//! -- one call for a continuation token and a second to redeem it.

use anyhow::{Result, bail};
use serde_json::Value;

use super::auth::Http;
use super::home::{self, Shelf};
use super::innertube::parse_duration;
use super::{Track, UNKNOWN_ARTIST};

/// youtube.com's own watch endpoint. Not the music one: comments live here.
const YOUTUBE_NEXT_URL: &str = "https://www.youtube.com/youtubei/v1/next";

/// The plain web client, which is the one served a comment section. These two
/// travel together -- see [`crate::source::innertube`] on mismatched identities.
const WEB_CLIENT_NAME: &str = "WEB";
const WEB_CLIENT_VERSION: &str = "2.20241127.01.00";

/// Ceiling on the queue kept in memory. YouTube's radio continues indefinitely
/// through continuations; one page is around twenty-five tracks and is already
/// more than a session plays through.
const MAX_QUEUE: usize = 60;

/// Comments held for one track. The first page of the API returns twenty; this
/// is a bound on what a continuation could add rather than a target.
const MAX_COMMENTS: usize = 50;

/// Lyrics longer than this are not lyrics -- they are a description field that
/// happened to land in the same renderer.
const MAX_LYRICS: usize = 8_000;

/// The queue a track plays inside, and where to find the rest of its page.
///
/// The two ids are handed back rather than followed, because following them is
/// a round trip each and neither panel is on screen yet.
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
}

/// Lyrics, and whoever YouTube licensed them from.
#[derive(Debug, Clone)]
pub struct Lyrics {
    pub text: String,
    /// "Source: Musixmatch". Shown because the tab is otherwise a wall of text
    /// with no indication it came from anywhere.
    pub source: Option<String>,
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
    };

    // A response with no queue at all is one whose shape we no longer
    // recognise; the caller shows an empty panel rather than a wrong one.
    if watch.queue.is_empty() {
        bail!("YouTube Music returned no queue for this track");
    }
    Ok(watch)
}

/// The lyrics behind the browse id [`fetch`] handed back.
pub fn lyrics(http: &Http, browse_id: &str) -> Result<Lyrics> {
    let json = home::browse(http, None, browse_id)?;

    let mut shelves = Vec::new();
    home::collect(&json, "musicDescriptionShelfRenderer", &mut shelves);

    let shelf = shelves
        .first()
        .ok_or_else(|| anyhow::anyhow!("no lyrics are published for this track"))?;

    let mut text = shelf
        .pointer("/description/runs")
        .and_then(home::runs_text)
        .ok_or_else(|| anyhow::anyhow!("no lyrics are published for this track"))?;
    text.truncate(
        text.char_indices()
            .nth(MAX_LYRICS)
            .map_or(text.len(), |(at, _)| at),
    );

    Ok(Lyrics {
        text,
        source: shelf.pointer("/footer/runs").and_then(home::runs_text),
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
        uploader: fields.first().copied().unwrap_or(UNKNOWN_ARTIST).to_string(),
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

        let watch = fetch(&http, &id).expect("no queue came back");
        println!("queue: {} ({} tracks)", watch.queue_title, watch.queue.len());
        for track in watch.queue.iter().take(5) {
            println!("  {:<40.38} {:>8}", track.label(), track.duration_str());
        }
        assert!(!watch.queue.is_empty());

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
}

