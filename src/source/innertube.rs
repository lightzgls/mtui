//! Direct resolution through YouTube's own player API.
//!
//! This is the fast path. `yt-dlp` is a compatibility layer for 1800+ sites and
//! charges for all of it on every play: ~1.3 s to boot Python and import the
//! machinery before a single byte of network, then a fresh process with no
//! warm connection, and possibly a JavaScript runtime to solve the nsig
//! challenge. Measured end to end at ~4.2 s.
//!
//! Asking YouTube's own client API the same question takes ~0.2 s, because it
//! is one HTTPS POST on a pooled connection. The clients in [`CLIENTS`] are
//! chosen because they are served plain `url` fields rather than
//! `signatureCipher`, so nothing has to be deciphered and no JS runtime is
//! involved.
//!
//! The catch, and the reason [`crate::source::youtube`] is still here: this is
//! an internal API. It can change without notice, and it refuses things yt-dlp
//! handles -- age-gated, region-locked, or PO-token-gated videos. So every
//! failure here falls back to yt-dlp rather than surfacing. The worst case is
//! exactly the old behaviour plus one cheap round trip.

use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::youtube::parse_expiry;
use super::{StreamUrl, Track};

/// itag 140 -- AAC-LC in MP4, the same format the yt-dlp path requests.
const AUDIO_ITAG: u32 = 140;

/// Kept short on purpose. This is a speculative fast path; if it is not clearly
/// winning, the fallback is right there and will do the job properly.
const TIMEOUT: Duration = Duration::from_secs(5);

const PLAYER_URL: &str = "https://www.youtube.com/youtubei/v1/player";

/// Where a capped URL stops being served. Measured, not documented: every
/// capped URL seen answers ranges below one mebibyte and 403s from there on.
const CAP_BYTES: u64 = 1024 * 1024;

/// YouTube *Music*'s search, which is a different corpus to YouTube's.
///
/// Plain `ytsearch` returns whatever the site has: reaction videos, hour-long
/// mix compilations, full concert uploads, duplicate reuploads from channels
/// that do not own the track. This endpoint returns songs, with the artist and
/// album as structured fields rather than guesswork on a title string.
const SEARCH_URL: &str = "https://music.youtube.com/youtubei/v1/search";

/// The "Songs" search filter, base64 protobuf as YouTube Music's own client
/// sends it. Without it the response also carries albums, artists, playlists
/// and podcast episodes -- none of which we can hand to a decoder.
const SONGS_FILTER: &str = "EgWKAQIIAWoKEAoQCRADEAQQBQ%3D%3D";

const MUSIC_CLIENT_NAME: &str = "WEB_REMIX";
const MUSIC_CLIENT_VERSION: &str = "1.20241127.01.00";

/// A player client identity. These values travel together -- mismatching a
/// name, version and user agent is how a request starts getting refused.
struct PlayerClient {
    name: &'static str,
    version: &'static str,
    user_agent: &'static str,
    device_model: Option<&'static str>,
    android_sdk: Option<u32>,
}

/// Clients to try, in order, before giving up and letting yt-dlp answer.
///
/// Order is not arbitrary and is worth keeping measured. YouTube Music serves
/// most songs as "art track" uploads, and those are refused with
/// `LOGIN_REQUIRED` by every client tried except `IOS` -- which also handles
/// ordinary videos, so it goes first. Since our search returns exactly those
/// art tracks, getting this wrong sends every single play down the slow path
/// without anything appearing to be broken.
const CLIENTS: &[PlayerClient] = &[
    PlayerClient {
        name: "IOS",
        version: "20.10.4",
        user_agent: "com.google.ios.youtube/20.10.4 (iPhone16,2; U; CPU iOS 18_3_2 like Mac OS X;)",
        device_model: Some("iPhone16,2"),
        android_sdk: None,
    },
    // A cheap second try: it cannot play art tracks, but it costs one round
    // trip and is a different code path at YouTube's end if IOS starts being
    // refused.
    PlayerClient {
        name: "ANDROID_VR",
        version: "1.62.27",
        user_agent: "com.google.android.apps.youtube.vr.oculus/1.62.27 \
                     (Linux; U; Android 12L; eureka-user Build/SQ3A.220605.009.A1) gzip",
        device_model: None,
        android_sdk: Some(32),
    },
];

/// Fields consumed from the player response. YouTube returns far more; serde
/// drops the rest rather than allocating it.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlayerResponse {
    #[serde(default)]
    playability_status: PlayabilityStatus,
    #[serde(default)]
    streaming_data: StreamingData,
}

#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct PlayabilityStatus {
    #[serde(default)]
    status: String,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct StreamingData {
    #[serde(default)]
    adaptive_formats: Vec<Format>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Format {
    itag: u32,
    /// Absent when YouTube ciphered the URL instead. Such a format is unusable
    /// here by definition -- deciphering is the expensive thing being avoided.
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    mime_type: Option<String>,
}

pub struct InnerTube {
    client: reqwest::Client,
    /// Held rather than built per call, so the connection pool and TLS session
    /// survive between tracks. That is most of the difference between a ~0.8 s
    /// cold call and a ~0.2 s warm one.
    runtime: tokio::runtime::Runtime,
}

impl InnerTube {
    pub fn new() -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("could not start a runtime for the player API")?;
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .build()
            .context("could not build the player API client")?;
        Ok(Self { client, runtime })
    }

    /// Resolves a video id to a directly streamable audio URL.
    ///
    /// Errors are ordinary here rather than exceptional: callers are expected
    /// to fall back to yt-dlp, not to report this to the user.
    pub fn resolve(&self, video_id: &str) -> Result<StreamUrl> {
        let mut last = None;
        for client in CLIENTS {
            match self.resolve_with(client, video_id) {
                Ok(stream) => return Ok(stream),
                // Keep going: a refusal from one client says nothing about the
                // next, and the whole cascade is still far cheaper than yt-dlp.
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("no player clients are configured")))
    }

    fn resolve_with(&self, client: &PlayerClient, video_id: &str) -> Result<StreamUrl> {
        let mut context = serde_json::json!({
            "clientName": client.name,
            "clientVersion": client.version,
            "hl": "en",
        });
        // Only sent when the client actually has one. An Android SDK level on
        // an iPhone is the kind of mismatch that gets a request refused.
        if let Some(model) = client.device_model {
            context["deviceModel"] = model.into();
        }
        if let Some(sdk) = client.android_sdk {
            context["androidSdkVersion"] = sdk.into();
        }

        let body = serde_json::json!({
            "videoId": video_id,
            "context": { "client": context },
        });

        // Serialised here rather than via `RequestBuilder::json`, which would
        // mean turning on reqwest's `json` feature for a convenience this file
        // needs exactly once. serde_json is already a direct dependency.
        let body = serde_json::to_vec(&body).context("could not encode the player API request")?;

        let raw = self.runtime.block_on(async {
            self.client
                .post(PLAYER_URL)
                .header(reqwest::header::USER_AGENT, client.user_agent)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body)
                .send()
                .await?
                .error_for_status()?
                .bytes()
                .await
        })?;

        let response: PlayerResponse =
            serde_json::from_slice(&raw).context("player API returned unexpected JSON")?;

        // "OK" is the only status that carries playable formats. Anything else
        // (LOGIN_REQUIRED, UNPLAYABLE, AGE_VERIFICATION_REQUIRED) is precisely
        // the case the yt-dlp fallback exists for.
        if response.playability_status.status != "OK" {
            bail!(
                "{} refused {video_id}: {} ({})",
                client.name,
                response.playability_status.status,
                response.playability_status.reason.as_deref().unwrap_or("no reason given")
            );
        }

        let url = pick_audio(&response.streaming_data.adaptive_formats)
            .with_context(|| format!("player API returned no usable audio for {video_id}"))?;

        if !self.serves_whole_file(url) {
            bail!("{} returned a capped URL for {video_id}", client.name);
        }

        Ok(StreamUrl {
            expires_at: parse_expiry(url),
            url: url.to_string(),
        })
    }

    /// Whether YouTube will serve all of `url`, or only the start of it.
    ///
    /// A resolve that succeeds is not the same thing as a URL that plays. For
    /// some tracks -- licensed music, in every case seen here -- the player API
    /// answers with a URL that serves its first mebibyte and then refuses every
    /// range past it with a 403. Nothing in the response says so, and the first
    /// mebibyte plays: about sixty-five seconds of audio, after which the sound
    /// stops mid-song. By then the resolve is a minute in the past, the decoder
    /// reports only that it has no more samples, and what the user sees is a
    /// track that stopped for no reason.
    ///
    /// yt-dlp's URL for the same video has no such cap, so this is worth
    /// catching -- and a failure here is exactly what makes the caller fall
    /// back to it, the same as any other refusal.
    ///
    /// One request for one byte, on a pooled connection, against a fast path
    /// that already costs a round trip: a few tens of milliseconds to avoid
    /// three seconds of yt-dlp when the URL is good, and a minute of music that
    /// dies when it is not.
    fn serves_whole_file(&self, url: &str) -> bool {
        let last = match content_length(url) {
            // Under the cap there is nothing past it to be refused.
            Some(clen) if clen <= CAP_BYTES => return true,
            Some(clen) => clen - 1,
            // No length to aim at, so ask at the cap itself. A file that ends
            // before it answers 416, which is not a refusal.
            None => CAP_BYTES,
        };

        self.runtime.block_on(async {
            let sent = self
                .client
                .get(url)
                .header(reqwest::header::RANGE, format!("bytes={last}-{last}"))
                .send()
                .await;
            match sent {
                Ok(response) => response.status() != reqwest::StatusCode::FORBIDDEN,
                // A URL that cannot be reached at all is not a capped one, and
                // the stream is about to find that out for itself. Refusing it
                // here would send every resolve to yt-dlp the moment the
                // network hiccups.
                Err(_) => true,
            }
        })
    }

    /// Searches YouTube Music for songs.
    ///
    /// Same bargain as [`Self::resolve`]: fast and much better targeted, but an
    /// internal API, so a caller is expected to fall back to yt-dlp rather than
    /// report a failure.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Track>> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }

        let body = serde_json::json!({
            "query": query,
            "params": SONGS_FILTER,
            "context": {
                "client": {
                    "clientName": MUSIC_CLIENT_NAME,
                    "clientVersion": MUSIC_CLIENT_VERSION,
                    "hl": "en",
                }
            }
        });
        let body = serde_json::to_vec(&body).context("could not encode the search request")?;

        let raw = self.runtime.block_on(async {
            self.client
                .post(SEARCH_URL)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body)
                .send()
                .await?
                .error_for_status()?
                .bytes()
                .await
        })?;

        let json: serde_json::Value =
            serde_json::from_slice(&raw).context("search returned unexpected JSON")?;

        let tracks = parse_search(&json, limit);
        if tracks.is_empty() {
            bail!("search returned nothing usable");
        }
        Ok(tracks)
    }
}

/// Walks the search response, which is a deeply nested render tree.
///
/// Navigated as untyped JSON on purpose. Mirroring this shape as structs would
/// be some two hundred lines describing a layout YouTube reserves the right to
/// change, and every one of those lines would be a way for a rename to become a
/// hard failure. Here, anything unrecognised simply yields no tracks, which the
/// caller reads as "fall back to yt-dlp".
fn parse_search(json: &serde_json::Value, limit: usize) -> Vec<Track> {
    let sections = json
        .pointer("/contents/tabbedSearchResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents")
        .and_then(serde_json::Value::as_array);
    let Some(sections) = sections else {
        return Vec::new();
    };

    sections
        .iter()
        .filter_map(|section| {
            section
                .pointer("/musicShelfRenderer/contents")
                .and_then(serde_json::Value::as_array)
        })
        .flatten()
        .filter_map(|item| parse_track(&item["musicResponsiveListItemRenderer"]))
        .take(limit)
        .collect()
}

fn parse_track(item: &serde_json::Value) -> Option<Track> {
    // Absent on rows that are not playable individually, such as an album
    // header that shares the shelf.
    let id = item.pointer("/playlistItemData/videoId")?.as_str()?.to_string();

    let title = flex_column(item, 0)?;
    // Second column is "Artist • Album • Duration", already joined for display
    // by YouTube. Splitting it back apart is more reliable than trusting each
    // run to be exactly one field.
    let details = flex_column(item, 1).unwrap_or_default();
    let fields: Vec<&str> = details.split('•').map(str::trim).collect();

    let uploader = fields.first().filter(|s| !s.is_empty()).copied();
    // Counted from the end, never by fixed index: singles carry no album, and
    // an artist credit can itself contain a separator.
    let duration = fields.last().copied().and_then(parse_duration);
    let album = (fields.len() >= 3)
        .then(|| fields[fields.len() - 2])
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    Some(Track {
        id,
        title,
        uploader: uploader.unwrap_or(super::UNKNOWN_ARTIST).to_string(),
        duration,
        album,
        // A search result is not a playlist row.
        playlist_item_id: None,
    })
}

/// Joins the text runs of one display column into a single string.
fn flex_column(item: &serde_json::Value, index: usize) -> Option<String> {
    let runs = item
        .pointer(&format!(
            "/flexColumns/{index}/musicResponsiveListItemFlexColumnRenderer/text/runs"
        ))?
        .as_array()?;

    let text: String = runs
        .iter()
        .filter_map(|run| run["text"].as_str())
        .collect();
    (!text.is_empty()).then_some(text)
}

/// Parses `M:SS` or `H:MM:SS` into a duration. Anything else is `None`, which
/// the UI renders as `LIVE`.
fn parse_duration(text: &str) -> Option<Duration> {
    let mut secs: u64 = 0;
    let mut fields = 0;
    for field in text.trim().split(':') {
        secs = secs.checked_mul(60)?.checked_add(field.trim().parse().ok()?)?;
        fields += 1;
    }
    (2..=3).contains(&fields).then(|| Duration::from_secs(secs))
}

/// Picks the audio stream to play: itag 140 if offered, otherwise any
/// unciphered audio-only MP4.
///
/// The fallback matters because the itag on offer varies by client and video;
/// insisting on 140 alone would send perfectly playable videos down the slow
/// path. Anything chosen here must still be AAC in MP4, since that is the only
/// decoder compiled in.
/// The file's size, which googlevideo states in the URL it signs as `clen`.
fn content_length(url: &str) -> Option<u64> {
    url.split(['?', '&'])
        .find_map(|kv| kv.strip_prefix("clen="))?
        .parse()
        .ok()
}

fn pick_audio(formats: &[Format]) -> Option<&str> {
    let usable = |f: &Format| {
        f.mime_type
            .as_deref()
            .is_some_and(|m| m.starts_with("audio/mp4"))
    };

    formats
        .iter()
        .find(|f| f.itag == AUDIO_ITAG && f.url.is_some())
        .or_else(|| formats.iter().find(|f| f.url.is_some() && usable(f)))
        .and_then(|f| f.url.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn format(itag: u32, url: Option<&str>, mime: &str) -> Format {
        Format {
            itag,
            url: url.map(str::to_string),
            mime_type: Some(mime.to_string()),
        }
    }

    #[test]
    fn prefers_itag_140() {
        let formats = vec![
            format(251, Some("opus"), "audio/webm; codecs=\"opus\""),
            format(140, Some("aac"), "audio/mp4; codecs=\"mp4a.40.2\""),
        ];
        assert_eq!(pick_audio(&formats), Some("aac"));
    }

    #[test]
    fn falls_back_to_any_unciphered_mp4_audio() {
        let formats = vec![format(139, Some("aac-low"), "audio/mp4; codecs=\"mp4a.40.5\"")];
        assert_eq!(pick_audio(&formats), Some("aac-low"));
    }

    #[test]
    fn skips_ciphered_formats() {
        // No `url` means YouTube sent a signatureCipher instead; deciphering is
        // the whole cost this path exists to avoid.
        let formats = vec![format(140, None, "audio/mp4")];
        assert_eq!(pick_audio(&formats), None);
    }

    #[test]
    fn rejects_formats_we_cannot_decode() {
        // Opus is not compiled in, so an Opus-only response must fall through
        // to yt-dlp rather than be handed to the decoder.
        let formats = vec![format(251, Some("opus"), "audio/webm; codecs=\"opus\"")];
        assert_eq!(pick_audio(&formats), None);
    }

    #[test]
    fn ignores_video_formats() {
        let formats = vec![format(137, Some("video"), "video/mp4; codecs=\"avc1\"")];
        assert_eq!(pick_audio(&formats), None);
    }

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration("3:47"), Some(Duration::from_secs(227)));
        assert_eq!(parse_duration(" 9:05 "), Some(Duration::from_secs(545)));
        assert_eq!(parse_duration("1:02:05"), Some(Duration::from_secs(3725)));
    }

    #[test]
    fn rejects_things_that_are_not_durations() {
        // A bare number is a play count or a year, not a length.
        assert_eq!(parse_duration("2013"), None);
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("Random Access Memories"), None);
        assert_eq!(parse_duration("1:2:3:4"), None);
    }

    /// Rebuilds the shape of one search row, so the pointer paths above are
    /// covered without a network call.
    fn row(id: &str, title: &str, details: &str) -> serde_json::Value {
        let column = |text: &str| {
            serde_json::json!({
                "musicResponsiveListItemFlexColumnRenderer": {
                    "text": { "runs": [ { "text": text } ] }
                }
            })
        };
        serde_json::json!({
            "playlistItemData": { "videoId": id },
            "flexColumns": [ column(title), column(details) ]
        })
    }

    #[test]
    fn reads_a_song_row() {
        let track = parse_track(&row(
            "JhulBGMA7G4",
            "Harder, Better, Faster, Stronger",
            "Daft Punk • Discovery • 3:47",
        ))
        .expect("row should parse");

        assert_eq!(track.id, "JhulBGMA7G4");
        assert_eq!(track.title, "Harder, Better, Faster, Stronger");
        assert_eq!(track.uploader, "Daft Punk");
        assert_eq!(track.album.as_deref(), Some("Discovery"));
        assert_eq!(track.duration, Some(Duration::from_secs(227)));
    }

    #[test]
    fn reads_a_single_with_no_album() {
        // Singles drop the album, so the duration moves column. Taking both
        // from the end rather than a fixed index is what makes this work.
        let track = parse_track(&row("abc", "Da Funk", "Daft Punk • 5:35")).expect("should parse");
        assert_eq!(track.uploader, "Daft Punk");
        assert_eq!(track.album, None);
        assert_eq!(track.duration, Some(Duration::from_secs(335)));
    }

    #[test]
    fn reads_an_album_past_a_multi_part_artist_credit() {
        // A collaboration puts a separator inside the artist field, so a
        // fixed index would mistake half the credit for the album.
        let track = parse_track(&row(
            "xyz",
            "Get Lucky",
            "Daft Punk • Pharrell Williams • Random Access Memories • 6:10",
        ))
        .expect("should parse");
        assert_eq!(track.album.as_deref(), Some("Random Access Memories"));
        assert_eq!(track.duration, Some(Duration::from_secs(370)));
    }

    #[test]
    fn labels_carry_the_artist_but_never_a_placeholder() {
        let track = parse_track(&row("a", "Veridis Quo", "Daft Punk • Discovery • 5:46")).unwrap();
        assert_eq!(track.label(), "Veridis Quo — Daft Punk");

        // Second column missing entirely: better a bare title than "— unknown".
        let mut bare = row("b", "Mystery Track", "");
        bare["flexColumns"][1] = serde_json::Value::Null;
        assert_eq!(parse_track(&bare).unwrap().label(), "Mystery Track");
    }

    #[test]
    fn skips_rows_that_are_not_playable() {
        // No videoId: an album or artist header sharing the shelf.
        let mut item = row("x", "Discovery", "Album • Daft Punk");
        item["playlistItemData"] = serde_json::Value::Null;
        assert!(parse_track(&item).is_none());
    }

    #[test]
    fn unrecognised_response_yields_nothing() {
        // Which is how the caller knows to fall back to yt-dlp.
        assert!(parse_search(&serde_json::json!({}), 20).is_empty());
        assert!(parse_search(&serde_json::json!({ "contents": 5 }), 20).is_empty());
    }

    /// Confirms the fast path is both working and actually fast against the
    /// live API, and that a warm connection is cheaper than a cold one.
    ///
    /// Ignored by default: needs the network, and it is checking a third party
    /// we do not control. A failure here is a signal to look at whether the
    /// client identity above still satisfies YouTube -- not a broken build,
    /// since every failure falls back to yt-dlp.
    #[test]
    #[ignore = "hits the live YouTube player API"]
    fn resolves_against_the_live_api() {
        use std::time::Instant;

        let tube = InnerTube::new().expect("client should build");

        // Both kinds matter. The second is a YouTube Music "art track", which
        // is what our own search returns and which most player clients refuse
        // outright -- so a regression here would quietly send every play from
        // the search results back to the 4 s yt-dlp path.
        for (id, kind) in [("dQw4w9WgXcQ", "ordinary video"), ("JhulBGMA7G4", "art track")] {
            let start = Instant::now();
            match tube.resolve(id) {
                Ok(stream) => {
                    println!("{kind:<15} {id}: {:.2}s", start.elapsed().as_secs_f64());
                    assert!(stream.url.starts_with("https://"), "expected an https URL");
                    assert!(stream.is_valid(), "a freshly resolved URL should be valid");
                }
                Err(e) => panic!("{kind} {id} failed: {e:#}"),
            }
        }
    }

    /// Checks the live search returns songs rather than whatever YouTube has.
    ///
    /// Ignored for the same reason as the resolver's live test: it depends on a
    /// third party, and a failure means the fallback took over, not that the
    /// build is broken.
    #[test]
    #[ignore = "hits the live YouTube Music API"]
    fn searches_against_the_live_api() {
        use std::time::Instant;

        let tube = InnerTube::new().expect("client should build");
        let start = Instant::now();
        let tracks = tube.search("daft punk", 10).expect("search failed");
        println!("search: {:.2}s, {} tracks", start.elapsed().as_secs_f64(), tracks.len());

        assert!(!tracks.is_empty(), "expected some songs");
        for track in &tracks {
            // `{:.N}` truncates by characters, not bytes, so a multi-byte
            // title cannot be sliced mid-codepoint here.
            println!(
                "  {:<36.34} {:<30.28} {:<28.26} {:>8}",
                track.title,
                track.uploader,
                track.album.as_deref().unwrap_or("-"),
                track.duration_str()
            );
            assert_eq!(track.id.len(), 11, "every row should carry a video id");
            assert!(!track.title.is_empty());
            // Songs have a length; a row without one is a live stream, which
            // the songs filter should have excluded.
            assert!(track.duration.is_some(), "{} has no duration", track.title);
        }
    }
}


#[cfg(test)]
mod capped_urls {
    use super::*;

    #[test]
    fn reads_the_file_size_the_url_states() {
        assert_eq!(
            content_length("https://x.googlevideo.com/videoplayback?itag=140&clen=3474230&dur=214"),
            Some(3_474_230)
        );
        // A livestream states no length, and neither does a malformed one.
        assert_eq!(content_length("https://x.googlevideo.com/videoplayback?itag=140"), None);
        assert_eq!(content_length("https://x.googlevideo.com/videoplayback?clen=soon"), None);
    }

    /// Whatever the app ends up playing must serve its last byte, not just its
    /// first mebibyte.
    ///
    /// The default track is the one that was reported as stopping mid-song. The
    /// player API answers for it with a URL capped at a mebibyte, so it has to
    /// come back refused and be picked up by yt-dlp -- which is the whole
    /// cascade, and why this asserts against the URL the app would really
    /// stream rather than against either path alone.
    ///
    /// `cargo test --release serves_the_whole_file -- --ignored --nocapture`
    #[test]
    #[ignore = "hits the network"]
    fn serves_the_whole_file() {
        let id = std::env::var("MTUI_VIDEO_ID").unwrap_or_else(|_| "nNN88hijp-o".into());
        let tube = InnerTube::new().expect("client should build");
        let stream = tube
            .resolve(&id)
            .inspect_err(|e| println!("player API declined, falling back: {e:#}"))
            .or_else(|_| crate::source::youtube::YouTube::default().resolve(&id))
            .expect("neither the player API nor yt-dlp resolved the track");

        let clen = content_length(&stream.url).expect("a signed URL states its length");
        let last = clen - 1;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");

        let status = runtime.block_on(async {
            reqwest::Client::new()
                .get(&stream.url)
                .header(reqwest::header::RANGE, format!("bytes={last}-{last}"))
                .send()
                .await
                .expect("request failed")
                .status()
        });

        assert!(
            status.is_success(),
            "resolved URL will not serve its last byte: HTTP {status}"
        );
    }
}
