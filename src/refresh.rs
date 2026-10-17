//! Redraw only when visible playback changes, input/work arrives, or an
//! animation needs its next frame. Worker polling remains independent.
use std::time::{Duration, Instant};

use crate::player::Snapshot;

#[derive(PartialEq)]
struct Stamp {
    playback: Snapshot,
    lyric: Option<usize>,
}

pub struct Refresh {
    last: Option<Stamp>,
    drawn_at: Instant,
}

impl Refresh {
    pub fn new(now: Instant) -> Self {
        Self {
            last: None,
            drawn_at: now,
        }
    }

    pub fn due(
        &mut self,
        mut playback: Snapshot,
        lyric: Option<usize>,
        invalidated: bool,
        animated: bool,
        now: Instant,
    ) -> bool {
        // The displayed clock has whole seconds; lyric cues retain their own
        // precision and can still trigger a redraw between those seconds.
        playback.position = Duration::from_secs(playback.position.as_secs());
        let stamp = Stamp { playback, lyric };
        let interval = if animated {
            Duration::from_millis(200)
        } else {
            Duration::from_secs(1)
        };
        if invalidated
            || self.last.as_ref() != Some(&stamp)
            || now.saturating_duration_since(self.drawn_at) >= interval
        {
            self.last = Some(stamp);
            self.drawn_at = now;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::PlayState;

    #[test]
    fn polling_skips_unchanged_frames_but_cues_seeks_and_controls_draw_immediately() {
        let start = Instant::now();
        let mut refresh = Refresh::new(start);
        let mut snap = Snapshot {
            state: PlayState::Playing,
            ..Snapshot::default()
        };
        assert!(refresh.due(snap.clone(), None, false, false, start));
        for millis in [50, 100, 200, 400, 800] {
            snap.position = Duration::from_millis(millis);
            assert!(!refresh.due(snap.clone(), None, false, false, start + snap.position));
        }
        assert!(refresh.due(
            snap.clone(),
            Some(0),
            false,
            false,
            start + Duration::from_millis(850)
        ));
        snap.position = Duration::from_millis(900);
        assert!(refresh.due(snap.clone(), Some(1), false, false, start + snap.position));
        snap.state = PlayState::Paused;
        assert!(refresh.due(snap.clone(), Some(1), false, false, start + snap.position));
        snap.volume = 0.5;
        assert!(refresh.due(snap.clone(), Some(1), false, false, start + snap.position));
        snap.position = Duration::from_secs(30);
        assert!(refresh.due(
            snap.clone(),
            Some(7),
            false,
            false,
            start + Duration::from_secs(1)
        ));
        assert!(refresh.due(snap, Some(1), true, false, start + Duration::from_secs(1)));
    }

    #[test]
    fn overflowing_titles_keep_their_animation_cadence() {
        let start = Instant::now();
        let mut refresh = Refresh::new(start);
        let snap = Snapshot::default();
        assert!(refresh.due(snap.clone(), None, false, true, start));
        assert!(!refresh.due(
            snap.clone(),
            None,
            false,
            true,
            start + Duration::from_millis(50)
        ));
        assert!(refresh.due(
            snap.clone(),
            None,
            false,
            true,
            start + Duration::from_millis(200)
        ));
        assert!(!refresh.due(
            snap.clone(),
            None,
            false,
            false,
            start + Duration::from_millis(400)
        ));
        assert!(refresh.due(
            snap,
            None,
            false,
            false,
            start + Duration::from_millis(1_200)
        ));
    }
}
