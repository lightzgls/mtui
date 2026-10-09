//! Slow, persistent scheduling for automatic browser-session renewal.
//!
//! Timestamps describe our attempts, not Google's cookie validity. The UI only
//! checks this policy once a minute; the browser runs on demand and closes.

use std::time::{Duration, Instant};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::{self, Import};

pub const INTERVAL: u64 = 48 * 60 * 60;
const CHECK_INTERVAL: Duration = Duration::from_secs(60);
const RETRY_BASE: u64 = 60 * 60;
const RETRY_MAX: u64 = 24 * 60 * 60;
const STATE_FILE: &str = "session-renewal.json";

#[derive(Default, Serialize, Deserialize)]
struct Attempts {
    last_attempt: u64,
    failures: u32,
}

impl Attempts {
    fn due(&self, now: u64, imported_at: u64, expired: bool) -> bool {
        // Clamp future timestamps, including a wall clock corrected backwards.
        let success_age = now.saturating_sub(imported_at.min(now));
        let attempt_age = now.saturating_sub(self.last_attempt.min(now));
        let retry = RETRY_BASE
            .saturating_mul(1 << self.failures.saturating_sub(1).min(5))
            .min(RETRY_MAX);
        (expired || success_age >= INTERVAL) && (self.last_attempt == 0 || attempt_age >= retry)
    }
}

pub struct Renewal {
    attempts: Attempts,
    next_check: Instant,
    expired: bool,
}

impl Default for Renewal {
    fn default() -> Self {
        let attempts = config::dir()
            .ok()
            .and_then(|dir| std::fs::read(dir.join(STATE_FILE)).ok())
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .unwrap_or_default();
        Self {
            attempts,
            next_check: Instant::now() + CHECK_INTERVAL,
            expired: false,
        }
    }
}

impl Renewal {
    pub fn take_due(&mut self) -> bool {
        let now = Instant::now();
        if now < self.next_check {
            return false;
        }
        self.next_check = now + CHECK_INTERVAL;
        // Hand-pasted cookies have no app-owned browser session to renew.
        let Some(imported) = Import::load() else {
            return false;
        };
        if !owns_profile(&imported.browser) {
            return false;
        }
        let Ok(dir) = config::dir() else {
            return false;
        };
        if !dir.join("webview").is_dir() {
            return false;
        }
        let at = super::sapisid::unix_now();
        if !self.attempts.due(at, imported.at, self.expired) {
            return false;
        }
        // Record before spawning: a crash/relaunch must not start a tight loop.
        self.attempts.last_attempt = at;
        self.attempts.failures = self.attempts.failures.saturating_add(1);
        self.persist();
        true
    }

    pub fn succeeded(&mut self) {
        // Success is not permission to open another browser immediately if a
        // broken endpoint continues reporting 401 with freshly captured cookies.
        self.attempts = Attempts {
            last_attempt: super::sapisid::unix_now(),
            failures: 0,
        };
        self.expired = false;
        self.next_check = Instant::now() + CHECK_INTERVAL;
        self.persist();
    }

    pub fn authentication_failed(&mut self) {
        self.expired = true;
        self.next_check = Instant::now();
    }

    pub fn observe_error(&mut self, error: &str) {
        // Only our explicit authentication classification can bring renewal
        // forward. Media 403s, playlist permissions and rate limits cannot.
        if error.starts_with("Your YouTube Music session expired.") {
            self.authentication_failed();
        }
    }

    fn persist(&self) {
        let save = || -> Result<()> {
            let path = config::dir()?.join(STATE_FILE);
            config::write_atomic(&path, &serde_json::to_vec(&self.attempts)?)
        };
        if save().is_err() {
            crate::diagnostics::warn("auth", "could not persist automatic renewal cooldown");
        }
    }
}

fn owns_profile(browser: &str) -> bool {
    matches!(browser, super::SESSION_NAME | "MTUI WebView2")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_imports_do_not_inherit_an_unrelated_browser_profile() {
        assert!(owns_profile(crate::session::SESSION_NAME));
        assert!(owns_profile("MTUI WebView2"));
        assert!(!owns_profile("Chrome"));
        assert!(!owns_profile("manual"));
    }

    #[test]
    fn fresh_sessions_are_left_alone_for_48_hours() {
        let attempts = Attempts::default();
        let imported_at = 1_700_000_000;
        assert!(!attempts.due(imported_at + INTERVAL - 1, imported_at, false));
        assert!(attempts.due(imported_at + INTERVAL, imported_at, false));
    }

    #[test]
    fn failed_attempts_back_off_across_restarts() {
        let at = 1_700_000_000;
        for (failures, wait) in [
            (1, RETRY_BASE),
            (2, 2 * RETRY_BASE),
            (3, 4 * RETRY_BASE),
            (9, RETRY_MAX),
        ] {
            let saved = serde_json::to_vec(&Attempts {
                last_attempt: at,
                failures,
            })
            .unwrap();
            let attempts: Attempts = serde_json::from_slice(&saved).unwrap();
            assert!(!attempts.due(at + wait - 1, at - INTERVAL, false));
            assert!(attempts.due(at + wait, at - INTERVAL, false));
        }
    }

    #[test]
    fn a_manual_refresh_defers_automatic_renewal() {
        let at = 1_700_000_000;
        let attempts = Attempts {
            last_attempt: at - RETRY_BASE,
            failures: 1,
        };
        assert!(!attempts.due(at, at, false));
        assert!(attempts.due(at + INTERVAL, at, false));
    }

    #[test]
    fn backwards_clock_changes_do_not_trigger_repeated_renewal() {
        let at = 1_700_000_000;
        let attempts = Attempts {
            last_attempt: at + 60,
            failures: 1,
        };
        assert!(!attempts.due(at, at - INTERVAL, false));
        assert!(!Attempts::default().due(at, at + INTERVAL, false));
    }

    #[test]
    fn expiry_recovers_early_without_bypassing_failure_cooldowns() {
        let at = 1_700_000_000;
        assert!(Attempts::default().due(at, at, true));
        let attempts = Attempts {
            last_attempt: at,
            failures: 1,
        };
        assert!(!attempts.due(at + 30, at, true));
        assert!(attempts.due(at + RETRY_BASE, at, true));
    }

    #[test]
    fn a_success_followed_by_another_401_cannot_loop_browser_startup() {
        let at = 1_700_000_000;
        let attempts = Attempts {
            last_attempt: at,
            failures: 0,
        };
        assert!(!attempts.due(at + 60, at, true));
        assert!(attempts.due(at + RETRY_BASE, at, true));
    }

    #[test]
    fn permission_and_transport_errors_do_not_force_session_renewal() {
        let mut renewal = Renewal {
            attempts: Attempts::default(),
            next_check: Instant::now(),
            expired: false,
        };
        for error in [
            "HTTP 403",
            "Your account changed while this action was waiting. Try again.",
            "YouTube Music denied permission for this action. HTTP 403",
            "HTTP 429",
            "HTTP 500",
        ] {
            renewal.observe_error(error);
            assert!(!renewal.expired);
        }
        renewal.observe_error("Your YouTube Music session expired. Sign in again. HTTP 401");
        assert!(renewal.expired);
    }
}
