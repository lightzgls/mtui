//! Artwork state, sizing, and terminal image placement.

use crate::runtime::config::{CoverStyle, ImageRenderer};
use crate::source::cover::{self, Cover};
use crate::source::worker::Request;

use super::App;

/// How much of the window the cover is allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CoverSize {
    /// A pane beside the results.
    #[default]
    Side,
    /// The whole main area, results hidden. The largest picture the window can
    /// hold -- past this, resolution is bounded by the terminal itself.
    Full,
}

impl CoverSize {
    pub(super) fn toggled(self) -> Self {
        match self {
            Self::Side => Self::Full,
            Self::Full => Self::Side,
        }
    }
}

/// How a landing-page card is drawn.
///
/// Four shapes rather than one because a terminal is not one size. A cell is
/// twice as tall as it is wide, so a square picture `n` columns across costs
/// `n / 2` rows -- which means a card with its sleeve above the title, the way
/// every music app draws one, is a dozen rows tall before a word is written.
/// That is the best-looking card and it does not fit on a short window, so it
/// is what a tall window gets and not what every window is forced into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CardShape {
    /// Title and subtitle only, no picture. The narrowest and shortest card,
    /// and the one every terminal can draw.
    Text,
    /// A small square sleeve to the left of the text. Costs one row over
    /// [`Self::Text`] and is what most windows end up showing.
    Tile,
    /// The sleeve across the top of the card with the text beneath it, as a
    /// music app draws it. Wants a tall window -- the renderer only chooses it
    /// on its own when two shelves of them fit.
    Poster,
    /// [`Self::Poster`] with room to breathe: half again the sleeve, and few
    /// enough across a row that the page reads as a shelf of records rather
    /// than a grid of thumbnails.
    ///
    /// Chosen for the lead shelf whenever one complete gallery card fits.
    Gallery,
}

impl CardShape {
    /// Largest first, which is the order the renderer tries them in.
    pub const ALL: [Self; 4] = [Self::Gallery, Self::Poster, Self::Tile, Self::Text];
}

/// Where and how large the cover should be painted, in cells and pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImagePlan {
    /// Top-left cell of the image.
    pub col: u16,
    pub row: u16,
    /// Size in pixels. A whole number of cells by construction.
    pub width: u16,
    pub height: u16,
    /// The cell dimension Kitty should hold fixed. Supplying only one axis
    /// makes the terminal derive the other from the source aspect ratio rather
    /// than stretching a square into an inaccurately reported cell box.
    pub bound: ImageBound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageBound {
    Columns(u16),
    Rows(u16),
}

/// Which cached bitmap a terminal-image placement paints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageSource {
    Playing,
    Card(String),
}

/// One bitmap placement in the current frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedImage {
    pub source: ImageSource,
    pub plan: ImagePlan,
}

impl App {

    pub fn set_graphics(&mut self, graphics: crate::terminal::graphics::Graphics) {
        let previous_edge = self.cover_edge();
        self.graphics = graphics;
        self.sync_art_resolution(previous_edge);
    }

    pub(super) fn cover_edge(&self) -> u32 {
        if self.cover_style == CoverStyle::Pixel && self.protocol_images() {
            cover::MAX_EDGE
        } else {
            cover::CELL_EDGE
        }
    }

    pub(super) fn sync_art_resolution(&mut self, previous_cover_edge: u32) {
        self.art.set_edge(if self.protocol_images() { crate::terminal::graphics::art::EDGE } else { crate::terminal::graphics::art::CELL_EDGE });
        let edge = self.cover_edge();
        if edge == previous_cover_edge { return; }
        if let Some(cover) = self.cover.as_mut() { cover.shrink(edge); }
        self.invalidate_artwork(ImageSource::Playing);
        // Metadata chooses the source first. Settings must not start a video
        // fallback while the Music watch response is still pending.
        if (edge > previous_cover_edge || self.cover.is_none())
            && let Some(id) = self.cover_id.clone()
        {
            let artwork_url = self.now.as_ref().and_then(|now| now.artwork_url.clone());
            self.request_cover(id, artwork_url);
        }
    }

    pub fn collection_art_key(&self) -> Option<&str> {
        self.browsing_endpoint.as_ref().map(|endpoint| endpoint.browse_id.as_str())
    }


    /// Kitty is used automatically when detected, or explicitly when a
    /// multiplexer hides the terminal name from capability detection.
    pub fn kitty_images(&self) -> bool {
        self.image_renderer == ImageRenderer::Kitty
            || (self.image_renderer == ImageRenderer::Automatic && self.graphics.kitty)
    }


    pub fn sixel_images(&self) -> bool {
        self.image_renderer == ImageRenderer::Sixel
            || (self.image_renderer == ImageRenderer::Automatic && self.graphics.sixel)
    }


    pub fn protocol_images(&self) -> bool {
        self.kitty_images() || self.sixel_images()
    }


    /// The cover to paint as pixels, if what the renderer planned is not
    /// already on screen. `None` means the pixels there are correct and
    /// repainting would only cost bandwidth and flicker.
    pub fn images_to_paint(&self) -> Vec<(&Cover, &PlannedImage)> {
        if self.painted == self.images && self.dirty_images.is_empty() {
            return Vec::new();
        }
        self.images
            .iter()
            .filter(|image| !self.painted.contains(image) || self.dirty_images.contains(&image.source))
            .filter_map(|image| {
                let art = match &image.source {
                    ImageSource::Playing => self.cover.as_ref(),
                    ImageSource::Card(key) => self.art.get(key),
                }?;
                Some((art, image))
            })
            .collect()
    }


    pub fn mark_painted(&mut self, kitty: bool) {
        self.painted.clone_from(&self.images);
        self.painted_with_kitty = kitty;
        self.dirty_images.clear();
    }


    /// True when pixels are on screen that no longer belong there: the pane
    /// went away, or moved out from under them. Erasing protocol pixels means
    /// painting cells over it, so only a redraw can undo it.
    pub fn image_needs_clearing(&self) -> bool {
        // A new cover arriving does not invalidate pictures already on screen.
        // Only removals or changed placements need their old pixels erased.
        self.painted.iter().any(|image| !self.images.contains(image))
    }


    pub fn painted_images(&self) -> &[PlannedImage] {
        &self.painted
    }


    pub fn painted_with_kitty(&self) -> bool {
        self.painted_with_kitty
    }


    /// Forgets what is on screen so the next frame repaints it. For events that
    /// can destroy the pixels without changing the plan, such as a resize.
    pub fn invalidate_image(&mut self) {
        self.painted.clear();
        self.painted_with_kitty = false;
        self.dirty_images.clear();
    }

    /// Repaint updated pixels without forgetting where the old image lives.
    /// Its placement is still needed if the next frame must erase it.
    pub(super) fn invalidate_artwork(&mut self, source: ImageSource) {
        if self.painted.iter().any(|image| image.source == source)
            && !self.dirty_images.contains(&source)
        {
            self.dirty_images.push(source);
        }
    }


    pub(super) fn toggle_cover_size(&mut self) {
        self.cover_size = self.cover_size.toggled();
    }


    /// Asks for the artwork of the cards the renderer has just drawn.
    ///
    /// Driven from the render pass rather than from the feed arriving, because
    /// "which cards are on screen" is a fact about the window and the scroll
    /// position, and both are things only the renderer knows. The cache filters
    /// out everything already held or already asked for, so this is a no-op on
    /// all but the first frame after the view moves.
    pub fn want_art(&mut self, cards: Vec<(String, Option<String>)>) {
        self.art.set_edge(if self.protocol_images() { crate::terminal::graphics::art::EDGE } else { crate::terminal::graphics::art::CELL_EDGE });
        self.source.visible_art(cards.iter().filter(|(_,url)|url.is_some()).map(|(key,_)|key.clone()));
        let mut urls: std::collections::HashMap<_, _> = cards.iter()
            .filter_map(|(key, url)| Some((key.clone(), url.clone()?))).collect();
        let requests = self.art.want_visible(cards.iter()
            .filter(|(_, url)| url.is_some()).map(|(key, _)| key.as_str()));
        for key in requests {
            if let Some(url) = urls.remove(&key) {
                let _ = self.source.send(Request::Art { key, url, edge: self.art.edge() });
            }
        }
    }


    /// Queues a cover fetch for `id` and drops whatever cover is on screen.
    ///
    /// Always sent *after* the resolve it accompanies: the worker is serial, so
    /// the reverse order would put a picture ahead of the audio the user asked
    /// for. A send failure needs no message -- the resolve just ahead of it will
    /// have reported the same dead worker.
    pub(super) fn request_cover(&mut self, id: String, artwork_url: Option<String>) {
        self.cover_id = Some(id.clone());
        let _ = self.source.send(Request::Cover { id, artwork_url, edge: self.cover_edge() });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::graphics::Graphics;
    use crate::source::youtube::watch::Watch;
    use crate::source::worker::Response;

    fn cover_reply(edge: u32, side: u32) -> Response {
        Response::Cover {
            id: "song".into(), artwork_url: None, edge,
            art: Some(Cover::solid(side, side)),
        }
    }

    #[test]
    fn real_images_upgrade_current_art_and_cells_release_it_without_refetching() {
        let (mut app, _driver, requests) = crate::app::testing::scenarios::fixture();
        let sleeve = "https://lh3.googleusercontent.com/sleeve=w544-h544-l90-rj";
        let mut track = app.results[0].clone();
        track.id = "song".into();
        app.now = Some(crate::app::NowPlaying::new(&track));
        app.request_cover("song".into(), Some(sleeve.into()));
        app.now.as_mut().unwrap().artwork_url = Some(sleeve.into());
        app.apply(cover_reply(cover::CELL_EDGE, cover::CELL_EDGE));
        let cards = vec![("card".into(), Some(sleeve.into()))];
        app.want_art(cards.clone());
        app.apply(Response::Art { key: "card".into(), edge: crate::terminal::graphics::art::CELL_EDGE,
            art: Some(Cover::solid(64, 64)) });
        requests.try_iter().for_each(drop);

        app.set_graphics(Graphics { kitty: true, sixel: false, cell: (8, 16) });
        assert!(matches!(requests.try_recv().unwrap(), Request::Cover { edge: cover::MAX_EDGE, artwork_url: Some(url), .. } if url == sleeve));
        assert_eq!(app.cover.as_ref().unwrap().width, cover::CELL_EDGE);
        app.want_art(cards.clone());
        assert!(matches!(requests.try_recv().unwrap(), Request::Art { edge: crate::terminal::graphics::art::EDGE, .. }));
        app.apply(cover_reply(cover::MAX_EDGE, 720));
        app.apply(Response::Art { key: "card".into(), edge: crate::terminal::graphics::art::EDGE,
            art: Some(Cover::solid(256, 256)) });
        // A low-resolution response that was already queued must not downgrade
        // either freshly loaded picture, or erase it on a late failure.
        app.apply(cover_reply(cover::CELL_EDGE, 128));
        app.apply(Response::Art { key: "card".into(), edge: crate::terminal::graphics::art::CELL_EDGE, art: None });
        assert_eq!(app.cover.as_ref().unwrap().width, 720);
        assert_eq!(app.art.get("card").unwrap().width, 256);

        app.set_graphics(Graphics::blocks());
        assert_eq!(app.cover.as_ref().unwrap().width, 128);
        assert_eq!(app.art.get("card").unwrap().width, 64);
        app.want_art(cards);
        assert!(requests.try_recv().is_err(), "downgrades reuse existing pixels");
        // High-resolution replies from before the downgrade are bounded too.
        app.apply(cover_reply(cover::MAX_EDGE, 720));
        app.apply(Response::Art { key: "card".into(), edge: crate::terminal::graphics::art::EDGE,
            art: Some(Cover::solid(256, 256)) });
        assert_eq!(app.cover.as_ref().unwrap().width, 128);
        assert_eq!(app.art.get("card").unwrap().width, 64);
        assert_eq!(app.now.as_ref().unwrap().artwork_url.as_deref(), Some(sleeve));
    }

    #[test]
    fn artwork_resolution_follows_actual_renderer_support_and_cover_style() {
        let (mut app, _driver, _requests) = crate::app::testing::scenarios::fixture();
        for renderer in [ImageRenderer::Automatic, ImageRenderer::Kitty, ImageRenderer::Sixel, ImageRenderer::PixelArt] {
            for supported in [false, true] {
                for style in [CoverStyle::Pixel, CoverStyle::ColoredAscii] {
                    app.image_renderer = renderer;
                    app.cover_style = style;
                    app.set_graphics(Graphics { kitty: supported, sixel: false, cell: (8, 16) });
                    let real = match renderer {
                        ImageRenderer::Automatic => supported,
                        ImageRenderer::Kitty | ImageRenderer::Sixel => true,
                        ImageRenderer::PixelArt => false,
                    };
                    assert_eq!(app.cover_edge(), if real && style == CoverStyle::Pixel { cover::MAX_EDGE } else { cover::CELL_EDGE });
                    assert_eq!(app.art.edge(), if real { crate::terminal::graphics::art::EDGE } else { crate::terminal::graphics::art::CELL_EDGE });
                }
            }
        }
    }

    #[test]
    fn changing_renderer_waits_for_metadata_and_replaces_pending_large_fetches() {
        let (mut app, _driver, requests) = crate::app::testing::scenarios::fixture();
        let mut track = app.results[0].clone();
        track.id = "song".into();
        app.now = Some(crate::app::NowPlaying::new(&track));
        app.set_graphics(Graphics { kitty: false, sixel: true, cell: (8, 16) });
        assert!(requests.try_recv().is_err(), "metadata must choose the source first");
        app.apply_watch("song", Ok(Watch::default()));
        assert!(requests.try_iter().any(|request| matches!(request, Request::Cover { edge: cover::MAX_EDGE, .. })));
        app.set_graphics(Graphics::blocks());
        assert!(matches!(requests.try_recv().unwrap(), Request::Cover { edge: cover::CELL_EDGE, .. }));
    }

    #[test]
    fn upgraded_pixels_repaint_and_downgrading_preserves_the_placement_to_erase() {
        let (mut app, _driver, _requests) = crate::app::testing::scenarios::fixture();
        app.request_cover("song".into(), None);
        app.apply(cover_reply(cover::CELL_EDGE, 128));
        app.images.push(PlannedImage {
            source: ImageSource::Playing,
            plan: ImagePlan { col: 0, row: 0, width: 128, height: 128, bound: ImageBound::Columns(16) },
        });
        app.mark_painted(true);
        assert!(app.images_to_paint().is_empty());
        app.apply(cover_reply(cover::CELL_EDGE, 128));
        assert_eq!(app.images_to_paint().len(), 1);
        assert!(!app.image_needs_clearing(), "same placement needs only new pixels");
        app.mark_painted(true);
        assert!(app.images_to_paint().is_empty());

        app.invalidate_artwork(ImageSource::Playing);
        app.images.clear();
        assert!(app.image_needs_clearing(), "old protocol pixels must be erased when switching to cells");
        assert!(app.painted_with_kitty());
        assert_eq!(app.painted_images().len(), 1);
        app.invalidate_image();
        assert!(app.dirty_images.is_empty());
    }
}
