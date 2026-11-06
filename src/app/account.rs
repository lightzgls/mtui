//! Sign-in, renewal, and logout lifecycle.

use std::time::Instant;

use crate::runtime::config::Cookies;
use crate::terminal::graphics::art::ArtCache;
use crate::session::{RenewalFailure, renewal::Renewal};
use crate::source::worker::Request;

use crate::app::{App, Overlay, SignIn, View};

#[derive(Default, PartialEq, Eq)]
enum SessionTask {
    #[default]
    Idle,
    Renewing,
    ManualSignIn,
    AutomaticSignIn,
}

#[derive(Default)]
pub(super) struct SessionState {
    task: SessionTask,
    sign_in_prompted: bool,
    renewal: Renewal,
    renewal_scope: Option<String>,
}

impl SessionState {
    pub(super) fn observe_error(&mut self, error: &str) {
        self.renewal.observe_error(error);
    }
}

impl App {

    pub(super) fn session_pending(&self) -> bool {
        self.session.task != SessionTask::Idle
    }


    /// The same loop runs in the tray. Only the slow policy check touches disk;
    /// browser startup and renewal remain on a separate, bounded worker.
    pub(super) fn tick_session(&mut self) {
        if crate::session::take_authentication_failure() {
            self.session.renewal.authentication_failed();
        }
        if self.session_pending()
            || matches!(&self.overlay, Overlay::SavePlaylist(picker) if picker.loading || picker.saving)
            || self.pending_save.is_some()
            || self.now.as_ref().is_some_and(|now| now.like_pending)
            || !self.session.renewal.take_due()
        { return; }
        self.session.renewal_scope = Cookies::active_report_scope();
        self.session.task = SessionTask::Renewing;
        if self.source.send(Request::RenewMusicSession).is_err() {
            self.session.task = SessionTask::Idle;
            crate::runtime::diagnostics::warn("auth", "could not queue automatic session renewal");
        }
    }


    pub(super) fn begin_music_sign_in(&mut self, recover: bool) {
        if self.session_pending() {
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


    fn request_music_sign_in(&mut self, recover: bool) {
        self.session.task = SessionTask::ManualSignIn;
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
            self.session.task = SessionTask::Idle;
            let reason = "source worker is not running".to_string();
            self.status = reason.clone();
            self.overlay = Overlay::SignIn(SignIn::Failed { reason });
        }
    }


    pub(super) fn apply_session_import(&mut self, browser: String) {
        self.clear_account_pages();
        self.clear_account_actions();
        self.status = format!("read your YouTube session from {browser}");
        self.session.task = SessionTask::Idle;
        self.session_restored();
        let in_library = self.view == View::Library;
        self.reset_library();
        if in_library { self.open_library(); }
    }


    pub(super) fn apply_session_renewal(&mut self, result: Result<(), RenewalFailure>) {
        self.session.task = SessionTask::Idle;
        let previous_scope = self.session.renewal_scope.take();
        match result {
            Ok(()) => {
                crate::runtime::diagnostics::info("auth", "saved Music session renewed automatically");
                if previous_scope != Cookies::active_report_scope() {
                    // Cookie rotation cannot prove the profile still belongs to
                    // the same account. Drop derived data before showing it again.
                    self.clear_account_pages();
                    self.clear_account_actions();
                    self.reset_library();
                    if self.view == View::Library {
                        self.open_library();
                    }
                } else if self.view == View::Library {
                    self.refresh_library();
                }
                self.session_restored();
            }
            Err(RenewalFailure::SignInRequired) if !self.session.sign_in_prompted => {
                self.session.sign_in_prompted = true;
                self.status = "Google needs verification—finish in the sign-in window.".into();
                if self.source.send(Request::MusicSignIn { recover: true }).is_ok() {
                    self.session.task = SessionTask::AutomaticSignIn;
                }
            }
            Err(RenewalFailure::SignInRequired) => {}
            Err(RenewalFailure::Temporary(message)) => {
                crate::runtime::diagnostics::warn("auth", &format!("automatic renewal deferred: {message}"));
            }
        }
    }


    pub(super) fn apply_sign_in_failure(&mut self, message: String) {
        crate::runtime::diagnostics::error("auth", "YouTube Music sign-in failed");
        let task = std::mem::take(&mut self.session.task);
        if task == SessionTask::AutomaticSignIn {
            self.status = "Google sign-in was not completed; automatic recovery will keep checking.".into();
            return;
        }
        self.status = message.clone();
        if matches!(self.overlay, Overlay::Settings) {
            self.preferences.notice = Some((message, true));
        } else {
            self.menu = None;
            self.overlay = Overlay::SignIn(SignIn::Failed { reason: message });
        }
    }


    fn session_restored(&mut self) {
        self.session.sign_in_prompted = false;
        self.session.renewal.succeeded();
        let _ = self.source.send(Request::RetryReports);
        if self.now.as_ref().is_none_or(|now| !now.like_pending) {
            self.request_rating();
        }
        // Supersede any guest Home requests made before authentication.
        self.request_home();
    }


    fn clear_account_pages(&mut self) {
        let in_library = self.view == View::Library;
        if self.now.is_some() {
            self.stop();
        }
        self.go_home();
        if in_library {
            self.view = View::Library;
        }
        self.player_back = None;
        self.back_to = View::Home;
        self.search_origin = View::Home;
        self.query.clear();
        self.query_cursor = 0;
        self.results.clear();
        self.search_items.clear();
        self.browsing = None;
        self.browsing_endpoint = None;
        self.collection = None;
        self.collection_error = None;
        self.artist = None;
        self.selected = 0;
        self.offset = 0;
        self.menu = None;
        if !matches!(self.overlay, Overlay::Settings) {
            self.overlay = Overlay::None;
        }

        self.home_generation = self.home_generation.wrapping_add(1);
        self.home_pending = false;
        self.home.clear();
        self.home_scroll.clear();
        self.home_shelf = 0;
        self.home_card = 0;
        self.home_top = 0;
        self.prefetch.clear_selection();
        self.art = ArtCache::default();
        self.sync_art_resolution(self.cover_edge());
    }


    pub(super) fn log_out_music(&mut self) {
        self.clear_account_pages();
        self.reset_library();
        self.clear_account_actions();
        self.session.task = SessionTask::Idle;
        self.session.sign_in_prompted = false;
        // Clear the worker's in-memory copy even if removing credentials or
        // the durable outbox reports an error below.
        let _ = self.source.send(Request::ClearReports);
        match crate::session::sign_out() {
            Ok(warning) => {
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

#[cfg(test)]
mod tests {
    use crate::app::browse::navigation::{PageRequestKind, View};
    use crate::app::testing::scenarios::fixture;
    use crate::source::BrowseEndpoint;
    use crate::source::catalog::collection::{Details, Page};
    use crate::source::catalog::home::Shelf;
    use crate::source::catalog::library::browse::Category;
    use crate::source::worker::{Request, Response};

    #[test]
    fn session_switch_discards_private_collections_and_rejects_late_browse() {
        let (mut app, _driver, _requests) = fixture();
        let endpoint = BrowseEndpoint::new("VLLM");
        app.view = View::Tracks;
        app.browsing = Some("Private list".into());
        app.browsing_endpoint = Some(endpoint.clone());
        app.push_current_page();
        let request_id = app.begin_page_request(PageRequestKind::Browse);
        app.clear_account_pages();

        app.apply(Response::Browsed {
            request_id,
            endpoint,
            title: "Private list".into(),
            page: Box::new(Ok(Page {
                details: Details { title: "Private list".into(), ..Details::default() },
                tracks: Vec::new(),
            })),
        });
        assert_eq!(app.view, View::Home);
        assert!(app.history.is_empty() && app.browsing.is_none() && app.collection.is_none());
        assert!(!app.page_loading());
    }

    #[test]
    fn automatic_renewal_reloads_home_and_rejects_the_old_guest_feed() {
        let (mut app, _driver, requests) = fixture();
        app.view = View::Home;
        app.session.task = super::SessionTask::Renewing;
        app.home = vec![Shelf { title: "Guest Home".into(), cards: Vec::new() }];
        let old_generation = app.home_generation;
        app.apply(Response::SessionRenewed {
            generation: app.source.session_generation(), result: Ok(()),
        });
        assert!(!app.session_pending());
        assert_eq!(app.view, View::Home);
        assert!(app.home_pending);
        assert_ne!(app.home_generation, old_generation);
        assert!(requests.try_iter().any(|request| matches!(request,
            Request::PersonalHome { generation } if generation == app.home_generation)));

        app.apply(Response::Home { generation: old_generation, shelves: Vec::new() });
        app.apply(Response::HomeMore {
            generation: old_generation,
            shelves: vec![Shelf { title: "Late guest shelf".into(), cards: Vec::new() }],
        });
        assert_eq!(app.home.len(), 1);
        assert_eq!(app.home[0].title, "Guest Home");
        assert!(app.home_pending);

        app.apply(Response::Home {
            generation: app.home_generation,
            shelves: vec![Shelf { title: "Your Home".into(), cards: Vec::new() }],
        });
        assert_eq!(app.home[0].title, "Your Home");
        assert!(!app.home_pending);
    }

    #[test]
    fn automatic_renewal_retries_the_visible_library_without_resetting_its_category() {
        let (mut app, _driver, requests) = fixture();
        app.view = View::Library;
        app.library.connected = true;
        app.library.category = Category::Songs;
        app.library.collection_mut().endpoint.params = Some("chosen-sort".into());
        app.library.collection_mut().error = Some("Your YouTube Music session expired.".into());
        app.apply(Response::SessionRenewed {
            generation: app.source.session_generation(), result: Ok(()),
        });
        assert_eq!(app.view, View::Library);
        assert_eq!(app.library.category, Category::Songs);
        assert!(app.library.loading());
        assert!(app.library.collection().error.is_none());
        assert!(requests.try_iter().any(|request| matches!(request,
            Request::Library { category: Category::Songs, endpoint, .. }
                if endpoint.params.as_deref() == Some("chosen-sort"))));
    }

    #[test]
    fn failed_or_obsolete_renewal_does_not_reload_account_pages() {
        let (mut app, _driver, requests) = fixture();
        let home_generation = app.home_generation;
        app.apply(Response::SessionRenewed {
            generation: app.source.session_generation(),
            result: Err(crate::session::RenewalFailure::Temporary("offline".into())),
        });
        assert_eq!(app.home_generation, home_generation);
        assert!(requests.try_recv().is_err());

        let old_generation = app.source.session_generation();
        app.source.invalidate_account();
        app.apply(Response::SessionRenewed { generation: old_generation, result: Ok(()) });
        assert_eq!(app.home_generation, home_generation);
        assert!(requests.try_recv().is_err());
    }
}
