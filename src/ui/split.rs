//! Mouse resizing uses the same bounds as the divider drawn on screen.

use ratatui::layout::Rect;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Split {
    SidebarWidth,
    SidebarArtwork,
    PlayerWidth,
    PlayerDetails,
}

#[derive(Default)]
pub(crate) struct Splits([Option<u16>; 4]);

impl Splits {
    pub(crate) fn size(&self, split: Split, extent: u16) -> Option<u16> {
        self.0[split as usize].map(|ratio| ((u32::from(extent) * u32::from(ratio) + 500) / 1000) as u16)
    }

    pub(crate) fn drag(&mut self, split: Split, bounds: Rect, x: u16, y: u16) {
        let (offset, extent) = match split {
            Split::SidebarWidth => (bounds.right().saturating_sub(x), bounds.width),
            Split::PlayerWidth => (bounds.right().saturating_sub(x.saturating_add(1)), bounds.width),
            Split::SidebarArtwork | Split::PlayerDetails => (y.saturating_sub(bounds.y), bounds.height),
        };
        if extent > 0 {
            self.0[split as usize] = Some((u32::from(offset.min(extent)) * 1000 / u32::from(extent)) as u16);
        }
    }
}
