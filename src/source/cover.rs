//! Cover art: a thumbnail, decoded and shrunk to something a terminal can
//! actually draw.
//!
//! Two callers want different things from that sentence, and both are served
//! here. [`fetch`] is the cover of the track that is playing: one picture, as
//! large as YouTube has it, found by video id. [`ArtFetcher`] is the landing
//! page's card artwork: many small pictures, found by URL because an album
//! sleeve has no id to derive one from, and kept small enough that a screenful
//! of them costs less than one cover does.
//!
//! Same containment rule as the rest of this module, with one stated exception.
//! The decoded image is downscaled here and dropped, and only the few tens of
//! kilobytes the renderer samples from survive the call -- but [`ArtFetcher`]
//! does keep a runtime and a connection resident for as long as the art thread
//! runs, because a dozen tiles arriving at once is the one case where handshakes
//! cost more than the pictures. Its doc comment makes that argument in full.
//!
//! What [`fetch`] shows is the *video* thumbnail, which for auto-generated
//! "Topic" uploads is the album art and for a music video is a frame from it.
//! Card artwork is the real sleeve, because the feed says where it is.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use jpeg_decoder::PixelFormat;

/// Longest edge kept after downscaling, in image pixels.
///
/// Set to keep everything YouTube actually has. The largest thumbnail it serves
/// is 1280x720, so this discards nothing: square album art arrives pillarboxed
/// inside that canvas and comes out of [`trim_bars`] at 720x720, while a 16:9
/// cover keeps all 1280 columns instead of being squashed to 720 before the
/// renderer has said how big a pane it has. Anything lower would mean upscaling
/// a shrunken copy back up for a full-window cover, which is throwing away
/// detail and then inventing it again.
///
/// The cost is honest and worth naming: a 1280x720 cover is 2.7 MB of RGB,
/// resident only while a track is playing and dropped the moment one stops.
/// That is the price of a picture rather than a mosaic.
const MAX_EDGE: u32 = 1280;

/// Ceiling on the response body. A 1280x720 thumbnail runs 60-90 KB; anything
/// wildly past that is not one, and buffering it would undo the point.
const MAX_BYTES: u64 = 2 * 1024 * 1024;

/// Per-channel distance at which two pixels count as the same colour when
/// looking for padding bars. Generous, because JPEG does not keep a flat black
/// perfectly flat.
const BAR_TOLERANCE: u32 = 14;

/// Most of an edge that may be trimmed as padding, as a percentage. Stops a
/// genuinely flat-coloured cover from being cropped away to nothing.
const MAX_TRIM_PERCENT: u32 = 45;

/// Whole-fetch budget, spanning every name in [`SIZES`] rather than each. A
/// cover is decoration: it is not worth making the worker unavailable for a
/// real request while a slow CDN edge thinks about it, and a ladder that spent
/// this much per rung could sit on the cover thread for four times as long.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Thumbnail names to try, largest first.
///
/// What matters is how many pixels survive [`trim_bars`], since every one of
/// these is the source frame fitted into a fixed canvas: square album art comes
/// out of the first two at 720x720, out of `sddefault` at 480x480, and out of
/// `hqdefault` at 360x360.
///
/// `maxresdefault` and `hq720` are the same 1280x720 image under two names, and
/// both are here because neither is generated for every upload -- an older or
/// low-resolution one may have `hq720` and no `maxresdefault`. `sddefault` sits
/// between them and the one size YouTube always has, so a track missing the top
/// two lands on 480x480 rather than dropping straight to 360x360.
const SIZES: &[&str] = &["maxresdefault", "hq720", "sddefault", "hqdefault"];

/// A decoded, downscaled thumbnail, held as tightly packed 8-bit RGB.
#[derive(Clone)]
pub struct Cover {
    pub width: u32,
    pub height: u32,
    /// The colour the picture reads as, for the border and title drawn around
    /// it. See [`accent_of`].
    pub accent: (u8, u8, u8),
    rgb: Vec<u8>,
}

impl Cover {
    /// The only way a `Cover` is made, so that no picture can exist without the
    /// accent colour drawn beside it having been worked out from its pixels.
    fn build(width: u32, height: u32, rgb: Vec<u8>) -> Self {
        Self {
            accent: accent_of(&rgb),
            width,
            height,
            rgb,
        }
    }

    /// The pixel at `(x, y)`, clamped to the image rather than panicking, so
    /// the renderer's integer scaling cannot fall off the edge.
    pub fn pixel(&self, x: u32, y: u32) -> (u8, u8, u8) {
        let x = x.min(self.width - 1);
        let y = y.min(self.height - 1);
        let i = ((y * self.width + x) * 3) as usize;
        (self.rgb[i], self.rgb[i + 1], self.rgb[i + 2])
    }

    /// Resamples to an exact pixel size for the sixel renderer, which needs the
    /// image at whatever size the pane works out to rather than at the size it
    /// happened to be stored.
    ///
    /// Bilinear: the target is usually within a factor of two of the stored
    /// size, where nearest-neighbour stair-steps every edge and a box filter
    /// blurs the upscales.
    pub fn resample(&self, width: u32, height: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity((width * height * 3) as usize);
        for y in 0..height {
            let (y0, y1, fy) = map(y, height, self.height);
            for x in 0..width {
                let (x0, x1, fx) = map(x, width, self.width);

                let at = |x: u32, y: u32| {
                    let i = ((y * self.width + x) * 3) as usize;
                    &self.rgb[i..i + 3]
                };
                let (tl, tr, bl, br) = (at(x0, y0), at(x1, y0), at(x0, y1), at(x1, y1));
                for c in 0..3 {
                    let top = f32::from(tl[c]) * (1.0 - fx) + f32::from(tr[c]) * fx;
                    let bottom = f32::from(bl[c]) * (1.0 - fx) + f32::from(br[c]) * fx;
                    out.push((top * (1.0 - fy) + bottom * fy).round() as u8);
                }
            }
        }
        out
    }
}

/// Maps an output coordinate to the two source coordinates it sits between and
/// how far along it is. Pixel centres, so the image is not shifted half a pixel.
fn map(out: u32, out_len: u32, src_len: u32) -> (u32, u32, f32) {
    let pos = ((out as f32 + 0.5) * src_len as f32 / out_len as f32 - 0.5).max(0.0);
    let low = (pos as u32).min(src_len - 1);
    let high = (low + 1).min(src_len - 1);
    (low, high, pos - low as f32)
}

#[cfg(test)]
impl Cover {
    /// A flat mid-grey cover of the given size, for tests elsewhere that care
    /// about geometry rather than pixels.
    pub fn solid(width: u32, height: u32) -> Self {
        Self::from_rgb(width, height, vec![128; (width * height * 3) as usize])
    }

    /// A cover with explicit pixels, for tests that care about which pixel ends
    /// up where.
    pub fn from_rgb(width: u32, height: u32, rgb: Vec<u8>) -> Self {
        assert_eq!(rgb.len() as u32, width * height * 3, "pixel count");
        Self::build(width, height, rgb)
    }
}

/// The colour a picture reads as, for the frame and the title drawn around it.
///
/// A landing page whose every card is bordered in the same cyan says nothing
/// about what is on it; one that takes each card's colour from its own sleeve
/// is a page of records rather than a page of boxes. That is the whole purpose
/// of this function, and it is why it looks for a *vivid* colour rather than an
/// average one -- averaging a sleeve gives the mud halfway between its ink and
/// its paper, which is the one colour not actually on it.
///
/// So: pixels are dropped into a coarse colour cube, each weighted by how
/// saturated it is, and the heaviest cell wins. Weighting by saturation is what
/// lets a mostly-black sleeve with one red word on it come back red, while
/// counting cells rather than scoring pixels one at a time keeps a scatter of
/// unrelated bright pixels from outvoting the block of colour that dominates
/// the picture.
fn accent_of(rgb: &[u8]) -> (u8, u8, u8) {
    /// Cells per channel in the colour cube. Four bits is fine enough to keep
    /// two different reds apart and coarse enough that one red does not land in
    /// eight neighbouring cells.
    const STEPS: usize = 16;
    /// Below this a pixel is shadow and above it is paper. Neither is the
    /// colour of the record, and both are abundant enough to win on volume.
    const FLOOR: u8 = 24;
    const CEILING: u8 = 232;

    let mut cells = vec![[0u32; 4]; STEPS * STEPS * STEPS];
    for px in rgb.chunks_exact(3) {
        let (r, g, b) = (px[0], px[1], px[2]);
        let high = r.max(g).max(b);
        let low = r.min(g).min(b);
        if high < FLOOR || low > CEILING {
            continue;
        }
        // Plus one so a genuinely grey picture still elects a cell rather than
        // returning the fallback below with a perfectly good grey to hand.
        let weight = u32::from(high - low) + 1;
        let cell = &mut cells[index(r, g, b, STEPS)];
        cell[0] += u32::from(r) * weight;
        cell[1] += u32::from(g) * weight;
        cell[2] += u32::from(b) * weight;
        cell[3] += weight;
    }

    let Some(winner) = cells.iter().max_by_key(|cell| cell[3]).filter(|c| c[3] > 0) else {
        // Every pixel was shadow or paper: a sleeve that is genuinely black or
        // genuinely white. Neither has an accent, and inventing one would put a
        // colour on screen that is nowhere in the picture.
        return (150, 150, 150);
    };
    let weight = winner[3];
    lift((
        (winner[0] / weight) as u8,
        (winner[1] / weight) as u8,
        (winner[2] / weight) as u8,
    ))
}

fn index(r: u8, g: u8, b: u8, steps: usize) -> usize {
    let bucket = |v: u8| (usize::from(v) * steps / 256).min(steps - 1);
    (bucket(r) * steps + bucket(g)) * steps + bucket(b)
}

/// Brightens a colour until it is legible as a border on a dark terminal,
/// keeping its hue.
///
/// A sleeve whose dominant colour is a deep navy is honestly represented by
/// that navy, and a navy line on a black background is a line nobody can see.
/// Scaling all three channels together lifts it without turning it a different
/// colour, which is what clamping each channel separately would do.
fn lift((r, g, b): (u8, u8, u8)) -> (u8, u8, u8) {
    /// What the brightest channel is raised to, when it falls short.
    const TARGET: u32 = 170;

    let high = u32::from(r.max(g).max(b));
    if high >= TARGET || high == 0 {
        return (r, g, b);
    }
    let scale = |v: u8| (u32::from(v) * TARGET / high).min(255) as u8;
    (scale(r), scale(g), scale(b))
}

/// Fetches and decodes the thumbnail for a YouTube video id, taking the largest
/// size that upload actually has.
pub fn fetch(video_id: &str) -> Result<Cover> {
    let deadline = Instant::now() + TIMEOUT;
    let mut last = None;

    for name in SIZES {
        // A missing size answers 404 in milliseconds, so the ladder normally
        // costs nothing; this only bites when the CDN is unreachable, and then
        // it stops rather than spending the budget again on the next name.
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        // Decode failures fall through with the fetch failures: a truncated or
        // otherwise unreadable JPEG at one size says nothing about the next,
        // and there is a smaller copy of the same picture right below it.
        match get(&url(video_id, name), left).and_then(|body| decode(&body, MAX_EDGE)) {
            Ok(cover) => return Ok(cover),
            Err(e) => last = Some(e),
        }
    }

    Err(last.unwrap_or_else(|| anyhow::anyhow!("no thumbnail sizes are configured")))
        .with_context(|| format!("could not fetch a thumbnail for {video_id}"))
}

fn url(video_id: &str, name: &str) -> String {
    format!("https://i.ytimg.com/vi/{video_id}/{name}.jpg")
}

/// The thumbnail to ask for when a card needs a picture and only the video id
/// is known.
///
/// `mqdefault` rather than one of the [`SIZES`] ladder, for two reasons: it is
/// the one name YouTube generates for every upload ever posted, so a card built
/// from this never has to fall down a ladder of 404s to find out; and at 320x180
/// it is a tenth the bytes of the sizes the player's cover pane wants, which is
/// the right trade when the result is drawn thirty pixels across.
pub fn thumb_url(video_id: &str) -> String {
    url(video_id, "mqdefault")
}

/// Fetches artwork by URL, keeping one connection alive between calls.
///
/// Everything else in this module builds a runtime per request and drops it,
/// which is right when the request is one picture per track. Card artwork is
/// not that shape: opening the landing page asks for a dozen tiles back to back
/// off the same two hosts, and a runtime and TLS handshake per tile would cost
/// several times what the pictures do. So the art thread keeps one of these for
/// as long as it runs -- an idle current-thread reactor and a warm connection,
/// which is a few kilobytes standing against a dozen handshakes.
///
/// Held by value rather than shared: a `reqwest::Client`'s pool belongs to the
/// runtime its requests were driven on, so the two have to live and die
/// together.
pub struct ArtFetcher {
    runtime: tokio::runtime::Runtime,
    client: reqwest::Client,
}

impl ArtFetcher {
    pub fn new() -> Result<Self> {
        Ok(Self {
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("could not start a runtime for artwork")?,
            client: reqwest::Client::new(),
        })
    }

    /// Fetches and decodes one artwork URL, shrunk to `edge` on its longest
    /// side.
    ///
    /// Takes a URL where [`fetch`] takes an id, because a card's picture is
    /// whatever YouTube said it was: an album sleeve lives on Google's image
    /// CDN under an opaque name, and there is no id to derive it from.
    pub fn fetch(&self, url: &str, edge: u32) -> Result<Cover> {
        let body = self.runtime.block_on(read(&self.client, url, TIMEOUT))?;
        decode(&body, edge).with_context(|| format!("could not decode the artwork at {url}"))
    }
}

fn get(url: &str, timeout: Duration) -> Result<Vec<u8>> {
    // A current-thread runtime, built for this one request and dropped with it.
    // The worker thread blocks on the request anyway, so there is nothing for a
    // resident reactor to do between tracks.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("could not start a runtime for the thumbnail fetch")?;

    runtime.block_on(read(&reqwest::Client::new(), url, timeout))
}

/// One GET, bounded in both time and size. Shared by the per-track cover and
/// the per-card artwork so the ceiling on what may be buffered is stated once.
async fn read(client: &reqwest::Client, url: &str, timeout: Duration) -> Result<Vec<u8>> {
    let response = client
        .get(url)
        .timeout(timeout)
        .send()
        .await?
        .error_for_status()?;

    // Checked before and after reading: a server may not send a length, and one
    // that does may be lying about it.
    if response.content_length().is_some_and(|n| n > MAX_BYTES) {
        bail!("thumbnail is implausibly large");
    }
    let body = response.bytes().await?;
    if body.len() as u64 > MAX_BYTES {
        bail!("thumbnail is implausibly large");
    }

    Ok(body.to_vec())
}

/// Decodes a JPEG, trims the padding YouTube fitted it into, and shrinks what
/// is left to `edge` on its longest side.
///
/// `edge` is a parameter rather than [`MAX_EDGE`] because the two callers want
/// different pictures from the same code: the cover pane wants every pixel
/// YouTube has, and a card tile drawn thirty pixels across wants a fraction of
/// one -- see [`crate::art::EDGE`].
fn decode(jpeg: &[u8], edge: u32) -> Result<Cover> {
    let mut decoder = jpeg_decoder::Decoder::new(jpeg);
    let pixels = decoder
        .decode()
        .context("thumbnail is not decodable JPEG")?;
    let info = decoder
        .info()
        .context("thumbnail carried no frame header")?;

    let (width, height) = (u32::from(info.width), u32::from(info.height));
    if width == 0 || height == 0 {
        bail!("thumbnail has zero extent");
    }

    // Normalise to RGB. Greyscale is rare but real; CMYK does not occur on
    // i.ytimg.com and is not worth a colour-management path to guess at.
    let rgb = match info.pixel_format {
        PixelFormat::RGB24 => pixels,
        PixelFormat::L8 => pixels.iter().flat_map(|&v| [v, v, v]).collect(),
        other => bail!("unsupported thumbnail pixel format: {other:?}"),
    };

    // The decoder is trusted to be self-consistent, but shrink() indexes on
    // these dimensions, so a mismatch has to be an error and not a panic.
    if rgb.len() as u32 != width * height * 3 {
        bail!("thumbnail pixel data does not match its {width}x{height} header");
    }

    let (x, y, w, h) = trim_bars(width, height, &rgb);
    let cropped = crop(width, &rgb, x, y, w, h);
    Ok(shrink(w, h, &cropped, edge))
}

/// Finds the picture inside YouTube's padding, as an `(x, y, width, height)`
/// rect.
///
/// Every thumbnail is the source frame fitted into a fixed canvas, so square
/// album art arrives with a flat bar down each side and a 4:3 video arrives
/// with one above and below. At the size a terminal draws, those bars are not
/// a border -- they are half the picture. Dropping them is the single largest
/// gain in apparent resolution available without changing how we draw.
///
/// Scanning inward from each edge and comparing against that edge's own colour
/// means only padding contiguous with the border is ever removed.
fn trim_bars(width: u32, height: u32, rgb: &[u8]) -> (u32, u32, u32, u32) {
    let px = |x: u32, y: u32| {
        let i = ((y * width + x) * 3) as usize;
        (rgb[i], rgb[i + 1], rgb[i + 2])
    };
    let close = |a: (u8, u8, u8), b: (u8, u8, u8)| {
        u32::from(a.0.abs_diff(b.0)) <= BAR_TOLERANCE
            && u32::from(a.1.abs_diff(b.1)) <= BAR_TOLERANCE
            && u32::from(a.2.abs_diff(b.2)) <= BAR_TOLERANCE
    };
    let row_is_flat = |y: u32, c| (0..width).all(|x| close(px(x, y), c));
    let col_is_flat = |x: u32, c| (0..height).all(|y| close(px(x, y), c));

    let v_limit = height * MAX_TRIM_PERCENT / 100;
    let h_limit = width * MAX_TRIM_PERCENT / 100;

    let mut top = 0;
    let colour = px(0, 0);
    while top < v_limit && row_is_flat(top, colour) {
        top += 1;
    }
    let mut bottom = 0;
    let colour = px(0, height - 1);
    while bottom < v_limit && row_is_flat(height - 1 - bottom, colour) {
        bottom += 1;
    }
    let mut left = 0;
    let colour = px(0, 0);
    while left < h_limit && col_is_flat(left, colour) {
        left += 1;
    }
    let mut right = 0;
    let colour = px(width - 1, 0);
    while right < h_limit && col_is_flat(width - 1 - right, colour) {
        right += 1;
    }

    (
        left,
        top,
        (width - left - right).max(1),
        (height - top - bottom).max(1),
    )
}

fn crop(width: u32, rgb: &[u8], x: u32, y: u32, w: u32, h: u32) -> Vec<u8> {
    // Note what this does *not* check: `h`. An image trimmed only at the bottom
    // takes this path and gets the whole buffer back, trailing bar rows and
    // all. That is still correct, but only because `y == 0` and `w == width`
    // make the wanted region a prefix of the buffer, and the sole caller hands
    // the result straight to `shrink(w, h, ..)`, which never reads past
    // `w * h * 3`. A caller that read the whole slice would get bar pixels.
    debug_assert!(
        (x, y, w) != (0, 0, width) || (h * width * 3) as usize <= rgb.len(),
        "the untrimmed fast path assumes the wanted rows are a prefix"
    );
    if (x, y, w) == (0, 0, width) {
        return rgb.to_vec();
    }
    let mut out = Vec::with_capacity((w * h * 3) as usize);
    for row in y..y + h {
        let start = ((row * width + x) * 3) as usize;
        out.extend_from_slice(&rgb[start..start + (w * 3) as usize]);
    }
    out
}

/// Box-averaging downscale to fit `edge` on the longest side, or a passthrough
/// if it already does.
///
/// Averaging rather than nearest-neighbour because this reduction is roughly
/// 2:1, where dropping pixels visibly aliases. It runs once per track on a
/// background thread, so the cost is irrelevant.
fn shrink(width: u32, height: u32, src: &[u8], edge: u32) -> Cover {
    let longest = width.max(height);
    if longest <= edge {
        return Cover::build(width, height, src.to_vec());
    }

    let dst_w = (width * edge / longest).max(1);
    let dst_h = (height * edge / longest).max(1);
    let mut rgb = Vec::with_capacity((dst_w * dst_h * 3) as usize);

    for y in 0..dst_h {
        // Half-open source rows for this output row, forced to cover at least
        // one so the averaging divisor is never zero.
        let y0 = y * height / dst_h;
        let y1 = ((y + 1) * height / dst_h).clamp(y0 + 1, height);
        for x in 0..dst_w {
            let x0 = x * width / dst_w;
            let x1 = ((x + 1) * width / dst_w).clamp(x0 + 1, width);

            let (mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32);
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let i = ((sy * width + sx) * 3) as usize;
                    r += u32::from(src[i]);
                    g += u32::from(src[i + 1]);
                    b += u32::from(src[i + 2]);
                    n += 1;
                }
            }
            rgb.extend_from_slice(&[(r / n) as u8, (g / n) as u8, (b / n) as u8]);
        }
    }

    Cover::build(dst_w, dst_h, rgb)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 2x2 image of solid quadrant colours, as raw RGB.
    fn quad() -> Vec<u8> {
        vec![
            255, 0, 0, // red
            0, 255, 0, // green
            0, 0, 255, // blue
            255, 255, 255, // white
        ]
    }

    #[test]
    fn small_images_pass_through_unscaled() {
        let cover = shrink(2, 2, &quad(), MAX_EDGE);
        assert_eq!((cover.width, cover.height), (2, 2));
        assert_eq!(cover.pixel(0, 0), (255, 0, 0));
        assert_eq!(cover.pixel(1, 1), (255, 255, 255));
    }

    #[test]
    fn shrink_preserves_aspect_ratio() {
        // Twice the ceiling on the long edge, so this actually shrinks.
        let (w, h) = (MAX_EDGE * 2, MAX_EDGE * 2 * 9 / 16);
        let src = vec![128; (w * h * 3) as usize];
        let cover = shrink(w, h, &src, MAX_EDGE);
        assert_eq!((cover.width, cover.height), (MAX_EDGE, MAX_EDGE * 9 / 16));
    }

    #[test]
    fn the_largest_thumbnail_youtube_serves_is_kept_whole() {
        // 1280x720 is maxresdefault, and nothing about it may be thrown away
        // before the renderer has said what size pane it has.
        let src = vec![128; (1280 * 720 * 3) as usize];
        let cover = shrink(1280, 720, &src, MAX_EDGE);
        assert_eq!((cover.width, cover.height), (1280, 720));
    }

    #[test]
    fn resample_hits_the_requested_size() {
        let cover = Cover::solid(100, 50);
        assert_eq!(cover.resample(37, 91).len(), 37 * 91 * 3);
        // Upscale and downscale alike: the sixel pane asks for both.
        assert_eq!(cover.resample(400, 200).len(), 400 * 200 * 3);
        assert_eq!(cover.resample(1, 1), vec![128, 128, 128]);
    }

    #[test]
    fn resample_keeps_flat_colour_flat() {
        // Interpolation must not ring or drift on constant input.
        let cover = Cover::from_rgb(4, 4, vec![70; 4 * 4 * 3]);
        assert!(cover.resample(9, 7).iter().all(|v| *v == 70));
    }

    #[test]
    fn resample_interpolates_between_pixels() {
        // Two pixels, black then white. Doubling the width puts the new
        // in-between samples at intermediate greys rather than duplicating.
        let cover = Cover::from_rgb(2, 1, vec![0, 0, 0, 255, 255, 255]);
        let wide = cover.resample(4, 1);
        let greys: Vec<u8> = wide.chunks_exact(3).map(|p| p[0]).collect();
        assert_eq!(greys.first(), Some(&0));
        assert_eq!(greys.last(), Some(&255));
        assert!(
            greys.windows(2).all(|w| w[0] <= w[1]),
            "must ramp monotonically, got {greys:?}"
        );
        assert!(greys[1..3].iter().all(|v| *v > 0 && *v < 255), "{greys:?}");
    }

    #[test]
    fn shrink_averages_rather_than_dropping_pixels() {
        // 2*MAX_EDGE wide, so each output pixel covers exactly two input ones:
        // a black column next to a white one must average to mid grey.
        let (w, h) = (MAX_EDGE * 2, 2);
        let mut src = Vec::new();
        for _ in 0..h {
            for x in 0..w {
                let v = if x % 2 == 0 { 0 } else { 254 };
                src.extend_from_slice(&[v, v, v]);
            }
        }
        let cover = shrink(w, h, &src, MAX_EDGE);
        assert_eq!(cover.width, MAX_EDGE);
        assert_eq!(cover.pixel(0, 0), (127, 127, 127));
    }

    #[test]
    fn pixel_clamps_out_of_range_coordinates() {
        let cover = shrink(2, 2, &quad(), MAX_EDGE);
        assert_eq!(cover.pixel(99, 99), cover.pixel(1, 1));
    }

    /// Builds an image with `bar` down both sides and `art` in the middle,
    /// which is exactly the shape square album art arrives in.
    fn pillarboxed(width: u32, height: u32, bars: u32) -> Vec<u8> {
        let mut rgb = Vec::new();
        for _ in 0..height {
            for x in 0..width {
                let inside = x >= bars && x < width - bars;
                rgb.extend_from_slice(if inside { &[200, 30, 90] } else { &[0, 0, 0] });
            }
        }
        rgb
    }

    #[test]
    fn trims_pillarbox_bars() {
        let rgb = pillarboxed(20, 10, 5);
        assert_eq!(trim_bars(20, 10, &rgb), (5, 0, 10, 10));
    }

    #[test]
    fn trims_letterbox_bars() {
        // Same thing rotated: flat rows top and bottom, art between them.
        let mut rgb = Vec::new();
        for y in 0..10 {
            for _ in 0..20 {
                let inside = (3..7).contains(&y);
                rgb.extend_from_slice(if inside { &[200, 30, 90] } else { &[0, 0, 0] });
            }
        }
        assert_eq!(trim_bars(20, 10, &rgb), (0, 3, 20, 4));
    }

    #[test]
    fn leaves_a_full_bleed_image_alone() {
        // Every edge differs from its neighbour, so there is nothing to trim.
        let mut rgb = Vec::new();
        for y in 0..10u32 {
            for x in 0..20u32 {
                rgb.extend_from_slice(&[(x * 12) as u8, (y * 25) as u8, 60]);
            }
        }
        assert_eq!(trim_bars(20, 10, &rgb), (0, 0, 20, 10));
    }

    #[test]
    fn a_flat_image_survives_trimming() {
        // Nothing but bar-coloured pixels: the trim must stop at its limit and
        // still leave a usable rect rather than cropping to nothing.
        let rgb = vec![7; 20 * 10 * 3];
        let (x, y, w, h) = trim_bars(20, 10, &rgb);
        assert!(w >= 1 && h >= 1, "got {w}x{h}");
        assert!(x + w <= 20 && y + h <= 10);
    }

    #[test]
    fn cropping_keeps_the_art_pixels() {
        let rgb = pillarboxed(20, 10, 5);
        let (x, y, w, h) = trim_bars(20, 10, &rgb);
        let cover = shrink(w, h, &crop(20, &rgb, x, y, w, h), MAX_EDGE);
        assert_eq!((cover.width, cover.height), (10, 10));
        // Every surviving pixel is art, none of the black padding.
        assert_eq!(cover.pixel(0, 0), (200, 30, 90));
        assert_eq!(cover.pixel(9, 9), (200, 30, 90));
    }

    /// The point of weighting by saturation: a sleeve that is mostly black with
    /// one coloured mark on it reads as that colour, not as black. Averaging
    /// would answer "very dark grey", which is no accent at all.
    #[test]
    fn a_mark_on_a_dark_sleeve_is_the_accent() {
        let (w, h) = (40u32, 40u32);
        let mut rgb = Vec::new();
        for y in 0..h {
            for x in 0..w {
                // A crimson square over an eighth of the picture.
                let mark = (4..14).contains(&x) && (4..14).contains(&y);
                rgb.extend_from_slice(if mark { &[200, 30, 60] } else { &[6, 6, 8] });
            }
        }
        let (r, g, b) = accent_of(&rgb);
        assert!(r > g && r > b, "expected a red accent, got {r},{g},{b}");
        assert!(r > 150, "and a visible one: {r},{g},{b}");
    }

    /// The larger of two colours wins, so an accent is the colour the sleeve is
    /// rather than the brightest thing anywhere on it.
    #[test]
    fn the_dominant_colour_wins_over_a_scattering() {
        let (w, h) = (40u32, 40u32);
        let mut rgb = Vec::new();
        for y in 0..h {
            for x in 0..w {
                // Two pixels of vivid yellow against a field of teal.
                let fleck = y == 0 && x < 2;
                rgb.extend_from_slice(if fleck {
                    &[255, 255, 0]
                } else {
                    &[20, 140, 140]
                });
            }
        }
        let (r, g, b) = accent_of(&rgb);
        assert!(g > r && b > r, "expected the teal field, got {r},{g},{b}");
    }

    /// A sleeve with no colour on it has no accent, and one must not be
    /// invented -- a colour on the border that is nowhere in the picture is a
    /// lie about the record.
    #[test]
    fn a_black_sleeve_gets_a_neutral_accent() {
        let (r, g, b) = accent_of(&[2; 40 * 40 * 3]);
        assert_eq!(r, g, "{r},{g},{b}");
        assert_eq!(g, b, "{r},{g},{b}");
        assert!(r > 100, "and still legible on a dark terminal: {r}");
    }

    /// A deep navy is honestly navy, and a navy line on a black terminal is a
    /// line nobody can see. Lifting keeps the hue and raises the brightness.
    #[test]
    fn a_dark_accent_is_lifted_without_changing_hue() {
        let (r, g, b) = lift((10, 20, 40));
        assert!(b > 150, "lifted to legible: {r},{g},{b}");
        // The ratios that make it navy rather than teal are preserved.
        assert_eq!(b / r, 4, "{r},{g},{b}");
        assert_eq!(b / g, 2, "{r},{g},{b}");
    }

    #[test]
    fn an_already_bright_accent_is_left_alone() {
        assert_eq!(lift((240, 30, 90)), (240, 30, 90));
        // Black has no hue to preserve, so scaling it would be division by
        // zero rather than a brightening.
        assert_eq!(lift((0, 0, 0)), (0, 0, 0));
    }

    #[test]
    fn decode_rejects_non_jpeg_input() {
        assert!(decode(b"this is not a jpeg", MAX_EDGE).is_err());
    }

    /// Exercises the whole live path -- URL shape, TLS, decode, downscale --
    /// against i.ytimg.com. Ignored by default so the suite stays runnable
    /// offline; run it with `cargo test -- --ignored` after touching any of it.
    #[test]
    #[ignore = "requires network access"]
    fn fetches_a_real_thumbnail() {
        let cover = fetch("dQw4w9WgXcQ").expect("thumbnail should fetch and decode");
        // Exact dimensions depend on how much padding this particular upload
        // arrives with, so assert the invariants instead. The lower bound is
        // the point of the ladder: anything under 720 on the long edge means a
        // rung was skipped and a small copy came back in place of the big one.
        let longest = cover.width.max(cover.height);
        assert!(
            (720..=MAX_EDGE).contains(&longest),
            "long edge is {longest}"
        );
        assert_eq!(cover.rgb.len() as u32, cover.width * cover.height * 3);
    }
}
