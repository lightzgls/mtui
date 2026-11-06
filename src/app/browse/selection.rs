//! Queue actions follow the selected playable song on every browsing page.
use crate::source::Track;
use crate::source::worker::PageRequestId;

use crate::app::App;
use crate::app::controls::input::MouseAction;
use crate::app::browse::navigation::View;
use crate::app::playback::now_playing::{RelatedRow, Tab};

/// A click must still refer to the same song and page after a worker reply.
#[derive(PartialEq, Eq)]
pub(crate) struct SongClick {
    pub select: MouseAction,
    pub activate: MouseAction,
    video_id: String,
    page_request: PageRequestId,
    queue_epoch: u64,
}

impl App {
    pub(crate) fn song_click(&self, action: MouseAction) -> Option<SongClick> {
        if self.overlay.is_open() || self.menu.is_some() { return None; }
        let (select, video_id) = match action {
            MouseAction::OpenHomeCard { shelf, card } if self.view == View::Home => (
                MouseAction::SelectHomeCard { shelf, card },
                self.home.get(shelf)?.cards.get(card)?.track()?.id,
            ),
            MouseAction::PlayTrack(index) if self.view == View::Tracks => {
                let track = if self.browsing.is_none() && !self.search_items.is_empty() {
                    self.search_items.get(index)?.track.as_ref()?
                } else { self.results.get(index)? };
                (MouseAction::SelectTrack(index), track.id.clone())
            }
            MouseAction::OpenArtistSong(index) if self.view == View::Artist => (
                MouseAction::SelectArtistSong(index),
                self.artist.as_ref()?.content()?.top_songs.get(index)?.track.id.clone(),
            ),
            MouseAction::OpenArtistCard { shelf, card } if self.view == View::Artist => (
                MouseAction::SelectArtistCard { shelf, card },
                self.artist.as_ref()?.content()?.shelves.get(shelf)?.cards.get(card)?.track()?.id,
            ),
            MouseAction::OpenLibraryItem(index) if self.view == View::Library => {
                let item = self.library.collection().items.get(index).filter(|item| item.available)?;
                let id = item.track.as_ref().map(|track| track.id.clone()).or_else(|| item.card.track().map(|track| track.id))?;
                (MouseAction::SelectLibraryItem(index), id)
            }
            MouseAction::SelectPageRow(index) => {
                let now = self.now.as_ref().filter(|now| now.tab == Tab::UpNext)?;
                (action, now.queue.get(index)?.id.clone())
            }
            MouseAction::OpenPageRow(index) => {
                let now = self.now.as_ref().filter(|now| now.tab == Tab::Related)?;
                let id = match now.related_rows().get(index)? {
                    RelatedRow::Card(card) => card.track()?.id,
                    RelatedRow::Heading(_) => return None,
                };
                (MouseAction::SelectPageRow(index), id)
            }
            _ => return None,
        };
        Some(SongClick { select, activate: match action {
            MouseAction::SelectPageRow(index) => MouseAction::OpenPageRow(index), _ => action,
        }, video_id, page_request: self.page_request, queue_epoch: self.queue_epoch })
    }

    pub(in crate::app) fn selected_queue_track(&self) -> Option<Track> {
        match self.view {
            View::Home => self.home_card()?.track(),
            View::Tracks => self.selected_result_track().cloned(),
            View::Artist => {
                let artist = self.artist.as_ref()?;
                artist.selected_track().cloned().or_else(|| artist.selected_card()?.track())
            }
            View::Library => {
                let item = self.library.selected().filter(|item| item.available)?;
                item.track.clone().or_else(|| item.card.track())
            }
            View::Playing => {
                let now = self.now.as_ref().filter(|now| now.tab == Tab::Related)?;
                match now.related_rows().get(now.cursor())? {
                    RelatedRow::Card(card) => card.track(),
                    RelatedRow::Heading(_) => None,
                }
            }
        }
    }

    pub(in crate::app) fn queue_selected_track(&mut self, next: bool) {
        let Some(track) = self.selected_queue_track() else { return; };
        let Some(now) = self.now.as_mut().filter(|now| now.playing.is_some()) else {
            self.status = "wait for Up next to finish loading".into();
            return;
        };
        let playing = now.playing.unwrap();
        let selected_id = now.queue.get(now.cursor[Tab::UpNext.index()]).map(|row| row.id.clone());
        if let Some(at) = now.queue.iter().position(|row| row.id == track.id) {
            if !next || at <= playing {
                self.status = format!("{} is already in the queue", track.title);
                return;
            }
            let held = now.queue.remove(at);
            now.queue.insert(playing + 1, held);
        } else if !now.insert_user_track(track.clone(), next) {
            return;
        }
        // Inserting or promoting before the cursor must keep its song selected.
        if let Some(at) = selected_id.and_then(|id| now.queue.iter().position(|row| row.id == id)) {
            now.cursor[Tab::UpNext.index()] = at;
        } else {
            now.cursor[Tab::UpNext.index()] = now.cursor[Tab::UpNext.index()].min(now.queue.len().saturating_sub(1));
        }
        if next {
            self.prefetch_queue_next();
            self.status = format!("playing {} next", track.label());
        } else {
            self.status = format!("added {} to the queue", track.label());
        }
    }
}
