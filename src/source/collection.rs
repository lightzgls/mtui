//! Authenticated album and playlist pages, scoped to their own track shelves.

use super::{BrowseEndpoint, Track, home, http::Http};
use crate::config::Cookies;
use anyhow::{Result, bail};
use serde_json::Value;

const BROWSE_URL: &str = "https://music.youtube.com/youtubei/v1/browse";
const MAX_TRACKS: usize = 200;

#[derive(Debug, Clone, Default)]
pub struct Details {
    pub title: String,
    pub subtitle: String,
    pub description: String,
    pub art: Option<String>,
    pub truncated: bool,
}

pub struct Page {
    pub details: Details,
    pub tracks: Vec<Track>,
}

#[cfg(test)]
pub(crate) fn saved_cards(http: &Http, cookies: &Cookies) -> Result<Vec<home::Card>> {
    let json = home::browse(http, Some(cookies), "FEmusic_liked_playlists")?;
    let mut rows = Vec::new();
    home::collect(&json, "musicTwoRowItemRenderer", &mut rows);
    Ok(rows.into_iter().filter_map(|row|
        home::parse_card(&serde_json::json!({"musicTwoRowItemRenderer": row})))
        .filter(|card| matches!(&card.target, home::Target::Open { endpoint } if endpoint.browse_id.starts_with("VL")))
        .collect())
}

pub fn fetch(http: &Http, endpoint: &BrowseEndpoint, title: &str) -> Result<Page> {
    let cookies = Cookies::available().ok().flatten();
    let json = home::browse_endpoint(http, cookies.as_ref(), endpoint)?;
    let mut page = parse(&json, title)?;
    let mut token = page_continuation(&json);
    let mut seen = std::collections::HashSet::new();
    for _ in 0..8 {
        let Some(next) = token.take().filter(|next| seen.insert(next.clone())) else {
            break;
        };
        if page.tracks.len() >= MAX_TRACKS {
            page.details.truncated = true;
            break;
        }
        let json = match home::post(
            http,
            BROWSE_URL,
            cookies.as_ref(),
            serde_json::json!({"continuation":next}),
        ) {
            Ok(json) => json,
            Err(_) => {
                page.details.truncated = true;
                break;
            }
        };
        let Some(contents) = track_contents(&json) else {
            page.details.truncated = true;
            break;
        };
        let more = tracks(contents);
        token = page_continuation(&json);
        let remaining = MAX_TRACKS - page.tracks.len();
        page.details.truncated |= more.len() > remaining;
        page.tracks.extend(more.into_iter().take(remaining));
    }
    page.details.truncated |= token.is_some();
    Ok(page)
}

fn find<'a>(json: &'a Value, key: &str) -> Option<&'a Value> {
    match json {
        Value::Object(object) => object
            .get(key)
            .or_else(|| object.values().find_map(|value| find(value, key))),
        Value::Array(items) => items.iter().find_map(|value| find(value, key)),
        _ => None,
    }
}

fn track_contents(json: &Value) -> Option<&Value> {
    for key in [
        "musicPlaylistShelfRenderer",
        "musicPlaylistShelfContinuation",
        "musicShelfContinuation",
    ] {
        if let Some(shelf) = find(json, key) {
            return shelf.get("contents");
        }
    }
    if let Some(action) = find(json, "appendContinuationItemsAction") {
        return action.get("continuationItems");
    }
    // Albums use a musicShelfRenderer beneath their detail header.
    find(json, "musicShelfRenderer").and_then(|shelf| shelf.get("contents"))
}

fn tracks(contents: &Value) -> Vec<Track> {
    contents
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("musicResponsiveListItemRenderer"))
        .filter(|row| {
            row["musicItemRendererDisplayPolicy"].as_str()
                != Some("MUSIC_ITEM_RENDERER_DISPLAY_POLICY_GREY_OUT")
        })
        .filter_map(home::parse_row)
        .take(MAX_TRACKS + 1)
        .collect()
}

fn continuation(contents: &Value) -> Option<String> {
    let renderer = contents
        .as_array()?
        .last()?
        .get("continuationItemRenderer")?;
    find(renderer, "continuationCommand")?
        .get("token")?
        .as_str()
        .map(str::to_owned)
}

fn page_continuation(json: &Value) -> Option<String> {
    if let Some(token) = track_contents(json).and_then(continuation) { return Some(token); }
    // Older responses put the token beside contents, rather than in a final
    // continuationItemRenderer. Scope it to the track shelf, not suggestions.
    ["musicPlaylistShelfRenderer", "musicPlaylistShelfContinuation", "musicShelfContinuation", "musicShelfRenderer"]
        .into_iter().filter_map(|key| find(json, key))
        .find_map(|shelf| shelf.get("continuations")?.as_array()?.iter().find_map(|item|
            item.pointer("/nextContinuationData/continuation")
                .or_else(|| item.pointer("/reloadContinuationData/continuation"))?
                .as_str().map(str::to_owned)))
}

fn text(value: &Value) -> String {
    value
        .get("runs")
        .and_then(home::runs_text)
        .or_else(|| {
            value
                .get("simpleText")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default()
}

fn parse(json: &Value, fallback: &str) -> Result<Page> {
    if json.get("error").is_some() {
        bail!("YouTube Music could not open this collection. Check your sign-in and try again.");
    }
    let header = find(json, "musicResponsiveHeaderRenderer")
        .or_else(|| find(json, "musicDetailHeaderRenderer"));
    let Some(contents) = track_contents(json) else {
        bail!("This collection is unavailable. It may be private, removed, or require sign-in.");
    };
    let header = header.unwrap_or(&Value::Null);
    let title = text(&header["title"]);
    let subtitle = [text(&header["subtitle"]), text(&header["secondSubtitle"])]
        .into_iter()
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join(" • ");
    let description = find(&header["description"], "description")
        .map(text)
        .unwrap_or_else(|| text(&header["description"]));
    let mut tracks = tracks(contents);
    let truncated = tracks.len() > MAX_TRACKS;
    tracks.truncate(MAX_TRACKS);
    Ok(Page {
        details: Details {
            title: if title.is_empty() {
                fallback.to_owned()
            } else {
                title
            },
            subtitle,
            description: description.chars().take(600).collect(),
            art: home::art_url(header.get("thumbnail")),
            truncated,
        },
        tracks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn row(id: &str) -> Value {
        json!({"musicResponsiveListItemRenderer":{
        "playlistItemData":{"videoId":id},"flexColumns":[
            {"musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[{"text":"Track"}]}}},
            {"musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[{"text":"Artist"}]}}}
        ]}})
    }
    #[test]
    fn private_owned_page_keeps_header_and_ignores_suggestions() {
        let page = json!({"contents":{"musicEditablePlaylistDetailHeaderRenderer":{"header":{
            "musicResponsiveHeaderRenderer":{"title":{"runs":[{"text":"My playlist"}]},"subtitle":{"runs":[{"text":"Private • Me"}]}}
        }},"musicPlaylistShelfRenderer":{"contents":[row("real")]},
        "musicShelfRenderer":{"contents":[row("suggestion")]}}});
        let parsed = parse(&page, "Fallback").unwrap();
        assert_eq!(parsed.details.title, "My playlist");
        assert_eq!(parsed.tracks.len(), 1);
        assert_eq!(parsed.tracks[0].id, "real");
    }
    #[test]
    fn empty_collection_is_a_page_but_unavailable_collection_is_an_error() {
        assert!(
            parse(
                &json!({"musicPlaylistShelfRenderer":{"contents":[]}}),
                "Empty"
            )
            .unwrap()
            .tracks
            .is_empty()
        );
        assert!(parse(&json!({"contents":{}}), "Gone").is_err());
    }
    #[test]
    fn continuation_keeps_order_and_repeated_tracks() {
        let contents = json!([row("same"),row("same"),{"continuationItemRenderer":{"continuationEndpoint":{
            "commandExecutorCommand":{"commands":[{"continuationCommand":{"token":"next"}}]}
        }}}]);
        assert_eq!(tracks(&contents).len(), 2);
        assert_eq!(continuation(&contents).as_deref(), Some("next"));
        let json = json!({"onResponseReceivedActions":[{"appendContinuationItemsAction":{"continuationItems":contents}}]});
        assert_eq!(tracks(track_contents(&json).unwrap()).len(), 2);
    }

    #[test]
    fn legacy_continuation_is_read_from_the_track_shelf() {
        let json = json!({"musicPlaylistShelfRenderer":{"contents":[row("first")],
            "continuations":[{"nextContinuationData":{"continuation":"tracks-next"}}]},
            "musicShelfRenderer":{"contents":[row("suggested")],
            "continuations":[{"nextContinuationData":{"continuation":"suggestions-next"}}]}});
        assert_eq!(page_continuation(&json).as_deref(), Some("tracks-next"));
    }

    #[test]
    #[ignore = "reads saved playlists from the live signed-in account; never writes history"]
    fn saved_playlists_against_the_live_api() {
        let cookies = Cookies::available()
            .expect("saved session should load")
            .expect("sign in before this check");
        let http = Http::new().unwrap();
        let json = home::browse(&http, Some(&cookies), "FEmusic_liked_playlists").unwrap();
        let mut renderers = Vec::new();
        home::collect(&json, "musicTwoRowItemRenderer", &mut renderers);
        let cards: Vec<_> = renderers
            .iter()
            .filter_map(|row| home::parse_card(&serde_json::json!({"musicTwoRowItemRenderer":row})))
            .collect();
        let endpoints: Vec<_> = cards
            .iter()
            .filter_map(|card| match &card.target {
                home::Target::Open { endpoint } if endpoint.browse_id.starts_with("VL") => {
                    Some(endpoint)
                }
                _ => None,
            })
            .collect();
        assert!(
            !endpoints.is_empty(),
            "signed-in library did not return saved playlists"
        );
        for endpoint in endpoints.into_iter().take(3) {
            let page =
                fetch(&http, endpoint, "Saved playlist").expect("saved playlist should load");
            println!(
                "Saved playlist: {} tracks, cover={}, metadata={}, bounded={}",
                page.tracks.len(),
                page.details.art.is_some(),
                !page.details.subtitle.is_empty(),
                page.details.truncated
            );
            assert!(!page.details.title.is_empty());
        }
    }
}
