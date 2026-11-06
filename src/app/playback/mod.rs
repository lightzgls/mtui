//! Playback transitions, stream recovery, queue dispatch, and listening progress.

pub(super) mod now_playing;
pub(super) mod prefetch;

use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::runtime::config;
use crate::player::{Command, PlayState, PlayerEvent, Snapshot};
use crate::source::{StreamUrl, Track, listening::journal};
use crate::source::catalog::home::Target;
use crate::source::youtube::watch::QueuePage;
use crate::source::worker::Request;

use crate::app::{App, Overlay};
use crate::app::artwork::CoverSize;
use crate::app::controls::input::POINTER_SCALE;
use crate::app::browse::navigation::{Mode, PageRequestKind, View};
use crate::app::playback::now_playing::{NowPlaying, Panel, RelatedRow, RepeatMode, Tab, shuffle_tracks};
use crate::app::controls::preferences::{DEFAULT_UNMUTED_VOLUME, VOLUME_STEP};

mod state;
pub(super) use state::PlaybackState;

/// Tracks the queue may skip past in a row before it gives up.
///
/// A radio queue is built by YouTube, not by the user, and some of what it
/// offers cannot be resolved from here at all -- so one dead track must not end
/// the session. A run of them, though, is a signal that something is wrong with
/// resolving rather than with the tracks, and silently walking the queue looking
/// for one that works is not a thing to do on the user's behalf.
///
/// This matters more now than when the queue was one page. A queue that pages
/// itself indefinitely has no natural end to stop a broken resolver at, so
/// without this bound a program that could no longer play anything would walk
/// forward through stations for as long as it was left running.
pub(super) const MAX_AUTO_SKIPS: u8 = 3;

/// Tracks remaining ahead at which the next page is asked for.
///
/// Not zero, which is the whole point. At zero the user hears silence for a
/// round trip *plus* a cold resolve -- seconds of it -- where five tracks of
/// warning means the page lands, and the track after it is prefetched, long
/// before either is needed. The same argument that puts the prefetch behind the
/// watch response rather than behind the track ending.
pub(super) const QUEUE_LOW: usize = 5;

/// Failed top-ups in a row before the queue stops asking.
///
/// One failure is a passing network blip and must not end an endless queue for
/// the session; a run of them is YouTube declining to page this queue, and
/// asking again on every track would be a request per song for nothing.
pub(super) const MAX_TOPUP_FAILURES: u8 = 3;

pub(super) fn duration_at_pointer(total: Duration, position: u16) -> Duration {
    total.mul_f64(pointer_fraction(position))
}

pub(super) fn pointer_fraction(position: u16) -> f64 {
    f64::from(position.min(POINTER_SCALE)) / f64::from(POINTER_SCALE)
}

pub(super) fn mute_transition(muted: bool, current: f32, remembered: f32) -> (f32, f32) {
    if muted {
        (
            if remembered > 0.0 { remembered.min(DEFAULT_UNMUTED_VOLUME) } else { VOLUME_STEP },
            remembered,
        )
    } else {
        (0.0, if current > 0.0 { current } else { remembered })
    }
}

pub(super) fn playback_failure_summary(why: &str) -> &'static str {
    let why = why.to_ascii_lowercase();
    if ["sign in", "login", "cookie", "authentication", "http 401", "http 403"]
        .iter()
        .any(|needle| why.contains(needle))
    {
        "YouTube session needs attention"
    } else if why.contains("source worker") || why.contains("resolver") {
        "playback service is unavailable"
    } else if ["unavailable", "not available", "private", "restricted", "removed"]
        .iter()
        .any(|needle| why.contains(needle))
    {
        "track is unavailable"
    } else if [
        "timeout",
        "timed out",
        "network",
        "connect",
        "dns",
        "http 5",
        "request failed",
    ]
    .iter()
    .any(|needle| why.contains(needle))
    {
        "network interrupted playback"
    } else if ["decode", "decoder", "codec", "format", "audio stream"]
        .iter()
        .any(|needle| why.contains(needle))
    {
        "audio format could not be played"
    } else {
        "track could not be played"
    }
}

pub(super) fn playback_failure_status(summary: &str) -> String {
    format!("{summary} — r retry · n skip")
}

impl App {

    pub fn snapshot(&self) -> Snapshot {
        self.player.snapshot()
    }


    pub(super) fn playback_failed(&self, snapshot: &Snapshot) -> bool {
        self.now.is_some() && self.playback.failed(snapshot)
    }


    /// True while the user is waiting on something: a request in flight, or a
    /// track being loaded. What the event loop redraws faster for -- a resolve
    /// that lands from cache is over in milliseconds, and waiting out the idle
    /// tick to notice would be most of the latency it has left.
    pub fn awaiting(&self) -> bool {
        self.page_loading() || self.playback.is_loading() || (self.view == View::Library && self.library.loading())
            || (self.view == View::Artist && self.artist.as_ref().is_some_and(|artist| artist.songs_request.is_some()))
    }


    /// Answers what the player thread cannot do for itself.
    ///
    /// It owns the decoder and the output device and nothing else on purpose --
    /// resolving a URL means yt-dlp, a cache and a network client, none of
    /// which belong on the thread feeding the speakers. So when a stream dies
    /// and needs a new one, it asks here.
    pub(super) fn poll_player(&mut self) {
        while let Some(event) = self.player.poll_event() {
            match event {
                PlayerEvent::NeedsUrl { id, from } => self.resume_track(id, from),
                PlayerEvent::OutputChanged { id, name } => {
                    self.output_device = id;
                    self.refresh_output_devices();
                    let settings = config::Settings {
            terminal_host: self.terminal_host,
                        start_in_tray: self.start_in_tray,
                        tray_click_pauses: self.tray_click_pauses,
                        precision_volume: self.precision_volume,
                        show_key_hints: self.show_key_hints,
                        cover_style: self.cover_style,
                        image_renderer: self.image_renderer,
                        volume: self.snapshot().volume,
                        output_device: self.output_device.clone(),
                    };
                    self.status = match settings.save() {
                        Ok(()) => format!("audio output set to {name}"),
                        Err(error) => {
                            crate::runtime::diagnostics::error(
                                "config",
                                &format!("could not save audio output: {error:#}"),
                            );
                            format!("audio output changed, but could not be saved: {error:#}")
                        }
                    };
                    if matches!(self.overlay, Overlay::Settings) {
                        self.preferences.notice = Some((self.status.clone(), self.status.contains("could not")));
                    }
                }
                PlayerEvent::OutputChangeFailed { why } => {
                    self.status = why;
                    if matches!(self.overlay, Overlay::Settings) {
                        self.preferences.notice = Some((self.status.clone(), true));
                    }
                }
            }
        }
    }


    /// Resolves a fresh URL for a track whose stream died, and hands it back.
    ///
    /// The cache is bypassed deliberately: the entry it holds for this track is
    /// the URL that just failed. Handing it back was how a track that stopped
    /// once stopped at the same second on every replay for the rest of the
    /// session, and then quietly started working hours later when the
    /// signature expired.
    pub(super) fn resume_track(&mut self, id: String, from: Duration) {
        let Some(track) = self.playback.listening_track() else {
            // Nothing is playing any more, so there is nothing to carry on.
            let _ = self.player.send(Command::ResumeFailed {
                why: "playback stopped".to_string(),
            });
            return;
        };
        if track.id != id { return; }
        let title = track.label();

        self.status = "Reconnecting audio…".into();
        let request = Request::Resolve {
            id: id.clone(),
            title,
            bypass_cache: true,
        };
        if self.source.send(request).is_err() {
            let _ = self.player.send(Command::ResumeFailed {
                why: "source worker is not running".to_string(),
            });
            return;
        }
        self.playback.begin_recovery(id, from);
    }


    /// Starts a track, or reports why it will not start -- but only when it is
    /// still the track the user is waiting for.
    ///
    /// The worker is serial and a resolve costs seconds, so a run of Enters
    /// leaves answers arriving for tracks that have already been skipped past.
    /// Every one of them used to be played, which is what put the previous song
    /// under the current one's title and cover; here they are dropped instead.
    pub(super) fn apply_resolved(&mut self, id: &str, title: String, stream: Result<StreamUrl, String>) {
        // A recovery, not a choice. Checked first because none of what follows
        // applies to it: the track is already playing, already named and
        // already on screen, and the only thing being replaced is the URL the
        // audio comes from. Running it through the path below would restart the
        // song from the beginning under its own title.
        if let Some(from) = self.playback.take_recovery(id) {
            let cmd = match stream {
                Ok(stream) => {
                    self.status.clear();
                    Command::Resume {
                        url: stream.url,
                        format: stream.format,
                        from,
                    }
                }
                // Nothing left to try. Reported rather than dropped: the player
                // is parked in `Buffering` waiting for this, and silence here
                // would leave it there naming a track it is never going to
                // play another second of.
                Err(why) => {
                    let summary = self.record_playback_failure(&why);
                    Command::ResumeFailed { why: summary }
                }
            };
            let _ = self.player.send(cmd);
            return;
        }

        let Some(pending) = self.playback.accept_resolved(id) else { return; };

        let why = match stream {
            Ok(stream) => {
                self.status.clear();
                // The track resolved, so whatever run of failures preceded it
                // is over -- the budget is for consecutive failures. Nothing
                // automatic is in flight any more either, so a later failure
                // belongs to whatever asked for it.
                self.playback.resolved();
                let _ = self.player.send(Command::Play {
                    url: stream.url,
                    format: stream.format,
                    title,
                    id: id.to_string(),
                });
                if let Some(seek) = pending.seek {
                    let _ = self.player.send(seek);
                }
                return;
            }
            Err(why) => why,
        };

        // A queue that cannot resolve its next track steps over it rather than
        // falling silent -- YouTube's radio offers plenty that this program
        // cannot play, and one of them must not end the session. Bounded, and
        // now that the queue pages itself there is no end to reach that would
        // stop it otherwise: see [`MAX_AUTO_SKIPS`].
        if self.playback.skip_failed_track() {
            // First line only: a failure with instructions in it runs to a
            // paragraph, and the status bar is one line -- the rest would be
            // drawn as control characters across it. The user did not ask for
            // this track anyway; the whole explanation is owed to whoever
            // presses Enter on it.
            let first = why.lines().next().unwrap_or(&why);
            self.status = format!("skipping a track that would not play ({first})");
            self.advance(1, true);
            // Nothing to step to: the queue ran out on the track that would not
            // play. The player is still showing that track as loading, and no
            // play is coming to replace it, so it is taken down here.
            if !self.playback.is_loading() {
                let _ = self.player.send(Command::Stop);
            }
            return;
        }

        self.playback.end_automatic_playback();
        // The player was put into `Buffering` for this track the moment it was
        // chosen, and nothing is going to arrive to take it out of it. Left
        // alone it would sit there naming a track that is never going to play,
        // over an error the user cannot see past it.
        let _ = self.player.send(Command::Stop);
        self.record_playback_failure(&why);
    }


    /// Asks for the next page of the queue, if one is wanted.
    ///
    /// Called on every play and whenever a queue arrives, and cheap to call
    /// when there is nothing to do -- which is most of the time.
    ///
    /// Two ways to answer, tried in that order. YouTube's own continuation is
    /// always preferred: it is the station the user chose, and it is one round
    /// trip. Only once that has nothing left does the journal build a new
    /// station -- which is the difference between a queue that ends when the
    /// radio does and one that plays for as long as anybody wants it to.
    pub(super) fn top_up_queue(&mut self) {
        let Some(now) = self.now.as_mut() else {
            return;
        };
        // Nothing is playing inside this queue, so nothing is walking towards
        // its end and a page appended here would be tracks nobody reaches.
        // `advance` declines to move for the same reason. This covers the
        // empty queue before the first watch response too, which must not be
        // seeded from the journal on top of the station already on its way.
        if now.playing.is_none() || now.repeat == RepeatMode::All || now.finite_queue {
            return;
        }
        // Still deep enough, already asking, or given up asking.
        if now.remaining() >= QUEUE_LOW
            || now.topping_up
            || now.topup_failures >= MAX_TOPUP_FAILURES
        {
            return;
        }

        let epoch = now.queue_epoch;
        let request = match now.continuation.clone() {
            Some(token) => Request::MoreQueue { epoch, token },
            None => {
                // Stepped after being read, so the first fallback of a session
                // is built from the best-scoring seed rather than the second.
                // Stepped per attempt rather than per queue, so a station that
                // turns out to be a dead end is not the one built again next
                // time -- which is why this is kept on `App`, where it outlives
                // any one queue.
                let rotation = self.seed_rotation;
                self.seed_rotation += 1;
                Request::SeedQueue { epoch, rotation }
            }
        };

        // Re-borrowed: minting the rotation above needed `self`, which ended
        // the borrow this was taken through.
        if let Some(now) = self.now.as_mut() {
            now.topping_up = true;
        }
        if self.source.send(request).is_err()
            && let Some(now) = self.now.as_mut()
        {
            // Nothing is coming, so the flag would otherwise stand for a
            // request in flight for the rest of the session and stop the queue
            // ever asking again.
            now.topping_up = false;
        }
    }


    /// Takes a page of tracks onto the end of the queue it was asked for.
    ///
    /// Nothing here reaches the status bar. The user did not ask for this and
    /// is listening to music; a queue that quietly stops growing ends exactly
    /// as it did before any of this existed, which is a queue that runs out.
    pub(super) fn apply_more_queue(&mut self, epoch: u64, page: Result<QueuePage, String>) {
        // The queue that asked has since been replaced -- the user played
        // something from outside it. There is nothing to apply this to, and the
        // queue that took its place has its own token.
        let Some(now) = self.now.as_mut().filter(|now| now.queue_epoch == epoch) else {
            return;
        };
        now.topping_up = false;

        let page = match page {
            Ok(page) => page,
            Err(_) => {
                // Whatever was going to be tried is still going to be tried:
                // a token that failed is kept for the next play, and a journal
                // that could not build a station will be asked again for a
                // different one. One failure is a blip. A run of them is
                // YouTube declining to page this queue, or a journal with
                // nothing left to seed from, and [`MAX_TOPUP_FAILURES`] is what
                // stops either becoming a request per track for the session.
                now.topup_failures += 1;
                return;
            }
        };

        if now.take_page(page) == 0 {
            return;
        }

        // The queue may have been down to its last track when this landed, in
        // which case the track now after it has never been warmed -- and it is
        // about to be the one playing. Cheap when it was already prefetched:
        // the worker answers a warm id out of its cache.
        if let Some(next) = now.next_in_queue().map(|track| track.id.clone()) {
            let _ = self.source.send(Request::Prefetch { id: next });
        }

        // The queue ran dry before this page arrived: the last track ended, the
        // advance found nothing to move to, and the player has been sitting in
        // silence since. There is something to play now, so it starts -- which
        // is what makes a queue that ran out mid-session recover on its own
        // rather than needing the user to press a key.
        //
        // A stop is not this case and cannot reach here: it clears the page,
        // and there is nothing left for a continuation to be applied to. A
        // paused track is not it either -- `Paused` is its own state.
        if !self.playback.is_loading() && self.snapshot().state == PlayState::Idle {
            self.advance(1, true);
        }
    }


    /// Updates Now Playing in place, whether it is in the sidebar or expanded.
    ///
    /// `auto` marks a play the queue started rather than the user; only those
    /// are allowed to step over a track that will not resolve.
    pub(super) fn play_track(&mut self, track: Track, auto: bool) {
        self.start_track(track, auto, false);
    }


    fn start_track(&mut self, track: Track, auto: bool, bypass_cache: bool) {
        // Automatic queue advances can happen while a contextual menu is open.
        // Its actions belong to the track it was opened over, not its successor.
        self.close_page_actions();
        // Before anything else touches the player: whatever was playing is over
        // as of this call, and this is the last moment its position is still
        // readable. Every play in the program funnels through here, so this
        // covers a skip, a queue advance and a fresh choice alike.
        self.finish_listening();
        // Whatever the previous track was waiting on, it is not waiting any
        // more. Left set, a recovery URL arriving for the song just left would
        // be applied to the one that replaced it.
        let report_scope = crate::runtime::config::Cookies::active_report_scope();
        self.playback.begin_track(&track, auto, report_scope);
        self.status = "Loading audio…".into();

        // Before the resolve, not after it. Resolving costs seconds, and until
        // this lands the speakers are still on the previous track while the
        // page below has already been replaced with this one -- the player
        // saying two different things about what is playing. This stops the old
        // track and puts the new one's name up as loading, so that what is on
        // screen and what is audible never disagree.
        //
        // Every play goes through here, so it also covers the queue advancing
        // itself, where the old track has ended and there is nothing to stop.
        let _ = self.player.send(Command::Load {
            title: track.label(),
        });
        // Carried over so the page does not blink back to the first tab on
        // every track: what the user was reading is a choice about the session,
        // not about the song.
        let tab = self.now.as_ref().map_or(Tab::UpNext, |now| now.tab);
        let repeat = self.now.as_ref().map_or(RepeatMode::Off, |now| now.repeat);
        let mut page = NowPlaying::new(&track);
        page.tab = tab;
        page.detail_tab = self.now.as_ref().map_or(Tab::UpNext, |now| now.detail_tab);
        page.repeat = repeat;
        page.lyrics_expanded = self.now.as_ref().is_some_and(|now| now.lyrics_expanded);
        // The queue survives moving *within* it -- that is what makes advancing
        // through a radio keep one stable "Up next" rather than reshuffling on
        // every track. Playing something from outside it drops it instead of
        // carrying it: a queue kept beside a track that is not in it would be
        // labelled as what is playing while listing something else, and if the
        // watch response then failed it would stay that way. The panels never
        // survive, because every one of them belongs to a video id that has
        // just changed.
        if let Some(old) = self.now.take()
            && let Some(index) = old.queue.iter().position(|held| held.id == track.id)
        {
            page.queue_title = old.queue_title;
            page.queue = old.queue;
            page.finite_queue = old.finite_queue;
            page.queue_offset = old.queue_offset;
            page.playing = Some(index);
            page.cursor[Tab::UpNext.index()] = index;
            // Everything the queue knows about paging itself travels with it,
            // or the endless queue would end on the first track: the token is
            // what buys the next page, and the epoch is what proves a page that
            // arrives two tracks later still belongs here.
            page.queue_epoch = old.queue_epoch;
            page.continuation = old.continuation;
            page.topping_up = old.topping_up;
            page.topup_failures = old.topup_failures;
            page.dropped = old.dropped;
            // Now that the queue has moved forward, whatever has fallen out of
            // the window behind it can go. This is the only place the queue
            // grows a position, so it is the only place that has to shrink.
            if page.repeat != RepeatMode::All {
                page.trim();
            }
        }
        self.now = Some(page);

        // Playback tracks its pending resolve separately from page navigation.
        let request = Request::Resolve {
            id: track.id.clone(),
            // Keep resolver/player metadata attached to the selected track.
            title: track.label(),
            // A track the user chose. The cache is exactly what should answer
            // it when it can -- that is the difference between pressing Enter
            // on a replay and waiting three seconds for yt-dlp again.
            bypass_cache,
        };
        if self.source.send(request).is_err() {
            self.playback.cancel_loading();
            let _ = self.player.send(Command::Stop);
            self.playback.fail("playback service is unavailable".to_string());
            self.status = playback_failure_status("playback service is unavailable");
            return;
        }
        // Both behind the resolve, which is the order that matters: it is the
        // one producing audio, and these are decoration around it.
        self.request_rating();
        // Wait for Music metadata before choosing the cover source. Clear the
        // previous picture now so a skipped track cannot keep its artwork.
        self.cover = None;
        self.cover_id = None;
        let _ = self.source.send(Request::Watch { video_id: track.id });
        // A tab the user is already on has to be fetched now; the switch that
        // would otherwise have done it is not going to happen again.
        self.request_tab();
        // Every play moves the queue forward by one, so every play is a place
        // the tail can have got short enough to want another page. This is the
        // one funnel they all pass through, which is why the check lives here
        // rather than beside the advance that started most of them.
        self.top_up_queue();
    }


    /// Fetches the panel behind the open tab, if it has not been asked for.
    ///
    /// Called on every tab switch and on every track change, so a panel is
    /// fetched when first looked at. Incomplete lyrics can be retried without
    /// making the user restart playback.
    pub(super) fn request_tab(&mut self) {
        if let Some((tab, detail_tab)) = self.now.as_ref().map(|now| (now.tab, now.detail_tab)) {
            self.request_panel(if self.view == View::Playing && tab == Tab::Lyrics { detail_tab } else { tab });
            if self.view == View::Playing { self.request_panel(Tab::Lyrics); }
        }
    }

    fn request_panel(&mut self, tab: Tab) {
        let Some(now) = self.now.as_mut() else {
            return;
        };
        let video_id = now.video_id.clone();

        let request = match tab {
            // Already in hand: the queue arrives with the watch response.
            Tab::UpNext => return,
            // Waits on the watch response rather than on the browse id it
            // carries: a track with no lyrics page is not a track with no
            // lyrics, it is the one LRCLIB is there to answer for. Until the
            // response lands there is no telling which of the two this is, so
            // the panel is left idle and `tick_page` opens it again.
            Tab::Lyrics if now.lyrics.is_idle() => {
                if !now.watched {
                    return;
                }
                now.lyrics = Panel::Loading;
                now.lyrics_attempts += 1;
                now.lyrics_retry_at = None;
                Request::Lyrics {
                    video_id,
                    browse_id: now.lyrics_id.clone(),
                    query: now.lyrics_query(),
                }
            }
            Tab::Related if now.related.is_idle() => match now.related_id.clone() {
                Some(browse_id) => {
                    now.related = Panel::Loading;
                    Request::Related {
                        video_id,
                        browse_id,
                    }
                }
                None => return,
            },
            Tab::Comments if now.comments.is_idle() => {
                now.comments = Panel::Loading;
                Request::Comments { video_id }
            }
            // Already loading, already loaded, or already known to be empty.
            _ => return,
        };

        if self.source.send(request).is_err() {
            self.status = "source worker is not running".to_string();
        }
    }


    /// Picks up a tab whose fetch could not be started when it was opened.
    ///
    /// Lyrics and Related are asked for with a browse id that arrives in the
    /// watch response, which is a round trip behind the play. Opening either
    /// tab in that window would otherwise leave it idle forever.
    pub fn tick_page(&mut self) {
        let waiting = self.now.as_ref().is_some_and(|now|
            ((now.tab == Tab::Lyrics || self.view == View::Playing) && now.lyrics.is_idle() && now.watched)
                || ((now.tab == Tab::Related || (self.view == View::Playing && now.tab == Tab::Lyrics && now.detail_tab == Tab::Related))
                    && now.related.is_idle() && now.related_id.is_some()));
        if waiting {
            self.request_tab();
        }
        if let Some(now) = self.now.as_mut()
            && (now.tab == Tab::Lyrics || self.view == View::Playing)
            && now.lyrics_retry_at.is_some_and(|at| Instant::now() >= at)
        {
            now.lyrics_retry_at = None;
            now.lyrics_attempts += 1;
            let request = Request::Lyrics {
                video_id: now.video_id.clone(),
                browse_id: now.lyrics_id.clone(),
                query: now.lyrics_query(),
            };
            // Keep the plain lyrics visible while their upgrade is in flight.
            let _ = self.source.send(request);
        }
    }


    /// Advances the queue when a track ends. Called once per frame.
    ///
    /// The snapshot only says what is true now, so the end of a track is an
    /// edge rather than a state: an active stream last frame and `Idle` this
    /// one, with no error to explain it. A seek straight to EOF can drain while
    /// the UI still observes Buffering, before it ever draws a Playing frame.
    /// A stream that died reports an error; a stop clears the page outright.
    pub fn tick_playback(&mut self) {
        let snap = self.snapshot();
        if self.playback.failure().is_none()
            && self.now.is_some()
            && !self.playback.is_loading()
            && snap.state == PlayState::Idle
            && let Some(error) = snap.error.as_deref()
        {
            self.record_playback_failure(error);
        }
        let ended = self.playback.observe_end(&snap);

        // Sampled every frame rather than read once at the end, because at the
        // end there is nothing to read: an ended track reports position zero.
        if let Some(report) = self.playback.observe_listening(&snap, Instant::now())
            && journal::record_pending(report)
        {
            self.playback.checkpoint_sent();
            let _ = self.source.send(Request::RetryReports);
        }
        if ended {
            // start_track already finalized the previous song. Its late EOF
            // must not take the replacement song's listening/recovery state.
            self.finish_listening();
            self.advance(1, true);
        }
    }


    pub(super) fn toggle_pause(&mut self) {
        let _ = self.player.send(Command::TogglePause);
    }


    pub(super) fn retry_current_track(&mut self) {
        let snapshot = self.snapshot();
        if !self.playback_failed(&snapshot) {
            return;
        }
        let Some(track) = self.playback.listening_track().cloned() else {
            return;
        };
        self.start_track(track, false, true);
    }


    pub(super) fn remove_queue_selection(&mut self) {
        let Some(removed) = self
            .now
            .as_mut()
            .and_then(NowPlaying::remove_selected_upcoming)
        else {
            return;
        };
        self.status = format!("removed {} from the queue", removed.label());
    }


    pub(super) fn move_queue_selection(&mut self, delta: isize) {
        let moved = self
            .now
            .as_mut()
            .is_some_and(|now| now.move_selected_upcoming(delta));
        if !moved {
            self.status = match self.now.as_ref() {
                Some(now) if now.playing.is_none() => "The queue is still loading.".into(),
                Some(now) if now.playing.is_some_and(|at| now.cursor() <= at) =>
                    "Select an upcoming song to move; the playing song and history stay in place.".into(),
                _ if delta < 0 => "This is already the first upcoming song.".into(),
                _ => "This is already the last upcoming song.".into(),
            };
            return;
        }
        self.prefetch_queue_next();
        if let Some(now) = &self.now {
            let at = now.cursor[Tab::UpNext.index()];
            self.status = format!("Moved {} to Up next #{}", now.queue[at].title,
                at.saturating_sub(now.playing.unwrap_or(0)));
        }
    }


    pub(crate) fn queue_drag_identity(&self, index: usize) -> Option<(String, u64)> {
        if self.mode != Mode::Browse || self.overlay.is_open() || self.menu.is_some() { return None; }
        let now = self.now.as_ref().filter(|now| now.tab == Tab::UpNext)?;
        (index > now.playing? && index < now.queue.len()).then(|| (now.queue[index].id.clone(), now.queue_epoch))
    }


    pub(crate) fn drag_queue_song(&mut self, id: &str, epoch: u64, target: usize) {
        let Some((_, active_epoch)) = self.queue_drag_identity(target) else { return; };
        if active_epoch != epoch { return; }
        let Some(now) = self.now.as_mut() else { return; };
        let Some(from) = now.queue.iter().position(|track| track.id == id) else { return; };
        if !now.playing.is_some_and(|playing| from > playing) || from == target { return; }
        now.cursor[Tab::UpNext.index()] = from;
        self.move_queue_selection(target as isize - from as isize);
    }


    pub(super) fn clear_upcoming_queue(&mut self) {
        let removed = self.now.as_mut().map_or(0, NowPlaying::clear_upcoming);
        let paging = self.now.as_ref().is_some_and(|now| now.topping_up || now.continuation.is_some());
        if removed == 0 && !paging {
            return;
        }
        // A continuation already in flight belongs to the queue before the
        // explicit clear. Minting a new epoch makes its late answer harmless.
        self.queue_epoch = self.queue_epoch.wrapping_add(1);
        if let Some(now) = self.now.as_mut() {
            now.queue_epoch = self.queue_epoch;
            now.continuation = None;
            now.topping_up = false;
            now.topup_failures = MAX_TOPUP_FAILURES;
        }
        self.status = format!("cleared {removed} upcoming tracks");
    }


    pub(super) fn shuffle_upcoming_queue(&mut self) {
        let shuffled = self.now.as_mut().map_or(0, NowPlaying::shuffle_upcoming);
        if shuffled == 0 {
            return;
        }
        self.prefetch_queue_next();
        self.status = format!("shuffled {shuffled} upcoming tracks");
    }


    pub(super) fn prefetch_queue_next(&self) {
        let next = self
            .now
            .as_ref()
            .and_then(NowPlaying::next_in_queue)
            .map(|track| track.id.clone());
        if let Some(id) = next {
            let _ = self.source.send(Request::Prefetch { id });
        }
    }


    pub(super) fn cycle_repeat(&mut self) {
        let Some(repeat) = self.now.as_ref().map(|now| now.repeat.next()) else {
            return;
        };
        if repeat == RepeatMode::All {
            // Freeze this bounded window. A continuation already on its way is
            // invalidated by the new epoch; its token remains available if
            // repeat is later switched off.
            self.queue_epoch = self.queue_epoch.wrapping_add(1);
        }
        if let Some(now) = self.now.as_mut() {
            now.repeat = repeat;
            if repeat == RepeatMode::All {
                now.queue_epoch = self.queue_epoch;
                now.topping_up = false;
            } else if now.topup_failures >= MAX_TOPUP_FAILURES {
                now.topup_failures = 0;
            }
        }
        self.status = format!("repeat {}", repeat.label());
        if repeat != RepeatMode::All {
            self.top_up_queue();
        }
    }


    /// Moves `delta` tracks through the queue and plays what it lands on.
    ///
    /// Silent at either end: there is nothing before the first track, and a
    /// queue with nothing after the last one is not a failure either. Reaching
    /// that end is now rare rather than routine -- the queue tops itself up
    /// several tracks before it gets there -- so the two cases left are a queue
    /// still waiting on a page and one that has genuinely nothing left to
    /// offer, which say different things and are told apart below.
    ///
    /// `auto` distinguishes the queue advancing itself from `n` and `p`. Only
    /// the former steps over a track that will not resolve -- a user who
    /// pressed a key is owed the reason it did nothing.
    pub(super) fn advance(&mut self, delta: isize, auto: bool) {
        let Some(now) = self.now.as_ref() else {
            return;
        };
        let Some(index) = now.advanced_index(delta, auto) else {
            if delta > 0 {
                // A page is on its way, so this is not the end -- only the gap
                // between running out and hearing back. `apply_more_queue`
                // starts playing again when it lands, so the user is told to
                // expect that rather than told the session is over.
                self.status = if now.topping_up {
                    "waiting for more of the queue ...".to_string()
                } else {
                    "end of the queue".to_string()
                };
            }
            return;
        };
        let Some(track) = now.queue.get(index).cloned() else {
            return;
        };

        self.play_track(track, auto);
    }


    pub(super) fn record_playback_failure(&mut self, why: &str) -> String {
        crate::runtime::diagnostics::error("playback", why);
        let summary = playback_failure_summary(why).to_string();
        self.playback.fail(summary.clone());
        self.status = playback_failure_status(&summary);
        summary
    }


    /// Hands the finished play to the worker, which journals it and -- with a
    /// cookie saved -- reports it to YouTube.
    ///
    /// Idempotent by construction: the track is taken, so the several paths that
    /// can end a play (the queue advancing, a new choice, or a stop) may
    /// all call this and only the first does anything.
    ///
    /// Nothing is reported for a track that never produced sound, which is what
    /// keeps a resolve that failed out of the history as a play that happened.
    pub(super) fn finish_listening(&mut self) {
        let Some((track, listening)) = self.playback.take_listening() else {
            return;
        };
        if listening.heard.is_zero() {
            return;
        }
        journal::record_final(&track, listening.report(&track.id));
        let _ = self.source.send(Request::RetryReports);
    }


    /// Writes the play in progress straight to the journal, for the way out.
    ///
    /// The durable append uses the same nonce as the in-song checkpoint.
    /// Network delivery resumes on the next launch without delaying shutdown.
    pub fn flush_listening(&mut self) {
        let Some((track, listening)) = self.playback.take_listening() else {
            return;
        };
        if !listening.heard.is_zero() {
            journal::record_final(&track, listening.report(&track.id));
        }
    }


    /// Speculatively resolves the selected track once the selection has sat
    /// still for the selection debounce. Called once per frame.
    ///
    /// This removes the pooled player-API round trip from Enter when possible.
    /// Difficult tracks are deliberately not sent through yt-dlp here: a child
    /// process already running cannot be superseded by a later selection.
    pub fn tick_prefetch(&mut self) {
        // Never compete for the serial worker with a request the user is
        // waiting on, and never stack two speculative resolves.
        if self.awaiting() {
            return;
        }
        // Nothing playable is under the cursor while a modal has it, so there
        // is nothing worth warming.
        if self.overlay.is_open() || self.menu.is_some() {
            return;
        }
        if !self.prefetch.take_settled_selection(Instant::now()) {
            return;
        }

        let Some(id) = self.selected_id() else {
            return;
        };
        if self.prefetch.is_ready(&id) {
            return;
        }
        if self
            .source
            .send(Request::Prefetch { id: id.clone() })
            .is_ok()
        {
            self.prefetch.begin(id);
        }
    }


    /// The video id Enter would play right now, if Enter would play anything.
    ///
    /// Both browsing views can hold one, so both are worth warming -- a card on
    /// the landing page is exactly as likely to be the next thing played as a
    /// row in the results.
    pub(super) fn selected_id(&self) -> Option<String> {
        match self.view {
            View::Library => self.library.selected().and_then(|item| item.available.then_some(item)).and_then(|item| item.track.as_ref()).map(|track| track.id.clone()),
            View::Tracks => Some(self.selected_result_track()?.id.clone()),
            View::Home => match &self.home_card()?.target {
                Target::Play { video_id } => Some(video_id.clone()),
                // Opening one is a round trip of its own, and nothing about
                // which track it lands on is knowable from here.
                Target::Open { .. } | Target::Artist { .. } => None,
            },
            // The queue row under the cursor when there is one, and otherwise
            // whatever the queue will play next -- which is the track this view
            // is most likely to need resolved, since it plays itself.
            View::Playing => {
                let now = self.now.as_ref()?;
                let selected = (now.tab == Tab::UpNext)
                    .then(|| now.queue.get(now.cursor()))
                    .flatten();
                Some(selected.or_else(|| now.next_in_queue())?.id.clone())
            }
            View::Artist => {
                let artist = self.artist.as_ref()?;
                artist
                    .selected_track()
                    .map(|track| track.id.clone())
                    .or_else(|| {
                        artist.selected_card().and_then(|card| match &card.target {
                            Target::Play { video_id } => Some(video_id.clone()),
                            Target::Open { .. } | Target::Artist { .. } => None,
                        })
                    })
            }
        }
    }


    /// The player page.
    ///
    /// `h`/`l` walk the tabs, as they walk a shelf on the landing page, and the
    /// digits jump straight to one. `j`/`k` move inside whichever panel is
    /// open -- a row in the two list panels, a line in the two text ones. The
    /// arrow keys keep the meaning they have in the track list: left and right
    /// seek, because this is the view most likely to be open while a long track
    /// plays, and up and down move with `j`/`k`.
    pub(super) fn handle_playing_key(&mut self, key: KeyEvent) -> Result<()> {
        let current_tab = self.now.as_ref().map(|now| now.tab);
        let mut tab = None;
        if current_tab == Some(Tab::UpNext) && key.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::SHIFT)
            && matches!(key.code, KeyCode::Up | KeyCode::Down)
        {
            self.move_queue_selection(if key.code == KeyCode::Up { -1 } else { 1 });
            return Ok(());
        }

        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('l') | KeyCode::Tab => tab = current_tab.map(|tab| tab.shifted(1)),
            KeyCode::Char('h') | KeyCode::BackTab => tab = current_tab.map(|tab| tab.shifted(-1)),
            KeyCode::Char(digit @ '1'..='4') => {
                tab = Tab::ALL.get(digit as usize - '1' as usize).copied();
            }
            KeyCode::Char('j') | KeyCode::Down => self.scroll_page(1),
            KeyCode::Char('k') | KeyCode::Up => self.scroll_page(-1),
            KeyCode::PageDown => self.scroll_page(10),
            KeyCode::PageUp => self.scroll_page(-10),
            KeyCode::Char('g') | KeyCode::Home => self.jump_page(0),
            // No length is known here for the text panels, so this asks for a
            // line no panel has and lets the renderer -- which knows how long
            // each of them is -- clamp it to the last one.
            KeyCode::Char('G') | KeyCode::End => self.jump_page(usize::MAX),
            KeyCode::Enter => self.open_page_row(),
            KeyCode::Char('d') if current_tab == Some(Tab::UpNext) => self.remove_queue_selection(),
            KeyCode::Char('K') if current_tab == Some(Tab::UpNext) => self.move_queue_selection(-1),
            KeyCode::Char('J') if current_tab == Some(Tab::UpNext) => self.move_queue_selection(1),
            KeyCode::Char('C') if current_tab == Some(Tab::UpNext) => self.clear_upcoming_queue(),
            KeyCode::Char('z') if current_tab == Some(Tab::UpNext) => self.shuffle_upcoming_queue(),
            KeyCode::Char('e') if current_tab == Some(Tab::Lyrics) => self.toggle_lyrics_expanded(),
            KeyCode::Char('f') if current_tab == Some(Tab::Lyrics) => self.toggle_lyrics_follow(),
            KeyCode::Char('R') => self.cycle_repeat(),
            KeyCode::Char('n') => self.advance(1, false),
            KeyCode::Char('p') => self.advance(-1, false),
            KeyCode::Char(' ') => self.toggle_pause(),
            KeyCode::Char('m') => self.toggle_mute(),
            KeyCode::Char('s') => self.stop(),
            KeyCode::Char('c') => self.toggle_cover_size(),
            KeyCode::Char('+') | KeyCode::Char('=') => self.nudge_volume(VOLUME_STEP),
            KeyCode::Char('-') | KeyCode::Char('_') => self.nudge_volume(-VOLUME_STEP),
            KeyCode::Right => self.seek_relative(5),
            KeyCode::Left => self.seek_relative(-5),
            KeyCode::Char('M') => self.begin_music_sign_in(false),
            KeyCode::Char('B') => self.background(),
            KeyCode::Char('D') => self.toggle_presence(),
            KeyCode::Char('S') => self.open_settings(),
            KeyCode::Char('/') | KeyCode::Char('i') => self.begin_search(),
            // Back to the list the track was started from. The music keeps
            // playing -- leaving the page is not stopping it, and `P` brings it
            // back.
            KeyCode::Esc if self.now.as_ref().is_some_and(|now| now.tab == Tab::Lyrics && now.lyrics_expanded) => {
                self.toggle_lyrics_expanded();
            }
            KeyCode::Esc => self.return_from_player(),
            KeyCode::Char('H') => self.go_home(),
            _ => {}
        }

        // Applied in one place rather than in each arm above, so that every way
        // of changing tabs also starts the fetch behind the new one.
        if let (Some(now), Some(tab)) = (self.now.as_mut(), tab)
            && now.open(tab)
        {
            self.request_tab();
        }
        Ok(())
    }


    pub(super) fn toggle_lyrics_expanded(&mut self) {
        if let Some(now) = self.now.as_mut().filter(|now| now.tab == Tab::Lyrics) {
            now.lyrics_expanded = !now.lyrics_expanded;
            // A cover-only view must not conceal the requested reading view.
            self.cover_size = CoverSize::Side;
        }
    }


    pub(super) fn toggle_lyrics_follow(&mut self) {
        if let Some(now) = self.now.as_mut().filter(|now| now.tab == Tab::Lyrics)
            && matches!(&now.lyrics, Panel::Ready(lyrics) if !lyrics.timed.is_empty())
        {
            now.follow_lyrics = !now.follow_lyrics;
        }
    }


    /// Moves the cursor of the open tab, when there is a page to move it on.
    pub(super) fn scroll_page(&mut self, delta: isize) {
        if let Some(now) = self.now.as_mut() {
            now.scroll(delta);
        }
    }


    /// [`Self::scroll_page`] straight to a line, for `g` and `G`.
    pub(super) fn jump_page(&mut self, to: usize) {
        if let Some(now) = self.now.as_mut() {
            now.jump(to);
        }
    }


    /// Enter on the player page: plays the queue row or the recommendation
    /// under the cursor. The two text panels have nothing to open.
    pub(super) fn open_page_row(&mut self) {
        let Some(now) = self.now.as_ref() else {
            return;
        };
        match now.tab {
            Tab::UpNext => {
                if let Some(track) = now.queue.get(now.cursor()).cloned() {
                    self.play_track(track, false);
                }
            }
            Tab::Related => {
                let rows = now.related_rows();
                // Anything else under the cursor is a shelf heading, which
                // names the rows below it rather than standing for one.
                let Some(RelatedRow::Card(card)) = rows.get(now.cursor()) else {
                    return;
                };
                self.activate_card((*card).clone());
            }
            Tab::Lyrics | Tab::Comments => {}
        }
    }


    /// Stops playback and closes the player page.
    ///
    /// Shared by every view that offers `s`, and the reason the queue does not
    /// pick up afterwards: dropping the page is what tells [`Self::advance`]
    /// there is nothing to advance through. A stop that left the queue running
    /// would be a pause with extra steps.
    pub(super) fn stop(&mut self) {
        // Stopping halfway through is still having listened that far, and this
        // is the last point at which how far is known.
        self.finish_listening();
        let _ = self.player.send(Command::Stop);
        self.now = None;
        // A resolve may still be in flight; without this it would start playing
        // moments after the user asked for silence. A recovery in flight is the
        // same hazard by the other route.
        self.playback.stop();
        // Nothing is playing, so nothing owns the cover pane.
        self.cover = None;
        self.cover_id = None;
        if self.view == View::Playing {
            self.return_from_player();
        }
        self.status = "stopped".to_string();
    }


    pub(super) fn play_selected(&mut self) {
        if self.browsing.is_some() {
            self.play_collection(self.selected, false);
            return;
        }
        if let Some(item) = self.search_items.get(self.selected).cloned() {
            if let Some(track) = item.track {
                self.play_track(track, false);
            } else {
                self.activate_card(item.card);
            }
            return;
        }
        // Resolving takes seconds and spawns yt-dlp, so it goes to the worker
        // and playback starts when the response arrives -- unless a prefetch
        // already warmed the cache, in which case the round trip is all that is
        // left and saying "resolving" would be a lie the user can see through.
        if let Some(track) = self.results.get(self.selected).cloned() {
            self.play_track(track, false);
        }
    }


    pub(super) fn reload_collection(&mut self) {
        if self.page_loading() { return; }
        let Some(endpoint) = self.browsing_endpoint.clone() else { return; };
        let title = self.browsing.clone().unwrap_or_default();
        self.collection_error = None;
        self.status = "Loading songs…".into();
        let request_id = self.begin_page_request(PageRequestKind::Browse);
        if self.source.send(Request::OpenBrowse { request_id, endpoint, title }).is_err() {
            self.cancel_page_request();
            self.collection_error = Some("source worker is not running".to_owned());
        }
    }


    pub(super) fn play_collection(&mut self, start: usize, shuffle: bool) {
        if self.page_loading() || self.collection_error.is_some() { return; }
        let mut tracks = self.results.clone();
        if shuffle { shuffle_tracks(&mut tracks); }
        let position = if shuffle { 0 } else { start };
        let Some(first) = tracks.get(position).cloned() else { return; };
        let title = self.browsing.clone().unwrap_or_else(|| "Songs".to_owned());
        self.play_track(first, false);
        self.queue_epoch = self.queue_epoch.wrapping_add(1);
        if let Some(now) = self.now.as_mut() {
            now.queue_title = title;
            now.queue = tracks;
            now.queue_epoch = self.queue_epoch;
            now.continuation = None;
            now.topping_up = false;
            now.topup_failures = 0;
            now.dropped.clear();
            now.playing = Some(position);
            now.cursor[Tab::UpNext.index()] = position;
        }
    }


    pub(super) fn nudge_volume(&mut self, delta: f32) {
        let current = self.volume_save.map_or_else(|| self.snapshot().volume, |(_, volume)| volume);
        let step = delta.abs();
        self.set_volume(((current / step).round() * step + delta).clamp(0.0, 1.0));
    }


    pub(super) fn set_volume(&mut self, volume: f32) {
        let volume = volume.clamp(0.0, 1.0);
        if let Err(error) = self.player.send(Command::SetVolume(volume)) {
            self.status = format!("could not change volume: {error:#}");
            return;
        }
        self.muted = volume == 0.0;
        if !self.muted {
            self.volume_before_mute = volume;
        }
        self.volume_save = Some((Instant::now(), volume));
        self.status = if self.muted { "muted".into() } else { format!("volume {:.0}%", volume * 100.0) };
    }


    pub(super) fn flush_volume(&mut self) {
        let Some((_, volume)) = self.volume_save.take() else { return; };
        let settings = config::Settings {
            terminal_host: self.terminal_host, start_in_tray: self.start_in_tray,
            tray_click_pauses: self.tray_click_pauses,
            precision_volume: self.precision_volume,
            show_key_hints: self.show_key_hints,
            cover_style: self.cover_style,
            image_renderer: self.image_renderer, volume, output_device: self.output_device.clone() };
        if let Err(error) = settings.save() {
            crate::runtime::diagnostics::error("config", &format!("could not save volume: {error:#}"));
            self.status = "Could not save the volume setting.".into();
        }
    }


    pub(super) fn toggle_mute(&mut self) {
        let (target, remembered) = mute_transition(
            self.muted,
            self.snapshot().volume,
            self.volume_before_mute,
        );
        if let Err(error) = self.player.send(Command::SetVolume(target)) {
            self.status = format!("could not change mute state: {error:#}");
            return;
        }
        self.volume_before_mute = remembered;
        self.muted = !self.muted;
        self.status = if self.muted {
            "muted -- press m or click mut to restore volume".to_string()
        } else {
            format!("volume {:.0}%", target * 100.0)
        };
    }


    /// Seeks relative to the current position, clamped at zero.
    ///
    /// Seeking backwards beyond what remains in the ring buffer may fail; the
    /// player reports that through the snapshot rather than panicking.
    pub(super) fn seek_relative(&mut self, secs: i64) {
        let snap = self.snapshot();
        if snap.state == PlayState::Idle {
            return;
        }
        let target = if secs >= 0 {
            snap.position + Duration::from_secs(secs as u64)
        } else {
            snap.position
                .saturating_sub(Duration::from_secs(secs.unsigned_abs()))
        };
        let _ = self.player.send(Command::Seek(target));
        self.playback.seeked();
    }


    /// Seeks to an absolute point selected on the progress bar.
    pub(super) fn seek_to_fraction(&mut self, position: u16) {
        let Some(total) = self.now.as_ref().and_then(|now| now.duration) else {
            return;
        };
        let target = duration_at_pointer(total, position);
        let seek = if position >= POINTER_SCALE { Command::SeekToEnd(target) } else { Command::Seek(target) };
        // Load may not have published its snapshot yet; never seek the old
        // decoder or lose the user's intent while the new URL is resolving.
        let Some(seek) = self.playback.seek_or_defer(seek) else { return; };
        // Play is asynchronous too: its command may already be queued while
        // the last published snapshot still says Idle. The player can safely
        // ignore a seek with no decoder; the UI must not discard a valid one.
        let _ = self.player.send(seek);
        self.playback.seeked();
    }
}
