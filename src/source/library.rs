//! Signed-in player actions. Writes run once and success requires server evidence.

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::{home, http::Http};
use crate::config::Cookies;

const BASE: &str = "https://music.youtube.com/youtubei/v1/";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Playlist {
    pub id: String,
    pub title: String,
    pub contains: bool,
}

fn session() -> Result<Cookies> {
    Cookies::available()?.ok_or_else(|| anyhow::anyhow!("Sign in to YouTube Music first."))
}

fn post(http: &Http, cookies: &Cookies, endpoint: &str, body: Value) -> Result<Value> {
    let json = home::post(http, &format!("{BASE}{endpoint}"), Some(cookies), body)?;
    validate_response(&json)?;
    Ok(json)
}

fn validate_response(json: &Value) -> Result<()> {
    if json.get("error").is_some() {
        bail!("YouTube Music refused this action. Reconnect your account and try again.");
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
                "YouTube Music requires account interaction. Reconnect your account and try again."
            );
        }
    }
    Ok(())
}

pub fn rating(http: &Http, video_id: &str) -> Result<bool> {
    read_rating(http, &session()?, video_id)
}

fn read_rating(http: &Http, cookies: &Cookies, video_id: &str) -> Result<bool> {
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

pub fn set_rating(http: &Http, video_id: &str, liked: bool) -> Result<bool> {
    let cookies = session()?;
    change_rating(video_id, liked, |endpoint, body| {
        post(http, &cookies, endpoint, body)
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

pub fn playlists(http: &Http, video_id: &str) -> Result<Vec<Playlist>> {
    read_playlists(http, &session()?, video_id)
}

fn read_playlists(http: &Http, cookies: &Cookies, video_id: &str) -> Result<Vec<Playlist>> {
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
                contains: row["containsSelectedVideos"].as_str() == Some("ALL")
                    || row["selected"].as_bool() == Some(true)
                    || row["checked"].as_bool() == Some(true),
            });
            if options.len() >= 100 {
                return options;
            }
        }
    }
    options
}

pub fn save(http: &Http, video_id: &str, playlist_id: &str) -> Result<()> {
    let cookies = session()?;
    save_with(video_id, playlist_id, |endpoint, body| {
        post(http, &cookies, endpoint, body)
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
    if playlist.contains {
        return Ok(());
    }
    let result = request(
        "browse/edit_playlist",
        json!({
            "playlistId": playlist_id.strip_prefix("VL").unwrap_or(playlist_id),
            "actions": [{"action": "ACTION_ADD_VIDEO", "addedVideoId": video_id}]
        }),
    )?;
    if confirmed_save(&result, video_id) {
        return Ok(());
    }
    // Some clients omit edit results. Verify membership, without repeating the write.
    if parse_playlists(&request("playlist/get_add_to_playlist", body)?)
        .iter()
        .any(|p| p.id == playlist_id && p.contains)
    {
        return Ok(());
    }
    bail!("The save was not confirmed by YouTube Music. Refresh before trying again.")
}

fn confirmed_save(json: &Value, video_id: &str) -> bool {
    json["status"].as_str() == Some("STATUS_SUCCEEDED")
        && json["playlistEditResults"]
            .as_array()
            .is_some_and(|results| {
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
            })
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
                    assert_eq!(body, json!({"playlistId":"p", "actions":[{"action":"ACTION_ADD_VIDEO", "addedVideoId":"v"}]}));
                    Ok(json!({"status":"STATUS_SUCCEEDED"})) }
                3 => { assert_eq!(endpoint, "playlist/get_add_to_playlist"); Ok(choice("ALL")) }
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
                    contains: true
                },
                Playlist {
                    id: "two".into(),
                    title: "Two".into(),
                    contains: false
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
        assert!(!confirmed_save(&json!({"status":"STATUS_SUCCEEDED"}), "v"));
        let result = json!({"status":"STATUS_SUCCEEDED", "playlistEditResults":[{
            "playlistEditVideoAddedResultData":{"videoId":"v", "setVideoId":"entry"}
        }]});
        assert!(confirmed_save(&result, "v"));
        assert!(!confirmed_save(&result, "different"));
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
