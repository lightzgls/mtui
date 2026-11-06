//! Nonblocking source polling and application of correlated worker responses.

use std::time::{Duration, Instant};

use anyhow::Result;

use crate::player::Command;
use crate::source::youtube::watch::Watch;
use crate::source::worker::{Request, Response};

use super::App;
use crate::app::browse::navigation::{Mode, View};
use crate::app::playback::now_playing::{NowPlaying, Panel, Tab};

/// Turns a panel fetch into what the panel should show.
///
/// A failure becomes [`Panel::Empty`] rather than a status-bar error on
/// purpose: the message here is almost always a fact about the track -- it has
/// no lyrics, its comments are turned off -- and it belongs in the panel that
/// is otherwise blank rather than on top of what is playing.
pub(super) fn panel<T>(fetched: Result<T, String>) -> Panel<T> {
    match fetched {
        Ok(value) => Panel::Ready(value),
        Err(msg) => Panel::Empty(msg),
    }
}

impl App {

    /// Drains completed source work. Called once per frame.
    ///
    /// Drains rather than taking one: two threads now feed the response queue,
    /// so a cover and a resolve can land in the same frame and handling only
    /// the first would leave the other a frame stale.
    pub fn poll_source(&mut self) -> bool {
        let mut changed = false;
        self.poll_update();
        if self.volume_save.is_some_and(|(at, _)| at.elapsed() >= Duration::from_millis(250)) {
            self.flush_volume();
        }
        while let Some(response) = self.source.poll() {
            changed = true;
            self.apply(response);
        }
        self.poll_player();
        self.tick_session();
        changed
    }


    pub(super) fn apply(&mut self, response: Response) {
        let session_generation = match &response {
            Response::Library { generation, .. } => Some(*generation),
            Response::CookiesImported { generation, .. }
            | Response::MusicSignInFailed { generation, .. }
            | Response::SessionRenewed { generation, .. } => Some(*generation),
            _ => None,
        };
        if session_generation.is_some_and(|generation| generation != self.source.session_generation()) {
            return;
        }
        let page_request = match &response {
            Response::Results { request_id, .. }
            | Response::Browsed { request_id, .. }
            | Response::Artist { request_id, .. } => Some(*request_id),
            _ => None,
        };
        if let Some(request_id) = page_request {
            if !self.accept_page_response(request_id) {
                return;
            }
            // A contextual menu is a snapshot of the page being replaced.
            self.close_page_actions();
        }

        match response {
            Response::Library { request_id, generation, category, page } => self.apply_library(request_id, generation, category, *page),
            Response::Rating { request_id, video_id, changed, result } => self.apply_rating(request_id, &video_id, changed, result),
            Response::Playlists { request_id, choices } => self.apply_playlists(request_id, choices),
            Response::PlaylistSaved { request_id, result } => self.apply_playlist_saved(request_id, result),
            Response::Results { tracks, .. } => {
                let tracks = match tracks {
                    Ok(tracks) => tracks,
                    Err(reason) => {
                        self.report(reason);
                        return;
                    }
                };
                if let Some(origin) = self.search_page.take() {
                    self.push_page(origin);
                }
                self.status.clear();
                self.browsing = None;
                self.browsing_endpoint = None;
                self.collection = None;
                self.collection_error = None;
                self.view = View::Tracks;
                self.results = tracks.iter().filter_map(|item| item.track.clone()).collect();
                self.search_items = tracks;
                self.selected = 0;
                self.offset = 0;
                // Start the debounce on the top hit: it is what Enter plays
                // most of the time, so it is the one worth having warm.
                self.prefetch.selection_changed(Instant::now());
                // Results are the point of a search, including an honest empty
                // result. Ending the edit here makes one Back restore its origin.
                self.mode = Mode::Browse;
            }
            Response::Home { generation, shelves } => self.receive_home(generation, shelves),
            Response::HomeFailed { generation } => self.receive_home_failure(generation),
            Response::HomeMore { generation, shelves } => self.extend_home(generation, shelves),
            Response::CookiesImported { browser, .. } => self.apply_session_import(browser),
            Response::SessionRenewed { result, .. } => self.apply_session_renewal(result),
            Response::MusicSignInFailed { message, .. } => self.apply_sign_in_failure(message),
            Response::Browsed {
                title,
                endpoint,
                page,
                ..
            } => {
                if self.browsing_endpoint.as_ref() != Some(&endpoint) {
                    return;
                }
                let page = match *page {
                    Ok(page) => page,
                    Err(reason) => {
                        self.collection_error = Some(reason.clone());
                        self.report(reason);
                        return;
                    }
                };
                let tracks = page.tracks;
                self.collection = Some(page.details);
                self.collection_error = None;
                if self.view == View::Tracks {
                    self.status.clear();
                }
                self.results = tracks;
                self.selected = 0;
                self.offset = 0;
                self.browsing = Some(title);
                self.browsing_endpoint = Some(endpoint);
                self.prefetch.selection_changed(Instant::now());
            }
            Response::Artist {
                browse_id, page, ..
            } => {
                let Some(artist) = self.artist.as_mut() else {
                    return;
                };
                if artist.requested.endpoint.browse_id != browse_id {
                    return;
                }
                artist.set_page(*page);
                let status = match &artist.page {
                    Panel::Ready(_) => String::new(),
                    Panel::Empty(reason) => reason.clone(),
                    Panel::Idle | Panel::Loading => String::new(),
                };
                if self.view == View::Artist {
                    self.status = status;
                }
                self.prefetch.selection_changed(Instant::now());
            }
            Response::ArtistSongs { request_id, browse_id, page } => {
                self.apply_artist_songs(request_id, &browse_id, *page);
            }
            Response::Resolved { id, title, stream } => self.apply_resolved(&id, title, stream),
            Response::Replacement { id, stream } => {
                if self.playback.listening_track().is_some_and(|track| track.id == id)
                    && let Ok(stream) = stream
                {
                    let command = match self.playback.take_recovery(&id) {
                        Some(from) => {
                            self.status.clear();
                            Command::Resume {
                                url: stream.url,
                                format: stream.format,
                                from,
                            }
                        }
                        None => Command::ArmReplacement {
                            id,
                            url: stream.url,
                            format: stream.format,
                        },
                    };
                    let _ = self.player.send(command);
                }
            }
            Response::Watch { video_id, watch } => self.apply_watch(&video_id, *watch),
            Response::MoreQueue { epoch, page } => self.apply_more_queue(epoch, page),
            Response::Lyrics { video_id, lyrics } => {
                if let Some(now) = self.for_track(&video_id) {
                    // A transient miss must neither erase words already on
                    // screen nor downgrade timings that have already arrived.
                    let already_synced = matches!(&now.lyrics, Panel::Ready(lyrics) if !lyrics.timed.is_empty());
                    if !already_synced {
                        if lyrics.is_ok() || !matches!(&now.lyrics, Panel::Ready(_)) {
                            now.lyrics = panel(lyrics);
                        }
                        let synced = matches!(&now.lyrics, Panel::Ready(lyrics) if !lyrics.timed.is_empty());
                        now.lyrics_retry_at = (!synced && now.lyrics_attempts < 3)
                            .then(|| Instant::now() + Duration::from_secs(if now.lyrics_attempts <= 1 { 2 } else { 8 }));
                    } else {
                        now.lyrics_retry_at = None;
                    }
                }
            }
            Response::Related { video_id, related } => {
                if let Some(now) = self.for_track(&video_id) {
                    now.related = panel(related);
                }
            }
            Response::Comments { video_id, comments } => {
                if let Some(now) = self.for_track(&video_id) {
                    now.comments = panel(*comments);
                }
            }
            Response::Prefetched { id, ready } => {
                self.prefetch.apply(id, ready);
            }
            Response::Art { key, art, edge } => {
                if edge >= self.art.edge() {
                    self.invalidate_artwork(super::ImageSource::Card(key.clone()));
                    self.art.store(key, art);
                }
            }
            Response::Cover { id, mut art, artwork_url, edge } => {
                // Stale unless it is still the track we asked about.
                if self.cover_id.as_deref() == Some(id.as_str()) && edge >= self.cover_edge() {
                    // The terminal may fall back to video pixels if its fetch
                    // fails. Discord keeps the native Music metadata URL and
                    // can load it independently of the terminal's image worker.
                    if artwork_url.is_some() && let Some(now) = self.for_track(&id) {
                        now.artwork_url = artwork_url;
                    }
                    if let Some(cover) = art.as_mut() { cover.shrink(self.cover_edge()); }
                    self.cover = art;
                    self.invalidate_artwork(super::ImageSource::Playing);
                }
            }
        }
    }


    /// The player page, but only if it still belongs to `video_id`.
    ///
    /// Every panel is fetched on its own and any of them can land after the
    /// user has skipped on. This is the one gate they all pass through, so a
    /// late response is dropped rather than drawn under the wrong title.
    pub(super) fn for_track(&mut self, video_id: &str) -> Option<&mut NowPlaying> {
        self.now.as_mut().filter(|now| now.video_id == video_id)
    }


    /// Takes the queue from a watch response.
    ///
    /// The queue is only *replaced* when it does not already contain what is
    /// playing. Advancing through a radio re-asks for the page of each track --
    /// that is where its lyrics and related ids come from -- and adopting each
    /// answer wholesale would reshuffle "Up next" under the user on every
    /// track. Playing something from outside the queue does replace it, which
    /// is what makes a search result start a new mix.
    pub(super) fn apply_watch(&mut self, video_id: &str, watch: Result<Watch, String>) {
        // Minted before the borrow below, which holds the whole of `self`.
        // Unconditionally, because a number spent on a response that turns out
        // to carry no new queue costs nothing: all this has to do is differ
        // from the epoch of the queue it replaces.
        self.queue_epoch += 1;
        let epoch = self.queue_epoch;

        let Some(now) = self.for_track(video_id) else {
            return;
        };
        // Set whether or not it succeeded: this is the round trip the Lyrics
        // tab waits on, and what it does next depends only on there having
        // been one.
        now.watched = true;

        let watch = match watch {
            Ok(watch) => watch,
            Err(why) => {
                // No queue is not an error worth a status line: the track is
                // playing, and the panels say so for themselves. Related's
                // browse id came from this response, so it is never coming --
                // settled here rather than left to say "loading" for the rest
                // of the track.
                now.playing = now.queue.iter().position(|track| track.id == video_id);
                now.related = Panel::Empty(why.clone());
                // Lyrics are not settled with it: LRCLIB needs only the artist
                // and title, and both arrived with the track rather than in
                // this response. Left idle, the fetch starts on the next tick.
                if now.lyrics_query().is_none() {
                    now.lyrics = Panel::Empty(why);
                }
                self.request_cover(video_id.to_string(), None);
                return;
            }
        };

        now.lyrics_id = watch.lyrics_id;
        now.related_id = watch.related_id;
        let artwork_url = watch.artwork_url.clone();
        now.artwork_url = watch.artwork_url;
        if let Some((title, endpoint)) = watch.album_route {
            now.album = Some(title.clone());
            now.album_route = Some((title, endpoint));
        }

        match now.queue.iter().position(|track| track.id == video_id) {
            // The queue carried over, so this response describes a queue we are
            // already inside. Its continuation is deliberately not adopted: the
            // token already held pages the radio the user has been listening
            // to, and this one would page a fresh radio seeded on the track
            // that happens to be playing now.
            Some(index) => now.playing = Some(index),
            None => {
                now.playing = watch.queue.iter().position(|track| track.id == video_id);
                now.queue_title = watch.queue_title;
                now.queue = watch.queue;
                now.queue_offset = 0;
                // A different queue, so everything the old one had learned
                // about paging itself is about somebody else's radio now.
                now.queue_epoch = epoch;
                now.continuation = watch.continuation;
                now.topping_up = false;
                now.topup_failures = 0;
                now.dropped.clear();
                // Follow the track that is playing rather than leaving the
                // cursor on whatever row a previous queue had under it.
                now.cursor[Tab::UpNext.index()] = now.playing.unwrap_or(0);
            }
        }

        // Cards can omit duration, while pasted URLs initially carry no useful
        // metadata at all. The queue has the canonical row for both.
        let canonical = now.hydrate_from_queue();

        // A tab YouTube did not offer is one that will never be fetched, so it
        // is settled here rather than left waiting. Lyrics are checked only
        // after queue hydration because pasted URLs learn the artist and title
        // from that row, which can make LRCLIB a valid fallback.
        if now.lyrics_id.is_none() && now.lyrics_query().is_none() {
            now.lyrics = Panel::Empty("no lyrics are published for this track".to_string());
        }
        if now.related_id.is_none() {
            now.related = Panel::Empty("nothing related came back for this track".to_string());
        }

        // Warm the track after this one while the current one plays. This is
        // the whole reason the queue is worth having early: an automatic
        // advance that hits the URL cache starts in milliseconds, where a cold
        // one spends seconds of silence between tracks.
        if let Some(next) = now.next_in_queue().map(|track| track.id.clone()) {
            let _ = self.source.send(Request::Prefetch { id: next });
        }

        // Listening history and recommendations must use the same canonical
        // metadata the player page now shows, not a pasted URL and blank artist.
        if let Some(canonical) = canonical {
            self.playback.hydrate_track(canonical);
        }

        // The canonical row may have made an artist action available, and the
        // queue selection can have moved. A snapshot opened before that answer
        // must not invoke the old set of actions.
        self.close_page_actions();

        // A queue short enough to need topping up the moment it lands -- the
        // tail of a playlist, or a radio that answered with one page. Asking
        // here as well as on every play is what covers it: `play_track` runs
        // before this response and saw a queue that was still empty.
        self.top_up_queue();
        self.request_cover(video_id.to_string(), artwork_url);
    }
}
