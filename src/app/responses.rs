//! Nonblocking source polling and application of correlated worker responses.

use std::time::{Duration, Instant};

use anyhow::Result;

use crate::player::Command;
use crate::source::watch::Watch;
use crate::source::worker::{Request, Response};

use super::{App, Overlay, SignIn};
use super::navigation::{Mode, View};
use super::now_playing::{NowPlaying, Panel, Tab};

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
        if let Response::ArtistSongs { request_id, browse_id, page } = response {
            self.apply_artist_songs(request_id, &browse_id, *page);
            return;
        }
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
        // Validate before any shared status, menu or busy state is touched. A
        // late page must be observationally silent, not merely prevented from
        // replacing the rows after it has already closed their menu.
        if page_request.is_some_and(|request_id| !self.accept_page_response(request_id)) {
            return;
        }

        // A contextual menu is a snapshot of the page under it. Responses that
        // replace that page close the menu rather than letting Enter act on a
        // different row than the one still visible in the modal.
        if matches!(
            &response,
            Response::Results { .. }
                | Response::Home { .. }
                | Response::Browsed { .. }
                | Response::Artist { .. }
        ) {
            self.close_page_actions();
        }

        // Nothing here is something the user is waiting on, so none of it may
        // clear the flag a search or a play set. The player page matters as
        // much as the cover does: its panels land on their own threads, and the
        // queue usually arrives while the resolve behind it is still running --
        // clearing the flag there would drop the redraw cadence back to the
        // idle one for the rest of the wait that flag exists to cover.
        //
        // A resolve is excluded for a different reason: a play never sets this
        // flag in the first place -- it tracks `pending` instead, for the
        // reason `play_track` gives -- so there is nothing here for a resolve
        // to be clearing. What it would clear is some *other* request's flag,
        // still in flight, and `awaiting` would then stop reporting a wait that
        // is still going on.
        if !matches!(
            response,
            Response::Rating { .. }
                | Response::Library { .. }
                | Response::Playlists { .. }
                | Response::PlaylistSaved { .. }
                | Response::Cover { .. }
                | Response::Art { .. }
                | Response::Results { .. }
                | Response::Browsed { .. }
                | Response::Artist { .. }
                | Response::Prefetched { .. }
                | Response::Resolved { .. }
                | Response::Replacement { .. }
                | Response::Watch { .. }
                | Response::MoreQueue { .. }
                | Response::Lyrics { .. }
                | Response::Related { .. }
                | Response::Comments { .. }
                | Response::Home { .. }
                | Response::HomeFailed { .. }
                | Response::CookiesImported { .. }
                | Response::MusicSignInFailed { .. }
                | Response::SessionRenewed { .. }
        ) {
            self.busy = false;
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
            Response::Home {
                generation,
                shelves,
            } => {
                if generation != self.home_generation {
                    return;
                }
                self.home_attempts = self.home_attempts.saturating_sub(1);
                self.home_pending = self.home_attempts != 0;
                self.apply_home(shelves);
            }
            Response::HomeFailed { generation } => {
                if generation != self.home_generation {
                    return;
                }
                self.home_attempts = self.home_attempts.saturating_sub(1);
                self.finish_home_attempts();
            }
            Response::HomeMore { generation, shelves } => {
                if generation != self.home_generation { return; }
                for shelf in shelves {
                    if self.home.len() >= 24 { break; }
                    if self.home.iter().any(|existing| existing.title == shelf.title) { continue; }
                    self.home.push(shelf);
                    self.home_scroll.push(0);
                }
            }
            Response::CookiesImported { browser, .. } => {
                self.clear_account_actions();
                // The page on screen was built without a session. There is a
                // better one available now, so it is asked for again -- this is
                // the whole point of doing the import in the background rather
                // than holding the first frame back behind it.
                self.status = format!("read your YouTube session from {browser}");
                self.music_signing_in = false;
                self.automatic_sign_in = false;
                self.sign_in_prompted = false;
                self.session_renewal.succeeded();
                if matches!(self.overlay, Overlay::SignIn(SignIn::Music { .. })) {
                    self.overlay = Overlay::None;
                }
                // A valid session is the missing half of any reports queued
                // while signed out or while the previous cookie was stale.
                let _ = self.source.send(Request::RetryReports);
                self.request_rating();
                self.request_home();
                let in_library = self.view == View::Library;
                self.reset_library();
                if in_library { self.open_library(); }
            }
            Response::SessionRenewed { result, .. } => {
                self.music_signing_in = false;
                match result {
                    Ok(()) => {
                        self.sign_in_prompted = false;
                        self.session_renewal.succeeded();
                        crate::diagnostics::info("auth", "saved Music session renewed automatically");
                        let _ = self.source.send(Request::RetryReports);
                        if self.now.as_ref().is_none_or(|now| !now.like_pending) { self.request_rating(); }
                    }
                    Err(crate::session::RenewalFailure::SignInRequired) => {
                        if !self.sign_in_prompted {
                            self.sign_in_prompted = true;
                            self.automatic_sign_in = true;
                            self.music_signing_in = true;
                            self.status = "Google needs verification—finish in the sign-in window.".into();
                            if self.source.send(Request::MusicSignIn { recover: true }).is_err() {
                                self.music_signing_in = false;
                                self.automatic_sign_in = false;
                            }
                        }
                    }
                    Err(crate::session::RenewalFailure::Temporary(message)) => {
                        crate::diagnostics::warn("auth", &format!("automatic renewal deferred: {message}"));
                    }
                }
            }
            Response::MusicSignInFailed { message: msg, .. } => {
                crate::diagnostics::error("auth", "YouTube Music sign-in failed");
                self.music_signing_in = false;
                if self.automatic_sign_in {
                    self.automatic_sign_in = false;
                    self.status = "Google sign-in was not completed; automatic recovery will keep checking.".into();
                    return;
                }
                self.status = msg.clone();
                if matches!(self.overlay, Overlay::Settings) {
                    self.preferences.notice = Some((msg, true));
                } else {
                    self.menu = None;
                    self.overlay = Overlay::SignIn(SignIn::Failed { reason: msg });
                }
            }
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
            Response::ArtistSongs { .. } => unreachable!("artist songs are matched before shared page state"),
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
            Response::Art { key, art } => self.art.store(key, art),
            Response::Cover { id, art, artwork_url } => {
                // Stale unless it is still the track we asked about.
                if self.cover_id.as_deref() == Some(id.as_str()) {
                    // The terminal may fall back to video pixels if its fetch
                    // fails. Discord keeps the native Music metadata URL and
                    // can load it independently of the terminal's image worker.
                    if artwork_url.is_some() && let Some(now) = self.for_track(&id) {
                        now.artwork_url = artwork_url;
                    }
                    self.cover = art;
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
