//! Signed-in player actions. Writes run once and success requires server evidence.

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::{home, http::Http};
use super::account::Session;

const BASE: &str = "https://music.youtube.com/youtubei/v1/";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Playlist {
    pub id: String,
    pub title: String,
    /// None when the provider's add menu does not expose membership.
    pub contains: Option<bool>,
}

#[cfg(test)]
fn session() -> Result<Session> {
    Session::current()
}

fn post(http: &Http, session: &Session, endpoint: &str, body: Value) -> Result<Value> {
    let result = home::post(http, &format!("{BASE}{endpoint}"), Some(session.cookies()?), body);
    let json = result.map_err(|error| {
        crate::diagnostics::warn("account", &format!("{endpoint} failed: {error:#}"));
        error
    })?;
    validate_response(&json).inspect_err(|error| {
        crate::diagnostics::warn("account", &format!("{endpoint} rejected: {error:#}"));
    })?;
    Ok(json)
}

fn validate_response(json: &Value) -> Result<()> {
    if let Some(error) = json.get("error").filter(|error| !error.is_null()) {
        bail!("{}", super::errors::message(400, Some(error)));
    }
    for key in [
        "showEngagementPanelEndpoint",
        "showDialogCommand",
        "modalWithTitleAndButtonRenderer",
    ] {
        let mut dialogs = Vec::new();
        home::collect(json, key, &mut dialogs);
        if !dialogs.is_empty() {
            bail!(
                "YouTube Music requires confirmation for this action. Check the playlist in YouTube Music."
            );
        }
    }
    Ok(())
}

#[cfg(test)]
pub fn rating(http: &Http, video_id: &str) -> Result<bool> {
    read_rating(http, &session()?, video_id)
}

pub(crate) fn read_rating(http: &Http, cookies: &Session, video_id: &str) -> Result<bool> {
    let json = post(
        http,
        cookies,
        "next",
        json!({"videoId": video_id, "isAudioOnly": true}),
    )?;
    parse_rating(&json, video_id)
        .ok_or_else(|| anyhow::anyhow!("Could not read this song's Like state. Try again."))
}

fn parse_rating(json: &Value, video_id: &str) -> Option<bool> {
    // Current Music sends the player rating outside the queue row. Its target
    // must name this video, so a different song's button cannot set our state.
    let state = |button: &Value| match button["likeStatus"].as_str() {
        Some("LIKE") => Some(true),
        Some("INDIFFERENT" | "DISLIKE") => Some(false),
        _ => None,
    };
    for key in ["likeButtonRenderer", "musicLikeButtonRenderer"] {
        let mut buttons = Vec::new();
        home::collect(json, key, &mut buttons);
        if let Some(liked) = buttons
            .into_iter()
            .filter(|button| {
                button.pointer("/target/videoId").and_then(Value::as_str) == Some(video_id)
            })
            .find_map(state)
        {
            return Some(liked);
        }
    }
    let mut rows = Vec::new();
    home::collect(json, "playlistPanelVideoRenderer", &mut rows);
    let row = rows
        .into_iter()
        .find(|row| row["videoId"].as_str() == Some(video_id))?;
    for key in ["likeButtonRenderer", "musicLikeButtonRenderer"] {
        let mut buttons = Vec::new();
        home::collect(row, key, &mut buttons);
        for button in buttons {
            if let Some(liked) = state(button) {
                return Some(liked);
            }
        }
    }
    None
}

pub(crate) fn set_rating_for(http: &Http, cookies: &Session, video_id: &str, liked: bool) -> Result<bool> {
    change_rating(video_id, liked, |endpoint, body| {
        post(http, cookies, endpoint, body)
    })
}

fn change_rating(
    video_id: &str,
    liked: bool,
    mut request: impl FnMut(&str, Value) -> Result<Value>,
) -> Result<bool> {
    request(
        if liked {
            "like/like"
        } else {
            "like/removelike"
        },
        json!({"target": {"videoId": video_id}}),
    )?;
    // No automatic write retries: a timeout can occur after the server commits.
    let json = request("next", json!({"videoId":video_id, "isAudioOnly":true}))?;
    let actual = parse_rating(&json, video_id).ok_or_else(|| {
        anyhow::anyhow!("Could not confirm this song's Like state. Refresh before trying again.")
    })?;
    if actual != liked {
        bail!("YouTube Music has not confirmed the change. Try again.");
    }
    Ok(actual)
}

#[cfg(test)]
pub fn playlists(http: &Http, video_id: &str) -> Result<Vec<Playlist>> {
    read_playlists(http, &session()?, video_id)
}

pub(crate) fn read_playlists(http: &Http, cookies: &Session, video_id: &str) -> Result<Vec<Playlist>> {
    let json = post(
        http,
        cookies,
        "playlist/get_add_to_playlist",
        json!({"videoIds": [video_id]}),
    )?;
    Ok(parse_playlists(&json))
}

fn parse_playlists(json: &Value) -> Vec<Playlist> {
    let mut options = Vec::new();
    for key in ["playlistAddToOptionRenderer", "addToPlaylistItemRenderer"] {
        let mut rows = Vec::new();
        home::collect(json, key, &mut rows);
        for row in rows {
            let id = row["playlistId"].as_str().or_else(|| {
                row.pointer("/serviceEndpoint/playlistEditEndpoint/playlistId")
                    .and_then(Value::as_str)
            });
            let title = row
                .pointer("/title/runs")
                .and_then(home::runs_text)
                .or_else(|| {
                    row.pointer("/title/simpleText")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                });
            let (Some(id), Some(title)) = (id, title) else {
                continue;
            };
            if id.is_empty() || title.is_empty() || options.iter().any(|p: &Playlist| p.id == id) {
                continue;
            }
            options.push(Playlist {
                id: id.to_owned(),
                title,
                contains: row["containsSelectedVideos"].as_str().and_then(|flag| match flag {
                    "ALL" => Some(true), "NONE" => Some(false), _ => None,
                }).or_else(||row["selected"].as_bool()).or_else(||row["checked"].as_bool()),
            });
            if options.len() >= 100 {
                return options;
            }
        }
    }
    options
}

pub(crate) fn save_for(http: &Http, cookies: &Session, video_id: &str, playlist_id: &str) -> Result<()> {
    save_with(video_id, playlist_id, |endpoint, body| {
        post(http, cookies, endpoint, body)
    })
}

fn save_with(
    video_id: &str,
    playlist_id: &str,
    mut request: impl FnMut(&str, Value) -> Result<Value>,
) -> Result<()> {
    let body = json!({"videoIds":[video_id]});
    let options = parse_playlists(&request("playlist/get_add_to_playlist", body.clone())?);
    let Some(playlist) = options.iter().find(|p| p.id == playlist_id) else {
        bail!("This playlist is no longer available for saving.");
    };
    if playlist.contains == Some(true) {
        return Ok(());
    }
    if playlist_id.strip_prefix("VL").unwrap_or(playlist_id) == "LM" {
        change_rating(video_id, true, request)?;
        return Ok(());
    }
    let result = request(
        "browse/edit_playlist",
        json!({
            "playlistId": playlist_id.strip_prefix("VL").unwrap_or(playlist_id),
            "actions": [{"action": "ACTION_ADD_VIDEO", "addedVideoId": video_id, "dedupeOption":"DEDUPE_OPTION_CHECK"}]
        }),
    );
    if result.as_ref().is_ok_and(|json|confirmed_save(json, video_id)) {
        return Ok(());
    }
    // A duplicate dialog or lost acknowledgement can follow a committed write.
    // Read the actual track shelf; modern add menus omit membership flags.
    if verify_membership(video_id, playlist_id, &mut request).unwrap_or(false) {
        return Ok(());
    }
    result?;
    bail!("The save was not confirmed. Check the playlist in YouTube Music before trying again.")
}

fn verify_membership(video_id: &str, playlist_id: &str, request: &mut impl FnMut(&str, Value) -> Result<Value>) -> Result<bool> {
    let id = playlist_id.strip_prefix("VL").unwrap_or(playlist_id);
    let mut body = json!({"browseId":format!("VL{id}")});
    let mut seen = std::collections::HashSet::new();
    for _ in 0..8 {
        let json = request("browse", body)?;
        if super::collection::contains_video(&json, video_id) { return Ok(true); }
        let Some(token) = super::collection::page_continuation(&json).filter(|token|seen.insert(token.clone())) else { return Ok(false); };
        body = json!({"continuation":token});
    }
    Ok(false)
}

fn confirmed_save(json: &Value, video_id: &str) -> bool {
    json["status"].as_str() == Some("STATUS_SUCCEEDED")
        && (json["playlistEditResults"].is_null() || json["playlistEditResults"]
            .as_array()
            .is_some_and(|results| {
                results.is_empty() ||
                results.iter().any(|result| {
                    result
                        .pointer("/playlistEditVideoAddedResultData/videoId")
                        .and_then(Value::as_str)
                        == Some(video_id)
                        && result
                            .pointer("/playlistEditVideoAddedResultData/setVideoId")
                            .and_then(Value::as_str)
                            .is_some_and(|s| !s.is_empty())
                })
            }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_rating_requires_the_matching_target() {
        let json = json!({"playerOverlays":{"playerOverlayRenderer":{"actions":[
            {"likeButtonRenderer":{"target":{"videoId":"other"}, "likeStatus":"LIKE"}},
            {"likeButtonRenderer":{"target":{"videoId":"current"}, "likeStatus":"INDIFFERENT"}}
        ]}}});
        assert_eq!(parse_rating(&json, "current"), Some(false));
        assert_eq!(parse_rating(&json, "missing"), None);
    }

    #[test]
    fn liking_and_unliking_require_confirmation_without_retrying_writes() {
        for liked in [true, false] {
            let mut calls = 0;
            let actual = change_rating("v", liked, |endpoint, body| {
                calls += 1;
                if calls == 1 {
                    assert_eq!(endpoint, if liked { "like/like" } else { "like/removelike" });
                    assert_eq!(body, json!({"target":{"videoId":"v"}}));
                    return Ok(json!({}));
                }
                assert_eq!(endpoint, "next");
                Ok(json!({"likeButtonRenderer":{"target":{"videoId":"v"}, "likeStatus":if liked { "LIKE" } else { "INDIFFERENT" }}}))
            }).unwrap();
            assert_eq!(actual, liked);
            assert_eq!(calls, 2);
        }
        let mut writes = 0;
        assert!(
            change_rating("v", true, |_, _| {
                writes += 1;
                bail!("timeout after write")
            })
            .is_err()
        );
        assert_eq!(writes, 1);
        assert!(change_rating("v", true, |_, _| Ok(json!({}))).is_err());
    }

    #[test]
    fn save_checks_membership_and_confirms_the_exact_song() {
        let choice = |contains| json!({"playlistAddToOptionRenderer":{"playlistId":"VLp", "title":{"simpleText":"Playlist"}, "containsSelectedVideos":contains}});
        let mut calls = 0;
        save_with("v", "VLp", |endpoint, body| {
            calls += 1;
            match calls {
                1 => { assert_eq!(endpoint, "playlist/get_add_to_playlist"); Ok(choice("NONE")) }
                2 => { assert_eq!(endpoint, "browse/edit_playlist");
                    assert_eq!(body, json!({"playlistId":"p", "actions":[{"action":"ACTION_ADD_VIDEO", "addedVideoId":"v", "dedupeOption":"DEDUPE_OPTION_CHECK"}]}));
                    Ok(json!({})) }
                3 => { assert_eq!(endpoint, "browse"); Ok(json!({"musicPlaylistShelfRenderer":{"contents":[{"musicResponsiveListItemRenderer":{"playlistItemData":{"videoId":"v"}}}]}})) }
                _ => panic!("write repeated"),
            }
        }).unwrap();
        assert_eq!(calls, 3);
        calls = 0;
        save_with("v", "VLp", |_, _| {
            calls += 1;
            Ok(choice("ALL"))
        })
        .unwrap();
        assert_eq!(calls, 1, "already saved song must not be written again");
        assert!(save_with("v", "missing", |_, _| Ok(choice("NONE"))).is_err());
    }

    #[test]
    fn rating_is_scoped_to_current_video_including_wrapped_rows() {
        let json = json!({"contents": [
            {"playlistPanelVideoRenderer": {"videoId":"other", "menu":{"likeButtonRenderer":{"likeStatus":"LIKE"}}}},
            {"playlistPanelVideoWrapperRenderer":{"primaryRenderer":{"playlistPanelVideoRenderer":{
                "videoId":"current", "menu":{"musicLikeButtonRenderer":{"likeStatus":"DISLIKE"}}
            }}}}
        ]});
        assert_eq!(parse_rating(&json, "current"), Some(false));
        assert_eq!(parse_rating(&json, "missing"), None);
    }

    #[test]
    fn editable_options_preserve_membership_and_ignore_parent_ids() {
        let json = json!({"playlistId":"parent", "title":{"simpleText":"Parent"}, "contents":[
            {"playlistAddToOptionRenderer":{"playlistId":"one", "title":{"runs":[{"text":"One"}]}, "containsSelectedVideos":"ALL"}},
            {"playlistAddToOptionRenderer":{"playlistId":"one", "title":{"simpleText":"Duplicate"}}},
            {"addToPlaylistItemRenderer":{"title":{"simpleText":"Two"}, "serviceEndpoint":{"playlistEditEndpoint":{"playlistId":"two"}}}},
            {"playlistAddToOptionRenderer":{"playlistId":"broken"}}
        ]});
        assert_eq!(
            parse_playlists(&json),
            vec![
                Playlist {
                    id: "one".into(),
                    title: "One".into(),
                    contains: Some(true)
                },
                Playlist {
                    id: "two".into(),
                    title: "Two".into(),
                    contains: None
                }
            ]
        );
    }

    #[test]
    fn gated_and_incomplete_writes_are_not_success() {
        assert!(
            validate_response(&json!({"actions":[{"showEngagementPanelEndpoint":{}}]})).is_err()
        );
        assert!(validate_response(&json!({"error":{"code":403}})).is_err());
        assert!(confirmed_save(&json!({"status":"STATUS_SUCCEEDED"}), "v"));
        let result = json!({"status":"STATUS_SUCCEEDED", "playlistEditResults":[{
            "playlistEditVideoAddedResultData":{"videoId":"v", "setVideoId":"entry"}
        }]});
        assert!(confirmed_save(&result, "v"));
        assert!(!confirmed_save(&result, "different"));
    }

    #[test]
    fn modern_status_only_acknowledgements_do_not_report_a_false_failure() {
        let mut writes = 0;
        save_with("v", "p", |endpoint, _| {
            if endpoint == "playlist/get_add_to_playlist" {
                return Ok(json!({"playlistAddToOptionRenderer":{"playlistId":"p","title":{"simpleText":"Playlist"}}}));
            }
            assert_eq!(endpoint, "browse/edit_playlist");
            writes += 1;
            Ok(json!({"status":"STATUS_SUCCEEDED"}))
        }).unwrap();
        assert_eq!(writes, 1);
        assert!(!confirmed_save(&json!({"status":"STATUS_SUCCEEDED","playlistEditResults":"invalid"}), "v"));
    }

    #[test]
    fn liked_music_saves_use_the_like_operation_and_confirm_the_song() {
        let mut calls = Vec::new();
        save_with("v", "LM", |endpoint, body| {
            calls.push(endpoint.to_owned());
            Ok(match endpoint {
                "playlist/get_add_to_playlist" => json!({"playlistAddToOptionRenderer":{"playlistId":"LM","title":{"simpleText":"Liked Music"}}}),
                "like/like" => { assert_eq!(body, json!({"target":{"videoId":"v"}})); json!({}) },
                "next" => json!({"likeButtonRenderer":{"target":{"videoId":"v"},"likeStatus":"LIKE"}}),
                _ => panic!("wrong save operation"),
            })
        }).unwrap();
        assert_eq!(calls, ["playlist/get_add_to_playlist", "like/like", "next"]);
    }

    #[test]
    fn a_duplicate_dialog_is_confirmed_from_track_membership_without_another_write() {
        let mut writes = 0;
        let mut reads = 0;
        save_with("v", "p", |endpoint, body| {
            match endpoint {
                "playlist/get_add_to_playlist" => Ok(json!({"playlistAddToOptionRenderer":{"playlistId":"p","title":{"simpleText":"Playlist"}}})),
                "browse/edit_playlist" => { writes += 1; bail!("provider requires confirmation") },
                "browse" => {
                    reads += 1;
                    if reads == 1 {
                        assert_eq!(body, json!({"browseId":"VLp"}));
                        Ok(json!({"musicPlaylistShelfRenderer":{"contents":[],"continuations":[{"nextContinuationData":{"continuation":"next"}}]},
                            "suggestions":{"musicResponsiveListItemRenderer":{"playlistItemData":{"videoId":"v"}}}}))
                    } else {
                        assert_eq!(body, json!({"continuation":"next"}));
                        Ok(json!({"musicPlaylistShelfContinuation":{"contents":[{"musicResponsiveListItemRenderer":{"playlistItemData":{"videoId":"v"}}}]}}))
                    }
                },
                _ => panic!("unexpected request"),
            }
        }).unwrap();
        assert_eq!((writes, reads), (1, 2));
    }

    #[test]
    #[ignore = "read-only authenticated Music player action probe"]
    fn live_player_action_reads() {
        let http = Http::new().unwrap();
        let id = "kPa7bsKwL-c";
        let choices = playlists(&http, id).expect("playlist choices should load");
        println!("Editable playlist choices: {}", choices.len());
        println!(
            "Current Like state: {:?}",
            rating(&http, id).expect("Like state should load")
        );
    }
}
