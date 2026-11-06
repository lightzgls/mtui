//! Playback lifecycle state. Service dispatch and journal writes stay in App.

use std::time::{Duration, Instant};

use crate::player::{Command, PlayState, Snapshot};
use crate::source::Track;
use crate::source::journal::PendingReport;
use crate::source::listening::Listening;

use super::MAX_AUTO_SKIPS;

/// A deferred seek belongs to exactly one unresolved track.
pub(in crate::app) struct PendingTrack {
    id: String,
    pub(in crate::app) seek: Option<Command>,
}

struct Recovery {
    id: String,
    from: Duration,
}

/// Transport bookkeeping shared by input, source responses and desktop controls.
/// Fields stay private so those callers use the same transition rules.
#[derive(Default)]
pub(in crate::app) struct PlaybackState {
    pending: Option<PendingTrack>,
    recovery: Option<Recovery>,
    failure: Option<String>,
    last_state: PlayState,
    automatic: bool,
    skips: u8,
    listening: Option<(Track, Listening)>,
}

impl PlaybackState {
    pub(in crate::app) fn pending_id(&self) -> Option<&str> {
        self.pending.as_ref().map(|pending| pending.id.as_str())
    }

    pub(in crate::app) fn is_loading(&self) -> bool {
        self.pending.is_some()
    }

    pub(in crate::app) fn is_recovering(&self) -> bool {
        self.recovery.is_some()
    }

    pub(in crate::app) fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    pub(in crate::app) fn fail(&mut self, summary: String) {
        self.failure = Some(summary);
    }

    pub(in crate::app) fn failed(&self, snapshot: &Snapshot) -> bool {
        !self.is_loading()
            && !self.is_recovering()
            && snapshot.state == PlayState::Idle
            && (self.failure.is_some() || snapshot.error.is_some())
    }

    /// Replace the request, recovery and listening identity together.
    pub(in crate::app) fn begin_track(&mut self, track: &Track, automatic: bool) {
        self.pending = Some(PendingTrack {
            id: track.id.clone(),
            seek: None,
        });
        self.listening = Some((track.clone(), Listening::new()));
        self.failure = None;
        self.automatic = automatic;
        self.recovery = None;
    }

    /// Only a matching answer releases this track's deferred seek.
    pub(in crate::app) fn accept_resolved(&mut self, id: &str) -> Option<PendingTrack> {
        if self.pending_id() != Some(id) {
            return None;
        }
        self.pending.take()
    }

    pub(in crate::app) fn cancel_loading(&mut self) {
        self.pending = None;
    }

    /// Returns an immediate command, or retains the latest seek until resolve.
    pub(in crate::app) fn seek_or_defer(&mut self, seek: Command) -> Option<Command> {
        match &mut self.pending {
            Some(pending) => {
                pending.seek = Some(seek);
                None
            }
            None => Some(seek),
        }
    }

    pub(in crate::app) fn begin_recovery(&mut self, id: String, from: Duration) {
        self.recovery = Some(Recovery { id, from });
    }

    pub(in crate::app) fn take_recovery(&mut self, id: &str) -> Option<Duration> {
        self.recovery
            .take_if(|recovery| recovery.id == id)
            .map(|recovery| recovery.from)
    }

    pub(in crate::app) fn resolved(&mut self) {
        self.skips = 0;
        self.automatic = false;
    }

    pub(in crate::app) fn skip_failed_track(&mut self) -> bool {
        if !self.automatic || self.skips >= MAX_AUTO_SKIPS {
            return false;
        }
        self.skips += 1;
        true
    }

    pub(in crate::app) fn end_automatic_playback(&mut self) {
        self.automatic = false;
    }

    /// Consume each clean EOF edge once, including a stream drained in Buffering.
    /// A late EOF must not finish the new play while its URL is still pending.
    pub(in crate::app) fn observe_end(&mut self, snapshot: &Snapshot) -> bool {
        let ended = matches!(self.last_state, PlayState::Playing | PlayState::Buffering)
            && snapshot.state == PlayState::Idle
            && snapshot.error.is_none()
            && self.failure.is_none()
            && !self.is_loading();
        self.last_state = snapshot.state;
        ended
    }

    pub(in crate::app) fn stop(&mut self) {
        self.automatic = false;
        self.pending = None;
        self.recovery = None;
        self.failure = None;
    }

    pub(in crate::app) fn listening_track(&self) -> Option<&Track> {
        self.listening.as_ref().map(|(track, _)| track)
    }

    pub(in crate::app) fn take_listening(&mut self) -> Option<(Track, Listening)> {
        self.listening.take()
    }

    /// Replace metadata without replacing this play's progress or report nonce.
    pub(in crate::app) fn hydrate_track(&mut self, canonical: Track) {
        if let Some((track, _)) = &mut self.listening
            && track.id == canonical.id
        {
            *track = canonical;
        }
    }

    pub(in crate::app) fn seeked(&mut self) {
        if let Some((_, listening)) = &mut self.listening {
            listening.seeked();
        }
    }

    /// The caller persists this checkpoint before acknowledging it below.
    pub(in crate::app) fn observe_listening(
        &mut self,
        snapshot: &Snapshot,
        now: Instant,
    ) -> Option<PendingReport> {
        let (track, listening) = self.listening.as_mut()?;
        listening.observe(snapshot.position, snapshot.state == PlayState::Playing, now);
        (!listening.reported && listening.heard >= Duration::from_secs(30))
            .then(|| listening.report(&track.id))
    }

    pub(in crate::app) fn checkpoint_sent(&mut self) {
        if let Some((_, listening)) = &mut self.listening {
            listening.reported = true;
            listening.checkpoint_sent();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_checkpoint_retries_until_acknowledged_and_keeps_identity_after_hydration() {
        let start = Instant::now();
        let mut state = PlaybackState::default();
        let mut track = Track {
            id: "song".into(),
            title: "Pasted URL".into(),
            uploader: "unknown".into(),
            duration: Some(Duration::from_secs(240)),
            album: None,
            artist_ref: None,
        };
        state.begin_track(&track, false);
        let snapshot = |seconds| Snapshot {
            state: PlayState::Playing,
            position: Duration::from_secs(seconds),
            ..Default::default()
        };
        assert!(state.observe_listening(&snapshot(0), start).is_none());
        let first = state
            .observe_listening(&snapshot(30), start + Duration::from_secs(30))
            .unwrap();
        track.title = "Canonical title".into();
        state.hydrate_track(track);
        let retry = state
            .observe_listening(&snapshot(31), start + Duration::from_secs(31))
            .unwrap();
        assert_eq!(retry.cpn, first.cpn);
        assert_eq!(retry.listened, 31);
        assert_eq!(retry.intervals[0].start_ms, 0);
        assert_eq!(retry.intervals[0].end_ms, 31_000);
        state.checkpoint_sent();
        assert!(
            state
                .observe_listening(&snapshot(32), start + Duration::from_secs(32))
                .is_none()
        );
        let (track, listening) = state.take_listening().unwrap();
        assert_eq!(track.title, "Canonical title");
        assert_eq!(listening.report(&track.id).cpn, first.cpn);
        assert_eq!(listening.heard, Duration::from_secs(32));
        assert!(state.take_listening().is_none());
    }
}
