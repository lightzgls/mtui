//! Selection debounce and the source worker's speculative cache knowledge.

use std::time::{Duration, Instant};

/// Avoid resolving every row while the user is scrolling through a list.
const PREFETCH_IDLE: Duration = Duration::from_millis(400);

#[derive(Default)]
pub(super) struct PrefetchState {
    selection_settled: Option<Instant>,
    in_flight: Option<String>,
    ready: Option<String>,
}

impl PrefetchState {
    pub(super) fn selection_changed(&mut self, now: Instant) {
        self.selection_settled = Some(now);
    }

    pub(super) fn clear_selection(&mut self) {
        self.selection_settled = None;
    }

    /// Consume a settled selection once, while allowing only one pending probe.
    pub(super) fn take_settled_selection(&mut self, now: Instant) -> bool {
        if self.in_flight.is_some()
            || self
                .selection_settled
                .is_none_or(|settled| now.saturating_duration_since(settled) < PREFETCH_IDLE)
        {
            return false;
        }
        self.selection_settled = None;
        true
    }

    pub(super) fn is_ready(&self, id: &str) -> bool {
        self.ready.as_deref() == Some(id)
    }

    pub(super) fn begin(&mut self, id: String) {
        self.in_flight = Some(id);
    }

    /// Queue and selection probes share responses but only replace their own
    /// in-flight slot or cached-id knowledge when an answer names the same id.
    pub(super) fn apply(&mut self, id: String, ready: bool) {
        if self.in_flight.as_deref() == Some(id.as_str()) {
            self.in_flight = None;
        }
        if ready {
            self.ready = Some(id);
        } else if self.is_ready(&id) {
            self.ready = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrolling_and_an_in_flight_probe_keep_only_the_latest_selection_due() {
        let start = Instant::now();
        let mut state = PrefetchState::default();
        state.selection_changed(start);
        state.selection_changed(start + Duration::from_millis(300));
        assert!(!state.take_settled_selection(start + Duration::from_millis(400)));
        assert!(state.take_settled_selection(start + Duration::from_millis(700)));
        state.begin("first".into());
        state.selection_changed(start + Duration::from_millis(800));
        assert!(!state.take_settled_selection(start + Duration::from_secs(2)));
        state.apply("queue-track".into(), true);
        assert!(!state.take_settled_selection(start + Duration::from_secs(2)));
        state.apply("first".into(), false);
        assert!(state.take_settled_selection(start + Duration::from_secs(2)));
        assert!(!state.take_settled_selection(start + Duration::from_secs(3)));
        assert!(state.is_ready("queue-track"));
    }

    #[test]
    fn a_late_failure_cannot_erase_another_tracks_warm_url() {
        let mut state = PrefetchState::default();
        state.apply("selection".into(), true);
        state.apply("old-queue-track".into(), false);
        assert!(state.is_ready("selection"));
        state.apply("queue-track".into(), true);
        state.apply("selection".into(), false);
        assert!(state.is_ready("queue-track"));
        state.apply("queue-track".into(), false);
        assert!(!state.is_ready("queue-track"));
    }
}
