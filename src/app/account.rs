//! Sign-in, renewal, and logout lifecycle.

use std::time::Instant;

use crate::art::ArtCache;
use crate::source::worker::Request;

use super::{App, Overlay, SignIn};

impl App {

    /// The same loop runs in the tray. Only the slow policy check touches disk;
    /// browser startup and renewal remain on a separate, bounded worker.
    pub(super) fn tick_session(&mut self) {
        if crate::session::take_authentication_failure() {
            self.session_renewal.authentication_failed();
        }
        if self.music_signing_in
            || matches!(&self.overlay, Overlay::SavePlaylist(picker) if picker.loading || picker.saving)
            || self.pending_save.is_some()
            || self.now.as_ref().is_some_and(|now| now.like_pending)
            || !self.session_renewal.take_due()
        { return; }
        self.music_signing_in = true;
        if self.source.send(Request::RenewMusicSession).is_err() {
            self.music_signing_in = false;
            crate::diagnostics::warn("auth", "could not queue automatic session renewal");
        }
    }


    pub(super) fn begin_music_sign_in(&mut self, recover: bool) {
        if self.music_signing_in {
            self.status = "the YouTube Music session import is already pending".to_string();
            return;
        }
        if self.overlay.is_open() && !matches!(self.overlay, Overlay::SignIn(SignIn::Failed { .. }))
        {
            self.status = "close the current panel, then press M to open sign-in".to_string();
            return;
        }
        self.menu = None;

        self.request_music_sign_in(recover);
    }


    pub(super) fn request_music_sign_in(&mut self, recover: bool) {
        self.automatic_sign_in = false;
        self.music_signing_in = true;
        self.status = if recover {
            "renewing the saved YouTube Music session ...".to_string()
        } else {
            "finish signing in in the YouTube Music window ...".to_string()
        };
        self.overlay = Overlay::SignIn(SignIn::Music {
            started: Instant::now(),
            recovering: recover,
        });
        if self.source.send(Request::MusicSignIn { recover }).is_err() {
            self.music_signing_in = false;
            let reason = "source worker is not running".to_string();
            self.status = reason.clone();
            self.overlay = Overlay::SignIn(SignIn::Failed { reason });
        }
    }


    pub(super) fn log_out_music(&mut self) {
        self.reset_library();
        self.clear_account_actions();
        self.music_signing_in = false;
        self.automatic_sign_in = false;
        self.sign_in_prompted = false;
        self.menu = None;
        // Clear the worker's in-memory copy even if removing credentials or
        // the durable outbox reports an error below.
        let _ = self.source.send(Request::ClearReports);
        match crate::session::sign_out() {
            Ok(warning) => {
                // Any personalized response already in flight belongs to the
                // session that just ended and must not repopulate Home.
                self.home_generation = self.home_generation.wrapping_add(1);
                self.home_attempts = 0;
                self.home_pending = false;
                self.home.clear();
                self.home_scroll.clear();
                self.home_shelf = 0;
                self.home_card = 0;
                self.home_top = 0;
                self.prefetch.clear_selection();
                self.art = ArtCache::default();
                self.status = warning.map_or_else(
                    || "logged out of YouTube Music -- loading guest Home".to_string(),
                    |warning| format!("logged out; {warning}"),
                );
                self.request_home();
            }
            Err(error) => {
                self.status = format!("could not log out of YouTube Music: {error:#}");
            }
        }
    }
}
