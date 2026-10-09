//! Persistent player actions and dialogs, independent of the page underneath.

use super::*;
use crate::source::library::Playlist;

#[derive(Debug)]
pub struct PlaylistPicker {
    pub video_id: String,
    pub title: String,
    pub selected: usize,
    pub choices: Vec<Playlist>,
    pub loading: bool,
    pub saving: bool,
    pub error: Option<String>,
    pub notice: Option<String>,
    pub(crate) request_id: u64,
}

impl App {
    fn next_account_request(&mut self) -> u64 {
        self.account_request = self.account_request.wrapping_add(1);
        self.account_request
    }

    pub(super) fn request_rating(&mut self) {
        if config::Cookies::available().ok().flatten().is_none() {
            return;
        }
        let request_id = self.next_account_request();
        let Some(now) = self.now.as_mut() else { return };
        now.rating_request = request_id;
        let _ = self.source.send(Request::Rating {
            request_id,
            video_id: now.video_id.clone(),
        });
    }

    pub(super) fn like_playing(&mut self) {
        if config::Cookies::available().ok().flatten().is_none() {
            self.overlay = Overlay::Message {
                body: "Sign in from Menu → Account to like songs.".into(),
            };
            return;
        }
        if self.now.as_ref().is_none_or(|now| now.like_pending) {
            return;
        }
        let request_id = self.next_account_request();
        let now = self.now.as_mut().expect("current song");
        let liked = now.liked != Some(true);
        let req = Request::SetRating {
            request_id,
            video_id: now.video_id.clone(),
            liked,
        };
        if self.source.send(req).is_err() {
            self.status = "Account service is unavailable.".into();
            return;
        }
        now.rating_request = request_id;
        now.like_pending = true;
        self.status = if liked {
            "Liking song…"
        } else {
            "Removing Like…"
        }
        .into();
    }

    pub(super) fn apply_rating(
        &mut self,
        request_id: u64,
        video_id: &str,
        changed: bool,
        result: Result<bool, String>,
    ) {
        let Some(now) = self
            .now
            .as_mut()
            .filter(|now| now.video_id == video_id && now.rating_request == request_id)
        else {
            return;
        };
        now.like_pending = false;
        if let Err(error) = &result { self.session_renewal.observe_error(error); }
        match result {
            Ok(liked) => {
                now.liked = Some(liked);
                if changed {
                    self.status = if liked {
                        "Liked on YouTube Music."
                    } else {
                        "Like removed on YouTube Music."
                    }
                    .into();
                }
            }
            Err(error) if changed => {
                self.status = error.clone();
                if !self.overlay.is_open() {
                    self.menu = None;
                    self.overlay = Overlay::Message { body: error };
                }
            }
            Err(_) => {} // Unknown stays unknown; a failed read is not an unliked song.
        }
        if self
            .menu
            .as_ref()
            .is_some_and(|menu| menu.page == MenuPage::PlayerActions)
        {
            let items = self.player_action_items();
            if let Some(menu) = self.menu.as_mut() {
                menu.items = items;
            }
        }
    }

    pub(super) fn share_playing(&mut self) {
        if let Some(now) = &self.now {
            let query = form_urlencoded::Serializer::new(String::new())
                .append_pair("v", &now.video_id)
                .finish();
            self.overlay = Overlay::Share {
                url: format!("https://music.youtube.com/watch?{query}"),
                notice: None,
            };
        }
    }

    fn copy_share_link(&mut self) {
        if let Overlay::Share { url, notice } = &mut self.overlay {
            self.status = match crate::clipboard::copy(url) {
                Ok(()) => "Music link copied.".into(),
                Err(error) => error.to_string(),
            };
            *notice = Some(self.status.clone());
        }
    }

    pub(super) fn save_playing(&mut self) {
        if config::Cookies::available().ok().flatten().is_none() {
            self.overlay = Overlay::Message {
                body: "Sign in from Menu → Account to save songs.".into(),
            };
            return;
        }
        if self.pending_save.is_some() {
            self.status = "A playlist save is still in progress.".into();
            return;
        }
        let Some(now) = &self.now else { return };
        self.overlay = Overlay::SavePlaylist(Box::new(PlaylistPicker {
            video_id: now.video_id.clone(),
            title: now.title.clone(),
            selected: 0,
            choices: Vec::new(),
            loading: true,
            saving: false,
            error: None,
            notice: None,
            request_id: 0,
        }));
        self.reload_playlists();
    }

    fn reload_playlists(&mut self) {
        let request_id = self.next_account_request();
        let Overlay::SavePlaylist(picker) = &mut self.overlay else {
            return;
        };
        if picker.saving {
            return;
        }
        picker.request_id = request_id;
        picker.loading = true;
        picker.error = None;
        picker.notice = None;
        if self
            .source
            .send(Request::Playlists {
                request_id,
                video_id: picker.video_id.clone(),
            })
            .is_err()
        {
            picker.loading = false;
            picker.error = Some("Account service is unavailable.".into());
        }
    }

    pub(super) fn apply_playlists(
        &mut self,
        request_id: u64,
        choices: Result<Vec<Playlist>, String>,
    ) {
        let Overlay::SavePlaylist(picker) = &mut self.overlay else {
            return;
        };
        if picker.request_id != request_id {
            return;
        }
        picker.loading = false;
        if let Err(error) = &choices { self.session_renewal.observe_error(error); }
        match choices {
            Ok(choices) => {
                picker.choices = choices;
                picker.selected = picker.selected.min(picker.choices.len().saturating_sub(1));
            }
            Err(error) => picker.error = Some(error),
        }
    }

    fn choose_playlist(&mut self, index: usize) {
        let request_id = self.next_account_request();
        let Overlay::SavePlaylist(picker) = &mut self.overlay else {
            return;
        };
        if picker.loading || picker.saving || picker.error.is_some() {
            return;
        }
        let Some(choice) = picker.choices.get(index) else {
            return;
        };
        picker.selected = index;
        if choice.contains == Some(true) {
            picker.notice = Some("This song is already saved here.".into());
            return;
        }
        let req = Request::SaveToPlaylist {
            request_id,
            video_id: picker.video_id.clone(),
            playlist_id: choice.id.clone(),
        };
        if self.source.send(req).is_err() {
            picker.error = Some("Account service is unavailable.".into());
            return;
        }
        self.pending_save = Some((request_id, choice.title.clone()));
        picker.request_id = request_id;
        picker.saving = true;
        picker.notice = Some(format!("Saving to {}…", choice.title));
    }

    pub(super) fn apply_playlist_saved(&mut self, request_id: u64, result: Result<(), String>) {
        let Some((pending, title)) = &self.pending_save else {
            return;
        };
        if *pending != request_id {
            return;
        }
        let message = match &result {
            Ok(()) => format!("Saved to {title} on YouTube Music."),
            Err(error) => error.clone(),
        };
        if let Err(error) = &result { self.session_renewal.observe_error(error); }
        self.pending_save = None;
        self.status = message.clone();
        let mut liked_video = None;
        if let Overlay::SavePlaylist(picker) = &mut self.overlay
            && picker.request_id == request_id
        {
            picker.saving = false;
            if result.is_ok() {
                if let Some(choice) = picker.choices.get_mut(picker.selected) {
                    choice.contains = Some(true);
                    if choice.id.strip_prefix("VL").unwrap_or(&choice.id) == "LM" {
                        liked_video = Some(picker.video_id.clone());
                    }
                }
                picker.notice = Some(message);
            } else {
                picker.error = Some(message);
                picker.notice = None;
            }
        }
        if let Some(video_id) = liked_video {
            let request_id = self.next_account_request();
            if let Some(now) = self.now.as_mut().filter(|now|now.video_id == video_id && !now.like_pending) {
                now.rating_request = request_id;
                now.liked = Some(true);
            }
        }
    }

    pub(super) fn clear_account_actions(&mut self) {
        self.source.invalidate_account();
        let request_id = self.next_account_request();
        self.pending_save = None;
        if let Some(now) = self.now.as_mut() {
            now.rating_request = request_id;
            now.like_pending = false;
            now.liked = None;
        }
        if matches!(self.overlay, Overlay::SavePlaylist(_)) {
            self.overlay = Overlay::None;
        }
    }

    pub(super) fn choose_output(&mut self) {
        self.open_settings();
        self.handle_preferences_intent(preferences::Intent::Activate(preferences::Setting::Output));
    }

    pub(super) fn player_action_items(&self) -> Vec<MenuItem> {
        let current = self.now.is_some();
        let pending = self.now.as_ref().is_some_and(|now| now.like_pending);
        let liked = self.now.as_ref().is_some_and(|now| now.liked == Some(true));
        let signed = config::Cookies::available().ok().flatten().is_some();
        vec![
            MenuItem::action(
                if pending {
                    "Updating Like…"
                } else if liked {
                    "Remove Like"
                } else {
                    "Like"
                },
                Some("l"),
                current && signed && !pending,
                None,
                MenuAction::LikePlaying,
            ),
            MenuItem::action(
                "Share / Copy link",
                Some("s"),
                current,
                None,
                MenuAction::SharePlaying,
            ),
            MenuItem::action(
                "Save to playlist",
                Some("a"),
                current && signed && self.pending_save.is_none(),
                None,
                MenuAction::SavePlaying,
            ),
            MenuItem::action(
                "Shuffle upcoming",
                Some("z"),
                current,
                None,
                MenuAction::ShuffleUpcomingQueue,
            ),
            MenuItem::action(
                format!(
                    "Repeat: {}",
                    self.now.as_ref().map_or("Off", |now| now.repeat.label())
                ),
                Some("R"),
                current,
                None,
                MenuAction::CycleRepeat,
            ),
            MenuItem::action(
                "Output device",
                Some("o"),
                true,
                None,
                MenuAction::ChooseOutput,
            ),
        ]
    }

    pub(super) fn handle_player_dialog_mouse(&mut self, action: MouseAction) -> bool {
        if !matches!(
            self.overlay,
            Overlay::Share { .. } | Overlay::SavePlaylist(_)
        ) {
            return false;
        }
        match action {
            MouseAction::ClosePlayerDialog => self.overlay = Overlay::None,
            MouseAction::CopyShareLink => self.copy_share_link(),
            MouseAction::ChoosePlaylist(index) => self.choose_playlist(index),
            MouseAction::RetryPlaylists => self.reload_playlists(),
            _ => {}
        }
        true
    }

    pub(super) fn handle_player_dialog_key(&mut self, key: KeyEvent) -> bool {
        if !matches!(
            self.overlay,
            Overlay::Share { .. } | Overlay::SavePlaylist(_)
        ) {
            return false;
        }
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
            self.overlay = Overlay::None;
            return true;
        }
        if matches!(self.overlay, Overlay::Share { .. }) {
            if matches!(key.code, KeyCode::Enter | KeyCode::Char('c')) {
                self.copy_share_link();
            }
            return true;
        }
        let Overlay::SavePlaylist(picker) = &mut self.overlay else {
            return true;
        };
        let last = picker.choices.len().saturating_sub(1);
        match key.code {
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                picker.selected = (picker.selected + 1).min(last)
            }
            KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => {
                picker.selected = picker.selected.saturating_sub(1)
            }
            KeyCode::Home | KeyCode::Char('g') => picker.selected = 0,
            KeyCode::End | KeyCode::Char('G') => picker.selected = last,
            KeyCode::Enter => {
                let index = picker.selected;
                self.choose_playlist(index);
            }
            KeyCode::Char('r') => self.reload_playlists(),
            _ => {}
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "isolated zero-volume App state and mouse regression; no account writes"]
    fn player_actions_keep_requests_scoped_and_dialogs_owned() {
        let player = Player::spawn(0.0, None).unwrap();
        let source = SourceWorker::spawn(YouTubeForTest::default()).unwrap();
        let mut app = App::new(
            player,
            source,
            Graphics::blocks(),
            config::Settings::default(),
        );
        let track = Track {
            id: "first".into(),
            title: "First song".into(),
            uploader: "Artist".into(),
            artist_ref: None,
            album: None,
            duration: None,
        };
        app.now = Some(NowPlaying::new(&track));
        let now = app.now.as_mut().unwrap();
        now.rating_request = 9;
        now.liked = Some(false);
        now.like_pending = true;
        app.busy = true;
        app.apply(Response::Rating {
            request_id: 8,
            video_id: "first".into(),
            changed: true,
            result: Ok(true),
        });
        assert_eq!(app.now.as_ref().unwrap().liked, Some(false));
        assert!(app.now.as_ref().unwrap().like_pending && app.busy);
        app.apply(Response::Rating {
            request_id: 9,
            video_id: "wrong".into(),
            changed: true,
            result: Ok(true),
        });
        assert!(app.now.as_ref().unwrap().like_pending);
        app.apply(Response::Rating {
            request_id: 9,
            video_id: "first".into(),
            changed: true,
            result: Err("Server refused the change".into()),
        });
        assert_eq!(app.now.as_ref().unwrap().liked, Some(false));
        assert!(!app.now.as_ref().unwrap().like_pending);
        assert!(matches!(app.overlay, Overlay::Message { .. }));
        app.overlay = Overlay::None;
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL))
            .unwrap();
        assert_eq!(app.menu.as_ref().unwrap().page, MenuPage::PlayerActions);
        assert_eq!(app.menu_items().len(), 6);
        app.menu = None;
        app.handle_mouse_action(MouseAction::SharePlaying).unwrap();
        assert!(
            matches!(&app.overlay, Overlay::Share { url, .. } if url == "https://music.youtube.com/watch?v=first")
        );
        app.handle_mouse_action(MouseAction::GoHome).unwrap();
        assert!(
            matches!(app.overlay, Overlay::Share { .. }),
            "dialog owns clicks"
        );
        app.handle_mouse_action(MouseAction::ClosePlayerDialog)
            .unwrap();
        assert!(!app.overlay.is_open());
        app.overlay = Overlay::SavePlaylist(Box::new(PlaylistPicker {
            video_id: "first".into(),
            title: "First song".into(),
            selected: 0,
            choices: vec![Playlist {
                id: "p".into(),
                title: "Evenings".into(),
                contains: Some(false),
            }],
            loading: false,
            saving: true,
            error: None,
            notice: None,
            request_id: 12,
        }));
        app.pending_save = Some((12, "Evenings".into()));
        app.now.as_mut().unwrap().video_id = "second".into();
        app.handle_mouse_action(MouseAction::ChoosePlaylist(0))
            .unwrap();
        assert_eq!(
            app.pending_save.as_ref().unwrap().0,
            12,
            "pending save cannot be repeated"
        );
        app.apply(Response::PlaylistSaved {
            request_id: 11,
            result: Ok(()),
        });
        assert!(app.pending_save.is_some());
        app.apply(Response::PlaylistSaved {
            request_id: 12,
            result: Ok(()),
        });
        let Overlay::SavePlaylist(picker) = &app.overlay else {
            panic!("picker disappeared")
        };
        assert_eq!(
            picker.video_id, "first",
            "save remains bound to the chosen song"
        );
        assert!(picker.choices[0].contains == Some(true) && !picker.saving);
        app.clear_account_actions();
        assert!(app.pending_save.is_none() && !app.overlay.is_open());
        assert!(app.now.as_ref().unwrap().liked.is_none());
        assert!(app.listening.is_none());
    }

    use crate::source::youtube::YouTube as YouTubeForTest;
}
