//! Artwork state, sizing, and terminal image placement.

use crate::config::ImageRenderer;
use crate::source::cover::Cover;
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
        self.image_renderer == ImageRenderer::Automatic && self.graphics.sixel
    }


    pub fn protocol_images(&self) -> bool {
        self.kitty_images() || self.sixel_images()
    }


    /// The cover to paint as pixels, if what the renderer planned is not
    /// already on screen. `None` means the pixels there are correct and
    /// repainting would only cost bandwidth and flicker.
    pub fn images_to_paint(&self) -> Vec<(&Cover, &PlannedImage)> {
        if self.painted == self.images {
            return Vec::new();
        }
        self.images
            .iter()
            .filter(|image| !self.painted.contains(image))
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
        self.source.visible_art(cards.iter().filter(|(_,url)|url.is_some()).map(|(key,_)|key.clone()));
        let mut urls: std::collections::HashMap<_, _> = cards.iter()
            .filter_map(|(key, url)| Some((key.clone(), url.clone()?))).collect();
        let requests = self.art.want_visible(cards.iter()
            .filter(|(_, url)| url.is_some()).map(|(key, _)| key.as_str()));
        for key in requests {
            if let Some(url) = urls.remove(&key) {
                let _ = self.source.send(Request::Art { key, url });
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
        let _ = self.source.send(Request::Cover { id, artwork_url });
    }
}
