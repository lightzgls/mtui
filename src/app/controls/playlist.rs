//! Playlist edits are submitted once through the account worker.

use crossterm::event::{KeyCode, KeyEvent};

use crate::app::{App, MouseAction, Overlay, View};
use crate::app::browse::search_input;
use crate::app::controls::actions::PlaylistPicker;
use crate::source::catalog::library::{PlaylistChange, playlist_title};
use crate::source::worker::Request;

pub struct Editor {
    change: PlaylistChange,
    pub request_id: Option<u64>,
    pub cursor: usize,
    pub error: Option<String>,
    pub prompt: String,
    picker: Option<Box<PlaylistPicker>>,
}

impl Editor {
    pub fn heading(&self) -> &'static str {
        match self.change {
            PlaylistChange::Create { .. } => "New playlist",
            PlaylistChange::Rename { .. } => "Rename playlist",
            PlaylistChange::Remove { .. } => "Remove from playlist",
            PlaylistChange::MoveBefore { .. } => "Move playlist track",
        }
    }

    pub fn submit_label(&self) -> &'static str {
        match self.change {
            PlaylistChange::Create { .. } => "Create",
            PlaylistChange::Rename { .. } => "Save",
            PlaylistChange::Remove { .. } => "Remove",
            PlaylistChange::MoveBefore { .. } => "Move",
        }
    }

    pub fn title(&self) -> Option<&str> {
        match &self.change {
            PlaylistChange::Create { title, .. } | PlaylistChange::Rename { title, .. } => Some(title),
            _ => None,
        }
    }
}

impl App {
    fn open_playlist_editor(&mut self, change: PlaylistChange, prompt: String) {
        let mut editor = Editor { change, request_id: None, cursor: 0, error: None, prompt, picker: None };
        editor.cursor = editor.title().map_or(0, str::len);
        self.overlay = Overlay::EditPlaylist(Box::new(editor));
    }

    pub(in crate::app) fn new_playlist(&mut self, video_id: Option<String>) {
        let prompt = if video_id.is_some() {
            "Create a private playlist with this song."
        } else {
            "Create a private playlist on YouTube Music."
        };
        self.open_playlist_editor(PlaylistChange::Create { title: String::new(), video_id }, prompt.into());
    }

    pub(in crate::app) fn new_playlist_for_song(&mut self) {
        if matches!(&self.overlay, Overlay::SavePlaylist(picker) if !picker.loading && !picker.saving)
            && let Overlay::SavePlaylist(picker) = std::mem::replace(&mut self.overlay, Overlay::None)
        {
            self.new_playlist(Some(picker.video_id.clone()));
            if let Overlay::EditPlaylist(editor) = &mut self.overlay { editor.picker = Some(picker); }
        }
    }

    fn close_playlist_editor(&mut self) {
        if let Overlay::EditPlaylist(editor) = &mut self.overlay {
            self.overlay = editor.picker.take().map_or(Overlay::None, Overlay::SavePlaylist);
        }
    }

    fn editable_playlist(&self) -> Option<(&str, &[Option<String>])> {
        if self.view != View::Tracks || self.page_loading() || self.collection_error.is_some() {
            return None;
        }
        let id = self.browsing_endpoint.as_ref()?.browse_id.strip_prefix("VL")?;
        if id == "LM" || id.is_empty() { return None; }
        Some((id, self.collection.as_ref()?.editable_entries.as_deref()?))
    }

    pub(in crate::app) fn can_rename_playlist(&self) -> bool {
        self.editable_playlist().is_some()
    }

    pub(in crate::app) fn can_remove_playlist_entry(&self) -> bool {
        self.editable_playlist().and_then(|(_, entries)| entries.get(self.selected))
            .is_some_and(Option::is_some)
    }

    fn playlist_move(&self, down: bool) -> Option<(&str, &str, &str)> {
        let (id, entries) = self.editable_playlist()?;
        let other = if down { self.selected.checked_add(1)? } else { self.selected.checked_sub(1)? };
        let current = entries.get(self.selected)?.as_deref()?;
        let adjacent = entries.get(other)?.as_deref()?;
        (current != adjacent).then_some(if down { (id, adjacent, current) } else { (id, current, adjacent) })
    }

    pub(in crate::app) fn can_move_playlist_entry(&self, down: bool) -> bool {
        self.playlist_move(down).is_some()
    }

    pub(in crate::app) fn rename_playlist(&mut self) {
        let Some((id, _)) = self.editable_playlist() else { return; };
        let Some(details) = &self.collection else { return; };
        self.open_playlist_editor(PlaylistChange::Rename { id: id.into(), title: details.title.clone() },
            "Change the name on YouTube Music.".into());
    }

    pub(in crate::app) fn remove_playlist_entry(&mut self) {
        let Some((id, entries)) = self.editable_playlist() else { return; };
        let Some(Some(entry_id)) = entries.get(self.selected) else { return; };
        let Some(track) = self.results.get(self.selected) else { return; };
        self.open_playlist_editor(PlaylistChange::Remove {
            id: id.into(), video_id: track.id.clone(), entry_id: entry_id.clone(),
        }, format!("Remove {} from this YouTube Music playlist?", track.title));
    }

    pub(in crate::app) fn move_playlist_entry(&mut self, down: bool) {
        let Some((id, entry_id, successor)) = self.playlist_move(down) else { return; };
        self.open_playlist_editor(PlaylistChange::MoveBefore {
            id: id.into(), entry_id: entry_id.into(), successor: successor.into(),
        }, "Change the order on YouTube Music.".into());
        self.submit_playlist_edit();
    }

    fn submit_playlist_edit(&mut self) {
        let request_id = self.next_account_request();
        let Overlay::EditPlaylist(editor) = &mut self.overlay else { return; };
        if editor.request_id.is_some() { return; }
        if let Some(title) = editor.title() && let Err(error) = playlist_title(title) {
            editor.error = Some(error.to_string());
            return;
        }
        match self.source.send(Request::ChangePlaylist { request_id, change: editor.change.clone() }) {
            Ok(()) => {
                editor.request_id = Some(request_id);
                editor.error = None;
            }
            Err(error) => editor.error = Some(error.to_string()),
        }
    }

    pub(in crate::app) fn playlist_editor_key(&mut self, key: KeyEvent) {
        let Overlay::EditPlaylist(editor) = &mut self.overlay else { return; };
        if editor.request_id.is_some() { return; }
        match key.code {
            KeyCode::Esc => self.close_playlist_editor(),
            KeyCode::Enter => self.submit_playlist_edit(),
            _ => {
                if let PlaylistChange::Create { title, .. } | PlaylistChange::Rename { title, .. } = &mut editor.change
                    && search_input::edit(title, &mut editor.cursor, key) == Some(true)
                {
                    editor.error = None;
                }
            }
        }
    }

    pub(in crate::app) fn playlist_editor_mouse(&mut self, action: MouseAction) {
        if !matches!(&self.overlay, Overlay::EditPlaylist(editor) if editor.request_id.is_none()) { return; }
        match action {
            MouseAction::ClosePlayerDialog => self.close_playlist_editor(),
            MouseAction::SubmitPlaylistEdit => self.submit_playlist_edit(),
            _ => {}
        }
    }

    pub(in crate::app) fn apply_playlist_change(&mut self, request_id: u64, result: Result<(), String>) {
        let Overlay::EditPlaylist(editor) = &mut self.overlay else { return; };
        if editor.request_id != Some(request_id) { return; }
        editor.request_id = None;
        if let Err(error) = result {
            self.session.observe_error(&error);
            editor.error = Some(error);
            return;
        }
        if let PlaylistChange::MoveBefore { entry_id, successor, .. } = &editor.change
            && let Some(entries) = self.collection.as_ref().and_then(|details| details.editable_entries.as_ref())
            && let Some(selected) = entries.get(self.selected).and_then(Option::as_deref)
        {
            if selected == entry_id { self.selected = self.selected.saturating_sub(1); }
            else if selected == successor { self.selected = self.selected.saturating_add(1); }
        }
        let (refresh_library, refresh_page, message) = match &editor.change {
            PlaylistChange::Create { video_id, .. } => (true, false, if video_id.is_some() {
                "Playlist created with this song on YouTube Music."
            } else { "Playlist created on YouTube Music." }),
            PlaylistChange::Rename { .. } => (true, true, "Playlist renamed on YouTube Music."),
            PlaylistChange::Remove { .. } => (false, true, "Song removed from playlist."),
            PlaylistChange::MoveBefore { .. } => (false, true, "Playlist order updated."),
        };
        self.overlay = Overlay::None;
        if refresh_library { self.refresh_playlist_library(); }
        if refresh_page { self.reload_collection(); }
        self.status = message.into();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use crate::source::{BrowseEndpoint, Track};
    use crate::source::catalog::collection::Details;
    use crate::source::worker::Response;

    fn key(code: KeyCode) -> KeyEvent { KeyEvent::new(code, KeyModifiers::NONE) }

    #[test]
    fn create_from_save_keeps_the_original_song_and_sends_only_once() {
        let (mut app, _, requests) = crate::app::testing::scenarios::fixture();
        app.overlay = Overlay::SavePlaylist(Box::new(PlaylistPicker {
            video_id: "original-song".into(), title: "Song".into(), selected: 0,
            choices: vec![], loading: false, saving: false, error: None, notice: None, request_id: 1,
        }));
        app.handle_key(key(KeyCode::Char('N'))).unwrap();
        app.handle_key(key(KeyCode::Enter)).unwrap();
        assert!(requests.try_recv().is_err(), "a blank title must never write");
        for c in "quiet".chars() { app.handle_key(key(KeyCode::Char(c))).unwrap(); }
        app.handle_key(key(KeyCode::Enter)).unwrap();
        app.handle_key(key(KeyCode::Enter)).unwrap();
        let Request::ChangePlaylist { request_id, change: PlaylistChange::Create { title, video_id } } = requests.try_recv().unwrap() else {
            panic!("expected a playlist creation");
        };
        assert_eq!(title, "quiet");
        assert_eq!(video_id.as_deref(), Some("original-song"));
        assert!(requests.try_recv().is_err());
        app.apply(Response::PlaylistChanged { request_id: request_id + 1, result: Ok(()) });
        assert!(app.overlay.is_open());
        app.apply(Response::PlaylistChanged { request_id, result: Ok(()) });
        assert!(!app.overlay.is_open());
        assert!(app.status.contains("created with this song"));
    }

    #[test]
    fn remove_and_move_target_playlist_entries_instead_of_duplicate_video_ids() {
        let (mut app, _, requests) = crate::app::testing::scenarios::fixture();
        app.view = View::Tracks;
        app.browsing_endpoint = Some(BrowseEndpoint::new("VLPLmine"));
        app.results = vec![Track { id: "same".into(), title: "Song".into(), uploader: "Artist".into(),
            duration: None, album: None, artist_ref: None }; 2];
        app.collection = Some(Details::default());
        assert!(!app.can_remove_playlist_entry());
        app.collection.as_mut().unwrap().editable_entries = Some(vec![Some("first".into()), Some("second".into())]);
        app.selected = 1;
        app.remove_playlist_entry();
        assert!(requests.try_recv().is_err(), "removing a song requires confirmation");
        app.handle_mouse_action(MouseAction::SubmitPlaylistEdit).unwrap();
        let Request::ChangePlaylist { request_id, change: PlaylistChange::Remove { id, video_id, entry_id } } = requests.try_recv().unwrap() else {
            panic!("expected removal of a playlist entry");
        };
        assert_eq!((id.as_str(), video_id.as_str(), entry_id.as_str()), ("PLmine", "same", "second"));
        app.apply(Response::PlaylistChanged { request_id, result: Err("Edit failed".into()) });
        assert_eq!(app.results.len(), 2);
        app.handle_key(key(KeyCode::Esc)).unwrap();
        app.move_playlist_entry(false);
        assert!(matches!(requests.try_recv().unwrap(), Request::ChangePlaylist {
            change: PlaylistChange::MoveBefore { entry_id, successor, .. }, ..
        } if entry_id == "second" && successor == "first"));
    }
}
