//! YouTube Music artist pages.
//!
//! An artist browse is not a playlist. Its first shelf is a short list of top
//! songs, followed by carousels of albums, singles, videos and related artists.
//! Keeping that structure is what makes this a page rather than another flat
//! track list.

use anyhow::{Result, bail};
use serde_json::Value;

use crate::source::catalog::home::{self, Shelf};
use crate::source::http::Http;
use crate::source::youtube::innertube::{flex_column, flex_runs, parse_duration};
use crate::source::{ArtistRef, BrowseEndpoint, Track, UNKNOWN_ARTIST};

const MAX_TOP_SONGS: usize = 50;
const MAX_SECTIONS: usize = 12;
const MAX_DESCRIPTION_CHARS: usize = 600;

#[derive(Debug, Clone)]
pub struct ArtistPage {
    pub artist: ArtistRef,
    pub audience: Option<String>,
    pub description: Option<String>,
    pub art: Option<String>,
    pub top_songs: Vec<ArtistSong>,
    /// The provider's full song listing, rather than the short artist preview.
    pub songs_endpoint: Option<BrowseEndpoint>,
    pub songs_truncated: bool,
    pub shelves: Vec<Shelf>,
}

impl ArtistPage {
    pub fn art_key(&self) -> String {
        format!("artist-header:{}", self.artist.endpoint.browse_id)
    }
}

#[derive(Debug, Clone)]
pub struct ArtistSong {
    pub track: Track,
    /// Already abbreviated by YouTube, for example "65M plays".
    pub plays: Option<String>,
}

pub fn fetch(http: &Http, artist: ArtistRef) -> Result<ArtistPage> {
    let json = home::browse_endpoint(http, None, &artist.endpoint)?;
    parse(&json, artist)
}

fn parse(json: &Value, mut artist: ArtistRef) -> Result<ArtistPage> {
    let header = find_header(json);
    if let Some(name) = header.and_then(header_title) {
        artist.name = name;
    }

    let audience = header.and_then(header_audience);
    let description = header
        .and_then(header_description)
        .or_else(|| description_shelf(json))
        .map(|text| bounded(&text, MAX_DESCRIPTION_CHARS));
    let art = header.and_then(thumbnail_url);
    let top_songs = top_songs(json);
    let shelves = home::parse_shelves(json)
        .into_iter()
        .filter(|shelf| !shelf.title.eq_ignore_ascii_case("Top songs"))
        .take(MAX_SECTIONS)
        .collect::<Vec<_>>();

    if top_songs.is_empty() && shelves.is_empty() {
        bail!("YouTube Music returned no artist content");
    }

    Ok(ArtistPage {
        artist,
        audience,
        description,
        art,
        top_songs,
        songs_endpoint: song_shelf(json).and_then(|shelf| {
            home::browse_route(&shelf["bottomEndpoint"])
                .or_else(|| shelf.pointer("/title/runs")?.as_array()?.iter()
                    .find_map(|run| home::browse_route(&run["navigationEndpoint"])))
        }),
        songs_truncated: false,
        shelves,
    })
}

fn find_header(json: &Value) -> Option<&Value> {
    for key in [
        "musicImmersiveHeaderRenderer",
        "musicVisualHeaderRenderer",
        "musicDetailHeaderRenderer",
    ] {
        let mut found = Vec::new();
        home::collect(json, key, &mut found);
        if let Some(header) = found.into_iter().next() {
            return Some(header);
        }
    }
    None
}

fn header_title(header: &Value) -> Option<String> {
    ["/title/runs", "/title"]
        .into_iter()
        .find_map(|path| text_at(header, path))
}

fn header_audience(header: &Value) -> Option<String> {
    [
        "/monthlyListenerCount/runs",
        "/monthlyListenerCount",
        "/subscriptionButton/subscribeButtonRenderer/longSubscriberCountText/runs",
        "/subscriptionButton/subscribeButtonRenderer/longSubscriberCountText",
        "/subscriptionButton/subscribeButtonRenderer/subscriberCountText/runs",
        "/subscriptionButton/subscribeButtonRenderer/subscriberCountText",
        "/subtitle/runs",
    ]
    .into_iter()
    .find_map(|path| text_at(header, path))
}

fn header_description(header: &Value) -> Option<String> {
    ["/description/runs", "/description"]
        .into_iter()
        .find_map(|path| text_at(header, path))
}

fn description_shelf(json: &Value) -> Option<String> {
    let mut shelves = Vec::new();
    home::collect(json, "musicDescriptionShelfRenderer", &mut shelves);
    shelves
        .into_iter()
        .find_map(|shelf| text_at(shelf, "/description/runs"))
}

fn text_at(value: &Value, path: &str) -> Option<String> {
    let value = value.pointer(path)?;
    value
        .as_str()
        .map(str::to_string)
        .or_else(|| value["simpleText"].as_str().map(str::to_string))
        .or_else(|| home::runs_text(value))
        .filter(|text| !text.trim().is_empty())
}

fn thumbnail_url(header: &Value) -> Option<String> {
    let mut lists = Vec::new();
    home::collect(header, "thumbnails", &mut lists);
    let mut candidates = lists
        .into_iter()
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|thumbnail| {
            Some((
                thumbnail["width"].as_u64().unwrap_or(0),
                thumbnail["url"].as_str()?,
            ))
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|(width, _)| *width);
    candidates
        .iter()
        .find(|(width, _)| *width >= crate::terminal::graphics::art::EDGE as u64)
        .or_else(|| candidates.last())
        .map(|(_, url)| (*url).to_string())
}

fn top_songs(json: &Value) -> Vec<ArtistSong> {
    song_shelf(json).and_then(|shelf| shelf["contents"].as_array())
        .into_iter().flatten()
        .filter_map(|item| parse_song(&item["musicResponsiveListItemRenderer"]))
        .take(MAX_TOP_SONGS).collect()
}

fn song_shelf(json: &Value) -> Option<&Value> {
    let mut shelves = Vec::new();
    home::collect(json, "musicShelfRenderer", &mut shelves);

    let named = shelves.iter().copied().find(|shelf| {
        text_at(shelf, "/title/runs").is_some_and(|title| title.eq_ignore_ascii_case("Top songs"))
    });
    named
        .into_iter()
        .chain(shelves)
        .find(|shelf| shelf["contents"].as_array().is_some_and(|items|
            items.iter().any(|item| parse_song(&item["musicResponsiveListItemRenderer"]).is_some())))
}

fn parse_song(row: &Value) -> Option<ArtistSong> {
    let details = flex_column(row, 1).unwrap_or_default();
    let uploader = details
        .split('•')
        .map(str::trim)
        .find(|field| !field.is_empty())
        .unwrap_or(UNKNOWN_ARTIST)
        .to_string();
    let duration = home::fixed_column(row, 0)
        .as_deref()
        .and_then(parse_duration);
    let plays = (1..row["flexColumns"].as_array()?.len())
        .filter_map(|index| flex_column(row, index)).find(|text| home::is_play_count(text));
    let album = home::row_album(row);

    Some(ArtistSong {
        track: Track {
            id: home::video_id(row)?,
            title: flex_column(row, 0)?,
            uploader,
            duration,
            album,
            artist_ref: flex_runs(row, 1).and_then(home::artist_ref),
        },
        plays,
    })
}

fn bounded(text: &str, max: usize) -> String {
    let end = text
        .char_indices()
        .nth(max)
        .map_or(text.len(), |(index, _)| index);
    text[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::BrowseEndpoint;

    fn artist() -> ArtistRef {
        ArtistRef {
            name: "Fallback".to_string(),
            endpoint: BrowseEndpoint::new("UCartist"),
        }
    }

    fn artist_endpoint(id: &str) -> Value {
        serde_json::json!({
            "browseEndpoint": {
                "browseId": id,
                "browseEndpointContextSupportedConfigs": {
                    "browseEndpointContextMusicConfig": {
                        "pageType": "MUSIC_PAGE_TYPE_ARTIST"
                    }
                }
            }
        })
    }

    fn song_row() -> Value {
        let column = |runs: Value| {
            serde_json::json!({
                "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": runs } }
            })
        };
        serde_json::json!({
            "musicResponsiveListItemRenderer": {
                "playlistItemData": { "videoId": "JhulBGMA7G4" },
                "flexColumns": [
                    column(serde_json::json!([ { "text": "Harder Better Faster Stronger" } ])),
                    column(serde_json::json!([ {
                        "text": "Daft Punk",
                        "navigationEndpoint": artist_endpoint("UCdaft")
                    } ])),
                    column(serde_json::json!([ { "text": "65M plays" } ])),
                    column(serde_json::json!([ { "text": "Discovery" } ]))
                ],
                "fixedColumns": []
            }
        })
    }

    #[test]
    fn parses_header_top_songs_and_release_shelves() {
        let json = serde_json::json!({
            "header": { "musicImmersiveHeaderRenderer": {
                "title": { "runs": [ { "text": "Daft Punk" } ] },
                "monthlyListenerCount": { "runs": [ { "text": "25M monthly listeners" } ] },
                "description": { "runs": [ { "text": "French electronic duo." } ] },
                "thumbnail": { "musicThumbnailRenderer": { "thumbnail": { "thumbnails": [
                    { "url": "https://example.invalid/header", "width": 120, "height": 120 }
                ] } } }
            } },
            "contents": [
                { "musicShelfRenderer": {
                    "title": { "runs": [ { "text": "Top songs" } ] },
                    "contents": [ song_row() ]
                } },
                { "musicCarouselShelfRenderer": {
                    "header": { "musicCarouselShelfBasicHeaderRenderer": {
                        "title": { "runs": [ { "text": "Albums" } ] }
                    } },
                    "contents": [ { "musicTwoRowItemRenderer": {
                        "title": { "runs": [ { "text": "Discovery" } ] },
                        "subtitle": { "runs": [ { "text": "Album" } ] },
                        "navigationEndpoint": { "browseEndpoint": { "browseId": "MPREalbum" } }
                    } } ]
                } }
            ]
        });

        let page = parse(&json, artist()).expect("artist should parse");
        assert_eq!(page.artist.name, "Daft Punk");
        assert_eq!(page.audience.as_deref(), Some("25M monthly listeners"));
        assert_eq!(page.top_songs.len(), 1);
        assert_eq!(page.top_songs[0].plays.as_deref(), Some("65M plays"));
        assert_eq!(page.top_songs[0].track.album.as_deref(), Some("Discovery"));
        assert_eq!(page.top_songs[0].track.duration, None);
        assert_eq!(
            page.top_songs[0]
                .track
                .artist_ref
                .as_ref()
                .map(|artist| artist.endpoint.browse_id.as_str()),
            Some("UCdaft")
        );
        assert_eq!(page.shelves[0].title, "Albums");
    }

    #[test]
    fn rejects_a_page_with_no_usable_content() {
        assert!(parse(&serde_json::json!({}), artist()).is_err());
    }

    #[test]
    fn play_counts_are_not_albums_when_artist_columns_change_order() {
        let column = |text: &str| serde_json::json!({
            "musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[{"text":text}]}}
        });
        for fields in [["Requiem", "90M plays"], ["90M plays", "Requiem"]] {
            let mut row = song_row()["musicResponsiveListItemRenderer"].clone();
            row["flexColumns"][2] = column(fields[0]);
            row["flexColumns"][3] = column(fields[1]);
            let song = parse_song(&row).unwrap();
            assert_eq!(song.track.album.as_deref(), Some("Requiem"));
            assert_eq!(song.plays.as_deref(), Some("90M plays"));
        }
    }

    #[test]
    fn full_song_route_belongs_to_the_song_shelf_and_preserves_params() {
        let json = serde_json::json!({"contents":[
            {"musicShelfRenderer":{"title":{"runs":[{"text":"Top songs"}]},
                "contents":[song_row()],
                "bottomEndpoint":{"browseEndpoint":{"browseId":"VLsongs","params":"artist-order"}}}},
            {"musicShelfRenderer":{"title":{"runs":[{"text":"Videos"}]},
                "bottomEndpoint":{"browseEndpoint":{"browseId":"VLvideos"}}}}
        ]});
        let page = parse(&json, artist()).unwrap();
        let route = page.songs_endpoint.unwrap();
        assert_eq!(route.browse_id, "VLsongs");
        assert_eq!(route.params.as_deref(), Some("artist-order"));
        assert_eq!(page.top_songs.len(), 1);
    }

    #[test]
    fn bounds_descriptions_on_character_boundaries() {
        assert_eq!(bounded("日本語", 2), "日本");
    }

    #[test]
    fn prefers_the_descriptive_subscriber_count() {
        let header = serde_json::json!({
            "subscriptionButton": { "subscribeButtonRenderer": {
                "longSubscriberCountText": { "runs": [ { "text": "8.4M subscribers" } ] },
                "subscriberCountText": { "runs": [ { "text": "8.4M" } ] }
            } }
        });

        assert_eq!(
            header_audience(&header).as_deref(),
            Some("8.4M subscribers")
        );
    }

    #[test]
    #[ignore = "hits the live YouTube Music API"]
    fn artist_page_against_the_live_api() {
        let http = Http::new().expect("client should build");
        let page = fetch(
            &http,
            ArtistRef {
                name: "Tame Impala".to_string(),
                endpoint: BrowseEndpoint::new("UCGz-eguN8tcic5kUG4s1ZgA"),
            },
        )
        .expect("artist page should answer");

        assert!(!page.top_songs.is_empty());
        assert!(!page.shelves.is_empty());
    }
}
