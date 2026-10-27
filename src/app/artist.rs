//! Artist song expansion and ordered playback retain the artist as Back's target.
use super::*;
use std::collections::HashSet;

impl App {
    pub(super) fn more_artist_songs(&mut self, play: bool) {
        let Some(view) = self.artist.as_mut() else { return; };
        if let Some((_, play_after)) = view.songs_request.as_mut() {
            *play_after |= play;
            return;
        }
        let Some(page) = view.content() else { return; };
        let endpoint = page.songs_endpoint.clone();
        if let Some(endpoint) = endpoint {
            let artist = view.requested.clone();
            self.page_request = self.page_request.wrapping_add(1).max(1);
            let request_id = self.page_request;
            view.songs_request = Some((request_id, play));
            view.songs_error = None;
            self.status = format!("Loading all top songs by {}…", artist.name);
            if self.source.send(Request::ArtistSongs { request_id, artist, endpoint }).is_err() {
                view.songs_request = None;
                view.songs_error = Some("Music service is unavailable. Try again.".into());
                self.status = view.songs_error.clone().unwrap();
            }
        } else if play {
            self.play_artist_songs();
        } else {
            view.songs_expanded = !view.songs_expanded;
            view.section = 0;
            view.top = 0;
            view.song = 0;
            view.song_offset = 0;
        }
    }

    pub(super) fn apply_artist_songs(&mut self, request_id: PageRequestId, browse_id: &str,
        result: Result<crate::source::collection::Page, String>) {
        if self.view != View::Artist { return; }
        let Some(_) = self.artist.as_ref().filter(|view|
            view.requested.endpoint.browse_id == browse_id
                && view.songs_request.is_some_and(|(id, _)| id == request_id)) else { return; };
        self.close_page_actions();
        let view = self.artist.as_mut().expect("validated artist request");
        let play = view.songs_request.take().is_some_and(|(_, play)| play);
        match result {
            Err(error) => {
                view.songs_error = Some(error);
                self.status = "Could not load more top songs. Choose Retry more.".into();
            }
            Ok(listing) => {
                let Panel::Ready(page) = &mut view.page else { return; };
                let mut seen = HashSet::new();
                let songs: Vec<_> = listing.tracks.into_iter().filter(|track| seen.insert(track.id.clone()))
                    .map(|mut track| {
                        let preview = page.top_songs.iter().find(|song| song.track.id == track.id);
                        if track.album.is_none() { track.album = preview.and_then(|song| song.track.album.clone()); }
                        if track.uploader.is_empty() || track.uploader == UNKNOWN_ARTIST {
                            track.uploader = page.artist.name.clone();
                        }
                        if track.artist_ref.is_none() { track.artist_ref = Some(page.artist.clone()); }
                        ArtistSong { track, plays: preview.and_then(|song| song.plays.clone()) }
                    }).collect();
                if songs.is_empty() {
                    view.songs_error = Some("The song list was empty. Try again.".into());
                    self.status = "No additional songs loaded. Your artist preview is still available.".into();
                    return;
                }
                page.top_songs = songs;
                if !listing.details.truncated { page.songs_endpoint = None; }
                page.songs_truncated = listing.details.truncated;
                view.songs_expanded = true;
                view.songs_error = None;
                self.status = if page.songs_truncated {
                    "The provider's top-song list is incomplete.".into()
                } else { String::new() };
                if play { self.play_artist_songs(); }
            }
        }
    }

    fn play_artist_songs(&mut self) {
        let Some(page) = self.artist.as_ref().and_then(ArtistView::content) else { return; };
        let tracks: Vec<_> = page.top_songs.iter().map(|song| song.track.clone()).collect();
        let Some(first) = tracks.first().cloned() else { return; };
        let title = format!("{} · Top songs", page.artist.name);
        let truncated = page.songs_truncated;
        self.play_track(first, false);
        self.queue_epoch = self.queue_epoch.wrapping_add(1);
        if let Some(now) = self.now.as_mut() {
            now.queue = tracks;
            now.queue_title = title.clone();
            now.queue_epoch = self.queue_epoch;
            now.finite_queue = true;
            now.continuation = None;
            now.topping_up = false;
            now.playing = Some(0);
            now.open(Tab::UpNext);
            now.cursor[Tab::UpNext.index()] = 0;
        }
        self.status = if truncated { "The loaded top-song list is incomplete.".into() } else { String::new() };
    }
}
