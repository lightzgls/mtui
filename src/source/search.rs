//! Typed YouTube Music search. Browse targets never become fake playable tracks.

use std::collections::HashSet;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::Track;
use super::home::{self, Card, Target};
use super::http::Http;
use super::innertube::{flex_runs, parse_duration};
use crate::config::Cookies;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Filter {
    #[default]
    All,
    Songs,
    Artists,
    Albums,
    Playlists,
    Videos,
}

impl Filter {
    pub const ALL: [Self; 6] = [
        Self::All,
        Self::Songs,
        Self::Artists,
        Self::Albums,
        Self::Playlists,
        Self::Videos,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Songs => "Songs",
            Self::Artists => "Artists",
            Self::Albums => "Albums",
            Self::Playlists => "Playlists",
            Self::Videos => "Videos",
        }
    }

    fn params(self) -> Option<String> {
        // Music web search protobuf parameters, also documented by ytmusicapi.
        let field = match self {
            Self::All => return None,
            Self::Playlists => return Some("Eg-KAQwIABAAGAAgACgBMABqChAEEAMQCRAFEAo%3D".into()),
            Self::Songs => "II",
            Self::Artists => "Ig",
            Self::Albums => "IY",
            Self::Videos => "IQ",
        };
        Some(format!("EgWKAQ{field}AWoMEA4QChADEAQQCRAF"))
    }
}

#[derive(Debug, Clone)]
pub struct Item {
    pub card: Card,
    pub kind: Filter,
    pub track: Option<Track>,
}

pub fn fetch(http: &Http, query: &str, filter: Filter, limit: usize) -> Result<Vec<Item>> {
    let cookies = Cookies::available().ok().flatten();
    let mut items = fetch_category(http, cookies.as_ref(), query, filter, limit)?;
    if filter == Filter::All {
        // Music sometimes serves only a top artist and songs in its generic
        // response. Fill the missing categories from its real filtered search.
        for category in Filter::ALL.into_iter().skip(1) {
            if items.len() >= limit {
                break;
            }
            if !items.iter().any(|item| item.kind == category)
                && let Ok(more) = fetch_category(
                    http,
                    cookies.as_ref(),
                    query,
                    category,
                    5.min(limit - items.len()),
                )
            {
                items.extend(more);
            }
        }
    }
    Ok(items)
}

fn fetch_category(
    http: &Http,
    cookies: Option<&Cookies>,
    query: &str,
    filter: Filter,
    limit: usize,
) -> Result<Vec<Item>> {
    let mut body = json!({"query": query});
    if let Some(params) = filter.params() {
        body["params"] = params.into();
    }
    let request = home::post_request_as(
        http,
        "https://music.youtube.com/youtubei/v1/search",
        cookies,
        super::innertube::MUSIC_CLIENT_VERSION,
        body,
    )?
    .timeout(std::time::Duration::from_secs(5));
    let (status, raw) = http.send(request)?;
    if !(200..300).contains(&status) {
        bail!("YouTube Music search refused the request: HTTP {status}");
    }
    let json = serde_json::from_slice(&raw)?;
    Ok(parse(&json, filter, limit))
}

fn classify(card: &Card, category: &str) -> Filter {
    match &card.target {
        Target::Artist { .. } => Filter::Artists,
        Target::Open { endpoint } => {
            if endpoint.browse_id.starts_with("MPRE") || category.contains("Album") {
                Filter::Albums
            } else {
                Filter::Playlists
            }
        }
        Target::Play { .. } => {
            if category.contains("Video") || card.kind() == Some("Video") {
                Filter::Videos
            } else {
                Filter::Songs
            }
        }
    }
}

fn item(raw: &Value, category: &str) -> Option<Item> {
    let card = home::parse_card(raw)?;
    let kind = classify(&card, category);
    let mut track = card.track();
    if let Some(track) = track.as_mut() {
        let row = &raw["musicResponsiveListItemRenderer"];
        if let Some(runs) = flex_runs(row, 1).and_then(Value::as_array) {
            // Ignore type/year/view-count metadata; use canonical linked names.
            if let Some(artist) = &card.artist_ref {
                track.uploader = artist.name.clone();
            }
            track.album = runs.iter().find_map(|run| {
                let id = run
                    .pointer("/navigationEndpoint/browseEndpoint/browseId")?
                    .as_str()?;
                id.starts_with("MPRE")
                    .then(|| run["text"].as_str().unwrap_or_default().to_string())
            });
        }
        track.duration = card.duration.or_else(|| {
            card.subtitle
                .split('•')
                .map(str::trim)
                .find_map(parse_duration)
        });
    }
    Some(Item { card, kind, track })
}

pub(super) fn parse(json: &Value, filter: Filter, limit: usize) -> Vec<Item> {
    // Walk only search sections, never menu suggestions or unrelated renderers.
    fn visit(value: &Value, items: &mut Vec<Item>) {
        if let Some(shelf) = value.get("musicShelfRenderer") {
            let title = shelf
                .pointer("/title/runs")
                .and_then(home::runs_text)
                .unwrap_or_default();
            if let Some(rows) = shelf["contents"].as_array() {
                items.extend(rows.iter().filter_map(|row| item(row, &title)));
            }
            return;
        }
        if let Some(top) = value.get("musicCardShelfRenderer") {
            // Normalize the top-result hero into the same card parser.
            let mut hero = top.clone();
            let endpoint = top
                .pointer("/title/runs/0/navigationEndpoint")
                .or_else(|| top.get("onTap"));
            if let Some(endpoint) = endpoint {
                hero["navigationEndpoint"] = endpoint.clone();
            }
            hero["thumbnailRenderer"] = top["thumbnail"].clone();
            if let Some(card) = home::parse_card(&json!({"musicTwoRowItemRenderer": hero})) {
                let kind = classify(&card, "");
                let track = card.track();
                items.push(Item { card, kind, track });
            }
            if let Some(rows) = top["contents"].as_array() {
                items.extend(rows.iter().filter_map(|row| item(row, "")));
            }
            return;
        }
        match value {
            Value::Object(map) => {
                for child in map.values() {
                    visit(child, items);
                }
            }
            Value::Array(array) => {
                for child in array {
                    visit(child, items);
                }
            }
            _ => {}
        }
    }
    let mut items = Vec::new();
    visit(&json["contents"], &mut items);
    let mut seen = HashSet::new();
    items
        .into_iter()
        .filter(|item| filter == Filter::All || item.kind == filter)
        .filter(|item| seen.insert(item.card.art_key().to_owned()))
        .take(limit)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixed_results_keep_browse_routes_and_playable_ids_separate() {
        let row = |name: &str, endpoint: Value| {
            json!({"musicResponsiveListItemRenderer": {
                "navigationEndpoint": endpoint,
                "flexColumns": [{"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": name}]}}}]
            }})
        };
        let data = json!({"contents": [{"musicShelfRenderer": {"contents": [
            row("artist", json!({"browseEndpoint": {"browseId":"UCartist", "browseEndpointContextSupportedConfigs":{"browseEndpointContextMusicConfig":{"pageType":"MUSIC_PAGE_TYPE_ARTIST"}}}})),
            row("album", json!({"browseEndpoint": {"browseId":"MPREalbum", "params":"opaque"}})),
            row("playlist", json!({"browseEndpoint": {"browseId":"VLplaylist"}})),
            row("song", json!({"watchEndpoint": {"videoId":"song1234567"}}))
        ]}}]});
        let items = parse(&data, Filter::All, 60);
        assert_eq!(items.len(), 4);
        assert_eq!(items.iter().filter(|item| item.track.is_some()).count(), 1);
        assert_eq!(parse(&data, Filter::Albums, 20).len(), 1);
        assert!(
            matches!(&items[1].card.target, Target::Open { endpoint } if endpoint.params.as_deref() == Some("opaque"))
        );
    }

    #[test]
    #[ignore = "read-only live YouTube Music search"]
    fn live_mixed_search_and_filters() {
        let http = Http::new().unwrap();
        let all = fetch(&http, "Radiohead", Filter::All, 60).unwrap();
        println!("All: {} results", all.len());
        for filter in [
            Filter::Songs,
            Filter::Artists,
            Filter::Albums,
            Filter::Playlists,
        ] {
            assert!(
                all.iter().any(|item| item.kind == filter),
                "missing {} in mixed search",
                filter.label()
            );
            let items = fetch(&http, "Radiohead", filter, 20).unwrap();
            assert!(!items.is_empty(), "empty {}", filter.label());
            assert!(items.iter().all(|item| item.kind == filter));
            println!("{}: {} results", filter.label(), items.len());
        }
    }

    #[test]
    #[ignore = "read-only native stream check; saves a bounded AAC fixture under target"]
    fn acquire_audio_regression_fixture() {
        let http = Http::new().unwrap();
        let items = fetch(&http, "Birds of a Feather Billie Eilish", Filter::Songs, 1).unwrap();
        let id = &items[0].track.as_ref().unwrap().id;
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(10)).build().unwrap();
        let stream = mtui_resolver::resolve_player(&runtime, &client, None, id).unwrap();
        let bytes = runtime.block_on(async {
            let mut response = client.get(&stream.url).header(reqwest::header::RANGE, "bytes=0-524287").send().await.map_err(|_| "audio request failed")?;
            if !response.status().is_success() { return Err("audio request was refused"); }
            let mut bytes = Vec::new();
            while bytes.len() < 524_288 {
                let Some(chunk) = response.chunk().await.map_err(|_| "audio read failed")? else { break; };
                bytes.extend_from_slice(&chunk[..chunk.len().min(524_288 - bytes.len())]);
            }
            Ok(bytes)
        }).unwrap();
        assert!(bytes.len() >= 32_768);
        std::fs::write("target/playback-regression.m4a", &bytes).unwrap();
        println!("Native AAC opening check passed: {} fixture bytes, itag {:?}", bytes.len(), stream.format.itag);
    }
}
