//! The signed-in user's YouTube library, through the official Data API v3.
//!
//! Deliberately *not* the internal API that [`crate::source::innertube`] uses.
//! That one is fast and unauthenticated and can afford to break, because every
//! failure there falls back to yt-dlp and the user sees nothing. This is
//! different in kind: it reads and writes a real Google account, so it uses the
//! documented, versioned, quota-metered endpoints and has no fallback. A break
//! here should be a clear error, not a silent guess.
//!
//! The cost of that choice is metadata quality. The Data API returns a video
//! title as its uploader typed it -- "Artist - Song (Official Music Video)" --
//! where the music search returns artist, album and title as separate fields.
//! Splitting that back apart is exactly the guesswork the music corpus exists
//! to avoid, so titles are shown as they come.
//!
//! Quota: the default allowance is 10,000 units a day. Listing costs 1 unit per
//! request, writes cost 50. A heavy session runs to a few hundred.

use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::auth::{Http, Session, describe};
use super::{Track, UNKNOWN_ARTIST};

const API: &str = "https://www.googleapis.com/youtube/v3";

/// Stands in for "Liked songs", which is not a playlist the API will list.
///
/// Deliberately not a valid YouTube playlist id -- those are alphanumeric with
/// `-` and `_` -- so it can never collide with a real one.
pub const LIKED_ID: &str = "@liked";

/// The most rows one request may ask for. The API's own ceiling, not ours.
const PAGE: usize = 50;

/// A playlist as shown in the library pane.
#[derive(Debug, Clone)]
pub struct Playlist {
    pub id: String,
    pub title: String,
    /// `None` for "Liked songs", whose size the API does not report up front.
    pub count: Option<u64>,
}

impl Playlist {
    pub fn is_liked(&self) -> bool {
        self.id == LIKED_ID
    }
}

pub struct Library {
    http: Http,
    /// `None` until the user signs in. Reloaded from disk rather than handed
    /// across threads, so the sign-in thread and this one share a file instead
    /// of a lock.
    session: Option<Session>,
}

impl Library {
    pub fn new() -> Result<Self> {
        Ok(Self {
            http: Http::new()?,
            // A failure to load is not a failure to start: the user may simply
            // never sign in, and if they do, `reload` reports the real reason.
            session: Session::load().ok().flatten(),
        })
    }

    pub fn is_signed_in(&self) -> bool {
        self.session.is_some()
    }

    /// Picks up tokens written by the sign-in thread.
    ///
    /// Also called speculatively before every request while signed out, so the
    /// "nothing there" case is worded for that rather than for a sign-in that
    /// just finished -- it is the one that happens on every launch.
    pub fn reload(&mut self) -> Result<()> {
        self.session = Session::load()?;
        if self.session.is_none() {
            bail!("no saved sign-in was found");
        }
        Ok(())
    }

    pub fn sign_out(&mut self) {
        self.session = None;
    }

    /// The user's playlists, with "Liked songs" in front.
    ///
    /// Liked songs lead because they are the list a music player is actually
    /// opened for, and they are one keypress from the top that way.
    pub fn playlists(&mut self) -> Result<Vec<Playlist>> {
        let mut playlists = vec![Playlist {
            id: LIKED_ID.to_string(),
            title: "Liked songs".to_string(),
            count: None,
        }];

        let mut page = String::new();
        loop {
            let mut params = vec![
                ("part", "snippet,contentDetails"),
                ("mine", "true"),
                ("maxResults", "50"),
            ];
            if !page.is_empty() {
                params.push(("pageToken", page.as_str()));
            }
            let json = self.get("playlists", &params)?;

            for item in json["items"].as_array().into_iter().flatten() {
                let Some(id) = item["id"].as_str() else {
                    continue;
                };
                playlists.push(Playlist {
                    id: id.to_string(),
                    title: item
                        .pointer("/snippet/title")
                        .and_then(|t| t.as_str())
                        .unwrap_or("(untitled playlist)")
                        .to_string(),
                    count: item
                        .pointer("/contentDetails/itemCount")
                        .and_then(serde_json::Value::as_u64),
                });
            }

            match json["nextPageToken"].as_str() {
                Some(next) => page = next.to_string(),
                None => break,
            }
        }

        Ok(playlists)
    }

    /// Tracks in a playlist, or the liked videos when given [`LIKED_ID`].
    pub fn tracks(&mut self, playlist_id: &str, limit: usize) -> Result<Vec<Track>> {
        if playlist_id == LIKED_ID {
            return self.liked(limit);
        }

        // Two calls per page, and both are needed. `playlistItems` is the only
        // thing that knows the item ids required to remove a row, but it does
        // not carry durations; `videos` carries durations but knows nothing
        // about playlists. Fifty ids go into one `videos` call, so this is two
        // requests per fifty tracks, not two per track.
        let mut tracks = Vec::new();
        let mut page = String::new();

        while tracks.len() < limit {
            let mut params = vec![
                ("part", "snippet,contentDetails"),
                ("playlistId", playlist_id),
                ("maxResults", "50"),
            ];
            if !page.is_empty() {
                params.push(("pageToken", page.as_str()));
            }
            let json = self.get("playlistItems", &params)?;

            // (video id, playlist item id, uploader) for each row, in order.
            let rows: Vec<(String, String, String)> = json["items"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|item| {
                    let video = item
                        .pointer("/contentDetails/videoId")
                        .and_then(|v| v.as_str())?;
                    let row = item["id"].as_str()?;
                    let owner = item
                        .pointer("/snippet/videoOwnerChannelTitle")
                        .or_else(|| item.pointer("/snippet/channelTitle"))
                        .and_then(|v| v.as_str())
                        .unwrap_or(UNKNOWN_ARTIST);
                    Some((video.to_string(), row.to_string(), owner.to_string()))
                })
                .collect();

            if rows.is_empty() {
                break;
            }

            let ids: Vec<&str> = rows.iter().map(|(video, _, _)| video.as_str()).collect();
            let details = self.videos(&ids)?;

            for (video, row, owner) in rows {
                // A row whose video did not come back is deleted, private, or
                // region-blocked. It shows in YouTube's own UI as "Deleted
                // video" and cannot be played, so it is dropped rather than
                // listed as something that will fail on Enter.
                let Some(found) = details.iter().find(|t| t.id == video) else {
                    continue;
                };
                tracks.push(Track {
                    uploader: if found.uploader == UNKNOWN_ARTIST {
                        owner
                    } else {
                        found.uploader.clone()
                    },
                    playlist_item_id: Some(row),
                    ..found.clone()
                });
                if tracks.len() >= limit {
                    break;
                }
            }

            match json["nextPageToken"].as_str() {
                Some(next) => page = next.to_string(),
                None => break,
            }
        }

        Ok(tracks)
    }

    /// Videos the user has liked.
    ///
    /// One call per page, not two: `myRating=like` returns full video resources
    /// already, durations included. There are no playlist item ids because
    /// there is no playlist -- unliking goes through [`Library::set_like`].
    fn liked(&mut self, limit: usize) -> Result<Vec<Track>> {
        let mut tracks = Vec::new();
        let mut page = String::new();

        while tracks.len() < limit {
            let mut params = vec![
                ("part", "snippet,contentDetails"),
                ("myRating", "like"),
                ("maxResults", "50"),
            ];
            if !page.is_empty() {
                params.push(("pageToken", page.as_str()));
            }
            let json = self.get("videos", &params)?;

            let before = tracks.len();
            tracks.extend(parse_videos(&json).into_iter().take(limit - before));
            if tracks.len() == before {
                break;
            }

            match json["nextPageToken"].as_str() {
                Some(next) => page = next.to_string(),
                None => break,
            }
        }

        Ok(tracks)
    }

    /// Looks up full details for up to [`PAGE`] video ids in one request.
    fn videos(&mut self, ids: &[&str]) -> Result<Vec<Track>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let joined = ids[..ids.len().min(PAGE)].join(",");
        let json = self.get(
            "videos",
            &[("part", "snippet,contentDetails"), ("id", joined.as_str())],
        )?;
        Ok(parse_videos(&json))
    }

    /// Adds a video to a playlist. Costs 50 quota units.
    pub fn add(&mut self, playlist_id: &str, video_id: &str) -> Result<()> {
        if playlist_id == LIKED_ID {
            // Liked songs is not a playlist and will not accept an insert.
            // Routing it to the rating endpoint is what the user meant anyway.
            return self.set_like(video_id, true);
        }

        let body = serde_json::json!({
            "snippet": {
                "playlistId": playlist_id,
                "resourceId": { "kind": "youtube#video", "videoId": video_id },
            }
        });
        self.write(reqwest::Method::POST, "playlistItems", &[("part", "snippet")], Some(body))?;
        Ok(())
    }

    /// Removes one row from a playlist.
    ///
    /// Takes the *playlist item* id, not the video id: the same video can sit
    /// in a playlist twice, and the API removes a row rather than a video.
    pub fn remove(&mut self, playlist_item_id: &str) -> Result<()> {
        self.write(
            reqwest::Method::DELETE,
            "playlistItems",
            &[("id", playlist_item_id)],
            None,
        )?;
        Ok(())
    }

    /// Reads the user's rating of a video, so a toggle can be a real toggle
    /// rather than two keys the user has to choose between. Costs 1 unit.
    pub fn is_liked(&mut self, video_id: &str) -> Result<bool> {
        let json = self.get("videos/getRating", &[("id", video_id)])?;
        Ok(json
            .pointer("/items/0/rating")
            .and_then(|r| r.as_str())
            .is_some_and(|r| r == "like"))
    }

    /// Likes or unlikes a video. Costs 50 quota units.
    pub fn set_like(&mut self, video_id: &str, like: bool) -> Result<()> {
        let rating = if like { "like" } else { "none" };
        self.write(
            reqwest::Method::POST,
            "videos/rate",
            &[("id", video_id), ("rating", rating)],
            None,
        )?;
        Ok(())
    }

    fn get(&mut self, path: &str, params: &[(&str, &str)]) -> Result<serde_json::Value> {
        let body = self.request(reqwest::Method::GET, path, params, None)?;
        serde_json::from_slice(&body)
            .with_context(|| format!("{path} returned something that is not JSON"))
    }

    /// A mutating call. Separate from [`Self::get`] only to name the intent at
    /// the call sites -- these are the ones that change the user's account.
    fn write(
        &mut self,
        method: reqwest::Method,
        path: &str,
        params: &[(&str, &str)],
        body: Option<serde_json::Value>,
    ) -> Result<Vec<u8>> {
        self.request(method, path, params, body)
    }

    fn request(
        &mut self,
        method: reqwest::Method,
        path: &str,
        params: &[(&str, &str)],
        body: Option<serde_json::Value>,
    ) -> Result<Vec<u8>> {
        // Split borrow: the token refresh needs the session mutably and the
        // HTTP client immutably, and they are two fields of the same struct.
        let Self { http, session } = self;
        let session = session
            .as_mut()
            .context("not signed in -- press A to sign in with Google")?;
        let token = session.access_token(http)?.to_string();

        let query = form_urlencoded::Serializer::new(String::new())
            .extend_pairs(params.iter())
            .finish();
        let url = format!("{API}/{path}?{query}");

        let mut request = http
            .client()
            .request(method, &url)
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"));
        if let Some(body) = body {
            let encoded = serde_json::to_vec(&body).context("could not encode the request")?;
            request = request
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(encoded);
        }

        let (status, raw) = http.send(request)?;
        // 204 on a DELETE, which has no body to parse and no error to report.
        if status == 204 {
            return Ok(Vec::new());
        }
        if !(200..300).contains(&status) {
            bail!("{}", explain(&raw, status));
        }
        Ok(raw)
    }
}

/// Turns a `videos.list` response into tracks.
fn parse_videos(json: &serde_json::Value) -> Vec<Track> {
    json["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            Some(Track {
                id: item["id"].as_str()?.to_string(),
                title: item
                    .pointer("/snippet/title")
                    .and_then(|t| t.as_str())
                    .unwrap_or("(untitled)")
                    .to_string(),
                uploader: item
                    .pointer("/snippet/channelTitle")
                    .and_then(|t| t.as_str())
                    .filter(|t| !t.is_empty())
                    .unwrap_or(UNKNOWN_ARTIST)
                    .to_string(),
                duration: item
                    .pointer("/contentDetails/duration")
                    .and_then(|d| d.as_str())
                    .and_then(parse_iso_duration),
                // The Data API has no album field. A video is not a release.
                album: None,
                playlist_item_id: None,
            })
        })
        .collect()
}

/// Turns a Data API failure into something a user can act on.
///
/// The generic message is close to useless on the two failures that actually
/// happen in practice, so both get named explicitly.
fn explain(body: &[u8], status: u16) -> String {
    let message = describe(body, status);
    let text = String::from_utf8_lossy(body);

    if status == 403 && text.contains("quotaExceeded") {
        return "Google API quota exhausted for today. It resets at midnight \
                Pacific time; you can raise the limit in the Cloud console."
            .to_string();
    }
    if status == 401 {
        return format!("{message} -- the sign-in was rejected; press A to sign in again");
    }
    message
}

/// Parses an ISO 8601 duration as the Data API writes it: `PT3M47S`, `PT1H2M5S`,
/// or `P0D` for a live stream.
///
/// Hand-rolled rather than pulled in as a dependency. This is the only place in
/// the program that needs it, the grammar in play is a handful of digit-letter
/// pairs, and a date-time crate is a large thing to link for one field.
fn parse_iso_duration(text: &str) -> Option<Duration> {
    let body = text.strip_prefix('P')?;
    let (date, time) = body.split_once('T').unwrap_or((body, ""));

    let mut secs: u64 = 0;
    let mut digits = String::new();
    let mut accumulate = |part: &str, units: &[(char, u64)]| -> Option<()> {
        for c in part.chars() {
            if c.is_ascii_digit() {
                digits.push(c);
                continue;
            }
            let value: u64 = digits.parse().ok()?;
            digits.clear();
            let (_, scale) = units.iter().find(|(unit, _)| *unit == c)?;
            secs = secs.checked_add(value.checked_mul(*scale)?)?;
        }
        // A trailing number with no unit is malformed, not a count of seconds.
        digits.is_empty().then_some(())
    };

    accumulate(date, &[('D', 86_400), ('W', 604_800)])?;
    accumulate(time, &[('H', 3_600), ('M', 60), ('S', 1)])?;

    // `P0D` is what a live stream reports. Zero is not a length, and the UI
    // already renders `None` as LIVE.
    (secs > 0).then(|| Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_durations_the_api_sends() {
        assert_eq!(parse_iso_duration("PT3M47S"), Some(Duration::from_secs(227)));
        assert_eq!(parse_iso_duration("PT1H2M5S"), Some(Duration::from_secs(3725)));
        assert_eq!(parse_iso_duration("PT45S"), Some(Duration::from_secs(45)));
        assert_eq!(parse_iso_duration("PT2H"), Some(Duration::from_secs(7200)));
        // A DJ set long enough to cross a day boundary.
        assert_eq!(parse_iso_duration("P1DT1H"), Some(Duration::from_secs(90_000)));
    }

    #[test]
    fn a_live_stream_has_no_duration() {
        // `P0D` is what the API reports while a stream is running, and the UI
        // renders a missing duration as LIVE.
        assert_eq!(parse_iso_duration("P0D"), None);
        assert_eq!(parse_iso_duration("PT0S"), None);
    }

    #[test]
    fn rejects_things_that_are_not_durations() {
        assert_eq!(parse_iso_duration(""), None);
        assert_eq!(parse_iso_duration("3:47"), None);
        // A number with no unit after it.
        assert_eq!(parse_iso_duration("PT3M4"), None);
        assert_eq!(parse_iso_duration("PT3X"), None);
    }

    fn video(id: &str, title: &str, channel: &str, duration: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "snippet": { "title": title, "channelTitle": channel },
            "contentDetails": { "duration": duration }
        })
    }

    #[test]
    fn reads_a_videos_response() {
        let json = serde_json::json!({
            "items": [ video("JhulBGMA7G4", "Harder Better Faster", "Daft Punk", "PT3M47S") ]
        });
        let tracks = parse_videos(&json);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].id, "JhulBGMA7G4");
        assert_eq!(tracks[0].uploader, "Daft Punk");
        assert_eq!(tracks[0].duration, Some(Duration::from_secs(227)));
        // Only a playlist listing knows the row id; this is not one.
        assert!(tracks[0].playlist_item_id.is_none());
        // The Data API has no album field at all.
        assert!(tracks[0].album.is_none());
    }

    #[test]
    fn skips_rows_without_an_id_rather_than_failing() {
        // An unrecognised or partial row should cost one track, not the page.
        let json = serde_json::json!({
            "items": [
                { "snippet": { "title": "no id here" } },
                video("abcdefghijk", "Real", "Someone", "PT1M"),
            ]
        });
        let tracks = parse_videos(&json);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].id, "abcdefghijk");
    }

    #[test]
    fn an_empty_response_yields_no_tracks() {
        assert!(parse_videos(&serde_json::json!({})).is_empty());
        assert!(parse_videos(&serde_json::json!({ "items": [] })).is_empty());
    }

    #[test]
    fn a_video_with_no_channel_falls_back_to_the_placeholder() {
        // Which `Track::label` then suppresses, rather than printing "unknown".
        let json = serde_json::json!({ "items": [ video("x", "Title", "", "PT1M") ] });
        let tracks = parse_videos(&json);
        assert_eq!(tracks[0].uploader, UNKNOWN_ARTIST);
        assert_eq!(tracks[0].label(), "Title");
    }

    #[test]
    fn quota_exhaustion_is_named_rather_than_echoed() {
        let body = br#"{"error":{"code":403,"message":"The request cannot be completed because you have exceeded your quota.","errors":[{"reason":"quotaExceeded"}]}}"#;
        assert!(explain(body, 403).contains("quota exhausted"));
    }

    #[test]
    fn an_expired_session_says_how_to_fix_it() {
        let body = br#"{"error":{"code":401,"message":"Invalid Credentials"}}"#;
        let explained = explain(body, 401);
        assert!(explained.contains("Invalid Credentials"));
        assert!(explained.contains("sign in again"));
    }

    #[test]
    fn the_liked_pseudo_playlist_cannot_collide_with_a_real_id() {
        // Real playlist ids are alphanumeric plus `-` and `_`.
        assert!(LIKED_ID.contains('@'));
        let liked = Playlist {
            id: LIKED_ID.to_string(),
            title: "Liked songs".into(),
            count: None,
        };
        assert!(liked.is_liked());
    }
}
