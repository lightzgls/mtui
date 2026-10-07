//! Listening measured from audio progress, excluding pauses, seeks and reconnects.

use super::journal::PendingReport;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Interval {
    pub start_ms: u64,
    pub end_ms: u64,
}

/// One play; its nonce survives every checkpoint and final report.
pub struct Listening {
    pub heard: Duration,
    previous: Option<(Duration, Instant, bool)>,
    position: Duration,
    intervals: Vec<Interval>,
    cpn: String,
    at: u64,
    pub reported: bool,
}

impl Listening {
    pub fn new() -> Self {
        Self {
            heard: Duration::ZERO,
            previous: None,
            position: Duration::ZERO,
            intervals: Vec::new(),
            cpn: super::stats::nonce(),
            at: super::sapisid::unix_now(),
            reported: false,
        }
    }

    pub fn observe(&mut self, position: Duration, playing: bool, now: Instant) {
        if let Some((previous, sampled, was_playing)) = self.previous
            && was_playing && playing
            && let Some(delta) = position.checked_sub(previous)
            && !delta.is_zero()
            // A discontinuity is a seek, not time heard. Allow clock jitter.
            && delta <= now.saturating_duration_since(sampled) + Duration::from_millis(250)
        {
            self.heard += delta;
            let start_ms = previous.as_millis() as u64;
            let end_ms = position.as_millis() as u64;
            if let Some(last) = self.intervals.last_mut()
                && last.end_ms == start_ms
            {
                last.end_ms = end_ms;
            } else if self.intervals.len() < 256 {
                self.intervals.push(Interval { start_ms, end_ms });
            }
        }
        // Idle resets position to zero; retain the last real media position.
        if playing || !position.is_zero() {
            self.position = position;
        }
        self.previous = Some((position, now, playing));
    }

    pub fn seeked(&mut self) {
        self.previous = None;
    }
    /// Future beacons carry only progress after this checkpoint. Pending
    /// checkpoints are merged by the durable outbox if not yet delivered.
    pub fn checkpoint_sent(&mut self) {
        self.intervals.clear();
    }

    pub fn report(&self, video_id: &str) -> PendingReport {
        PendingReport {
            video_id: video_id.to_string(),
            listened: self.heard.as_secs(),
            at: self.at,
            cpn: self.cpn.clone(),
            intervals: self.intervals.clone(),
            position_ms: Some(self.position.as_millis() as u64),
            revision: self.heard.as_millis() as u64,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample(
        listening: &mut Listening,
        clock: Instant,
        elapsed: u64,
        position: u64,
        playing: bool,
    ) {
        listening.observe(
            Duration::from_secs(position),
            playing,
            clock + Duration::from_secs(elapsed),
        );
    }

    #[test]
    fn seeks_pauses_and_reconnections_do_not_invent_listening() {
        let clock = Instant::now();
        let mut listening = Listening::new();
        sample(&mut listening, clock, 0, 0, true);
        sample(&mut listening, clock, 10, 10, true);
        listening.seeked();
        sample(&mut listening, clock, 11, 100, true);
        sample(&mut listening, clock, 16, 105, true);
        sample(&mut listening, clock, 17, 105, false);
        sample(&mut listening, clock, 100, 105, true);
        sample(&mut listening, clock, 105, 110, true);
        sample(&mut listening, clock, 106, 2, true);
        sample(&mut listening, clock, 111, 7, true);
        assert_eq!(listening.heard, Duration::from_secs(25));
        assert_eq!(
            listening.report("video").intervals,
            vec![
                Interval {
                    start_ms: 0,
                    end_ms: 10_000
                },
                Interval {
                    start_ms: 100_000,
                    end_ms: 110_000
                },
                Interval {
                    start_ms: 2_000,
                    end_ms: 7_000
                }
            ]
        );
        assert!(listening.heard < super::super::stats::MIN_REPORTABLE);
    }

    #[test]
    fn checkpoint_and_finish_share_identity_and_real_ranges() {
        let clock = Instant::now();
        let mut listening = Listening::new();
        sample(&mut listening, clock, 0, 0, true);
        sample(&mut listening, clock, 30, 30, true);
        let checkpoint = listening.report("video");
        sample(&mut listening, clock, 60, 60, true);
        sample(&mut listening, clock, 61, 0, false);
        let final_report = listening.report("video");
        assert_eq!(checkpoint.cpn, final_report.cpn);
        assert_eq!(checkpoint.at, final_report.at);
        assert_eq!(final_report.listened, 60);
        assert_eq!(final_report.position_ms, Some(60_000));
        assert_eq!(
            final_report.intervals,
            vec![Interval {
                start_ms: 0,
                end_ms: 60_000
            }]
        );
    }

    #[test]
    fn unsampled_seek_jump_is_rejected() {
        let clock = Instant::now();
        let mut listening = Listening::new();
        sample(&mut listening, clock, 0, 0, true);
        sample(&mut listening, clock, 1, 200, true);
        sample(&mut listening, clock, 2, 201, true);
        assert_eq!(listening.heard, Duration::from_secs(1));
    }
}
