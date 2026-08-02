//! Spotify source (Phase 5, `--features spotify`).
//!
//! Not implemented yet. This file exists so `cargo fmt` and `rust-analyzer` can
//! resolve `mod spotify`, which they do regardless of the `cfg` gate.
//!
//! When implemented, librespot must run as a **child process**, not a linked
//! dependency: it costs ~50-80 MB resident, which would otherwise become the
//! floor for the whole application even when Spotify is never used.
//!
//! Note that librespot is capped at 320 kbps Ogg Vorbis. Spotify does not
//! deliver its lossless tier to Connect devices, so this source can never be
//! higher quality than the default YouTube path.

use anyhow::{Result, bail};

use super::{StreamUrl, Track};

/// Placeholder so the module has a shape to grow into.
pub struct Spotify {
    _private: (),
}

impl Spotify {
    pub fn new() -> Result<Self> {
        bail!("the Spotify source is not implemented yet")
    }

    pub fn search(&self, _query: &str, _limit: usize) -> Result<Vec<Track>> {
        bail!("the Spotify source is not implemented yet")
    }

    pub fn resolve(&self, _id: &str) -> Result<StreamUrl> {
        bail!("the Spotify source is not implemented yet")
    }
}
