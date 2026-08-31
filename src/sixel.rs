//! Sixel encoding: turns an RGB image into the escape sequence a terminal
//! paints as actual pixels.
//!
//! Half-blocks cap a cover at two pixels per text row, which is why they look
//! like mosaics. Sixel has no such ceiling -- the terminal draws the bitmap at
//! its own pixel resolution, so a pane 44 columns wide carries roughly 440x440
//! pixels instead of 44x46.
//!
//! The format is a 1970s DEC protocol and reads like one. Pixels are written in
//! horizontal bands six rows tall, one pass per colour: within a pass, each
//! character carries a six-bit column mask saying which of those six rows the
//! current colour occupies. `$` returns to the start of the band for the next
//! colour, `-` moves down to the next band. Colours are indices into a palette
//! declared up front, so an adaptive palette is not an optimisation here -- it
//! is the only thing on offer.

use std::collections::HashMap;

/// Colour registers we declare. 256 is what terminals reliably provide, and
/// enough that album art quantises without visible banding.
const MAX_COLOURS: usize = 256;

/// Encodes an RGB image as a complete sixel sequence, DCS introducer through
/// string terminator.
pub fn encode(rgb: &[u8], width: u32, height: u32) -> Vec<u8> {
    debug_assert_eq!(rgb.len() as u32, width * height * 3, "pixel count");
    if width == 0 || height == 0 {
        return Vec::new();
    }

    let (palette, indices) = quantise(rgb);

    let mut out = Vec::with_capacity(rgb.len() / 3);
    out.extend_from_slice(b"\x1bPq");
    // Raster attributes: square pixels, and the extent up front so the terminal
    // can reserve the area before it starts painting.
    out.extend_from_slice(format!("\"1;1;{width};{height}").as_bytes());
    for (i, c) in palette.iter().enumerate() {
        // Sixel colour components are percentages of full scale, not bytes, so
        // a channel survives with about 1% precision. Rounding to nearest
        // rather than truncating matters: truncation biases every channel down
        // and visibly darkens the whole image.
        let pc = |v: u8| (u32::from(v) * 100 + 127) / 255;
        out.extend_from_slice(format!("#{i};2;{};{};{}", pc(c[0]), pc(c[1]), pc(c[2])).as_bytes());
    }

    for y0 in (0..height).step_by(6) {
        let rows = 6.min(height - y0);

        // Build every colour's bit-columns for this band in one pass over it.
        // Emitting straight from the index buffer instead would mean rescanning
        // the whole band once per colour present in it.
        let mut slot_of = vec![usize::MAX; palette.len()];
        let mut slots: Vec<(usize, Vec<u8>)> = Vec::new();
        for r in 0..rows {
            let row = &indices[((y0 + r) * width) as usize..][..width as usize];
            for (x, &colour) in row.iter().enumerate() {
                let slot = match slot_of[usize::from(colour)] {
                    usize::MAX => {
                        slot_of[usize::from(colour)] = slots.len();
                        slots.push((usize::from(colour), vec![0u8; width as usize]));
                        slots.len() - 1
                    }
                    slot => slot,
                };
                slots[slot].1[x] |= 1 << r;
            }
        }

        for (i, (colour, bits)) in slots.iter().enumerate() {
            if i > 0 {
                out.push(b'$');
            }
            out.extend_from_slice(format!("#{colour}").as_bytes());
            // Trailing empty columns paint nothing, so stopping at the last set
            // one is free size. `bits` always has a set column: a slot only
            // exists because some pixel put it there.
            let last = bits.iter().rposition(|b| *b != 0).unwrap_or(0);
            emit_runs(&mut out, &bits[..=last]);
        }
        out.push(b'-');
    }

    out.extend_from_slice(b"\x1b\\");
    out
}

/// Writes bit-columns as sixel characters, run-length encoding repeats.
///
/// `!<n><char>` costs three bytes at minimum, so runs shorter than four are
/// cheaper written out.
fn emit_runs(out: &mut Vec<u8>, bits: &[u8]) {
    let mut run = (0u8, 0u32);
    for &b in bits {
        let ch = 0x3f + b;
        if run.1 > 0 && ch == run.0 {
            run.1 += 1;
        } else {
            flush_run(out, run);
            run = (ch, 1);
        }
    }
    flush_run(out, run);
}

fn flush_run(out: &mut Vec<u8>, (ch, len): (u8, u32)) {
    match len {
        0 => {}
        1..=3 => out.extend(std::iter::repeat_n(ch, len as usize)),
        _ => {
            out.extend_from_slice(format!("!{len}").as_bytes());
            out.push(ch);
        }
    }
}

/// Reduces the image to at most [`MAX_COLOURS`] colours by median cut,
/// returning the palette and one palette index per pixel.
///
/// Median cut rather than a fixed colour cube because album art is usually a
/// handful of related tones: a cube spends most of its entries on colours the
/// image does not contain and then bands the ones it does.
#[allow(unknown_lints)]
#[allow(clippy::chunks_exact_to_as_chunks)]
fn quantise(rgb: &[u8]) -> (Vec<[u8; 3]>, Vec<u8>) {
    let mut counts: HashMap<[u8; 3], u32> = HashMap::new();
    for px in rgb.chunks_exact(3) {
        *counts.entry([px[0], px[1], px[2]]).or_insert(0) += 1;
    }
    // Sorted so the palette does not depend on hash iteration order; an image
    // must encode to the same bytes every time.
    let mut colours: Vec<([u8; 3], u32)> = counts.into_iter().collect();
    colours.sort_unstable();

    let boxes = median_cut(colours);

    let mut palette = Vec::with_capacity(boxes.len());
    let mut lookup: HashMap<[u8; 3], u8> = HashMap::new();
    for (i, group) in boxes.iter().enumerate() {
        palette.push(average(group));
        for (colour, _) in group {
            lookup.insert(*colour, i as u8);
        }
    }

    // Every colour in the image came from some box, so this cannot miss.
    let indices = rgb
        .chunks_exact(3)
        .map(|px| lookup[&[px[0], px[1], px[2]]])
        .collect();

    (palette, indices)
}

/// Splits the colour set until there are [`MAX_COLOURS`] groups, always cutting
/// the group with the widest spread on any single channel.
fn median_cut(colours: Vec<([u8; 3], u32)>) -> Vec<Vec<([u8; 3], u32)>> {
    let mut boxes = vec![colours];

    while boxes.len() < MAX_COLOURS {
        // Widest spread first: that is where quantisation error is largest.
        let Some((_, index, channel)) = boxes
            .iter()
            .enumerate()
            .filter(|(_, group)| group.len() > 1)
            .map(|(i, group)| {
                let (channel, spread) = widest_channel(group);
                (spread, i, channel)
            })
            .max()
        else {
            // Every group is a single colour: the image has fewer distinct
            // colours than the palette holds, and nothing is lost.
            break;
        };

        let mut group = boxes.swap_remove(index);
        group.sort_unstable_by_key(|(c, _)| c[channel]);

        // Cut at the weighted median so both halves carry a similar number of
        // pixels, rather than a similar number of distinct colours -- one rare
        // outlier colour should not claim half a box.
        let half = group.iter().map(|(_, n)| u64::from(*n)).sum::<u64>() / 2;
        let mut running = 0;
        let mut at = 1;
        for (i, (_, n)) in group.iter().enumerate() {
            running += u64::from(*n);
            if running > half {
                at = i.clamp(1, group.len() - 1);
                break;
            }
        }

        let rest = group.split_off(at);
        boxes.push(group);
        boxes.push(rest);
    }

    boxes
}

/// The channel a group varies most on, and by how much.
fn widest_channel(group: &[([u8; 3], u32)]) -> (usize, u8) {
    (0..3)
        .map(|c| {
            let lo = group.iter().map(|(v, _)| v[c]).min().unwrap_or(0);
            let hi = group.iter().map(|(v, _)| v[c]).max().unwrap_or(0);
            (c, hi - lo)
        })
        .max_by_key(|(_, spread)| *spread)
        .unwrap_or((0, 0))
}

/// Pixel-weighted mean of a group, which is the palette entry that minimises
/// visible error for the pixels actually mapped to it.
fn average(group: &[([u8; 3], u32)]) -> [u8; 3] {
    let mut sums = [0u64; 3];
    let mut total = 0u64;
    for (colour, count) in group {
        for (s, v) in sums.iter_mut().zip(colour) {
            *s += u64::from(*v) * u64::from(*count);
        }
        total += u64::from(*count);
    }
    if total == 0 {
        return [0; 3];
    }
    [
        (sums[0] / total) as u8,
        (sums[1] / total) as u8,
        (sums[2] / total) as u8,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sixel reader, so the encoder is checked against the format rather than
    /// against itself. Understands exactly the subset `encode` emits.
    fn decode(sixel: &[u8], width: u32, height: u32) -> Vec<u8> {
        let mut palette = [[0u8; 3]; MAX_COLOURS];
        let mut out = vec![0u8; (width * height * 3) as usize];
        let mut colour = 0usize;
        let (mut x, mut y) = (0u32, 0u32);

        let body = sixel
            .windows(3)
            .position(|w| w == b"\x1bPq")
            .map(|i| &sixel[i + 3..])
            .expect("DCS introducer");
        let mut i = 0;
        let number = |bytes: &[u8], i: &mut usize| -> u32 {
            let start = *i;
            while *i < bytes.len() && bytes[*i].is_ascii_digit() {
                *i += 1;
            }
            std::str::from_utf8(&bytes[start..*i])
                .unwrap()
                .parse()
                .unwrap()
        };

        while i < body.len() {
            match body[i] {
                b'"' => {
                    // Raster attributes: four numbers we do not need back.
                    i += 1;
                    for _ in 0..4 {
                        number(body, &mut i);
                        if body.get(i) == Some(&b';') {
                            i += 1;
                        }
                    }
                }
                b'#' => {
                    i += 1;
                    let index = number(body, &mut i) as usize;
                    if body.get(i) == Some(&b';') {
                        i += 1;
                        let mode = number(body, &mut i);
                        assert_eq!(mode, 2, "RGB colour space");
                        let mut rgb = [0u8; 3];
                        for channel in &mut rgb {
                            i += 1; // the ';'
                            *channel = ((number(body, &mut i) * 255 + 50) / 100) as u8;
                        }
                        palette[index] = rgb;
                    }
                    colour = index;
                }
                b'!' => {
                    i += 1;
                    let count = number(body, &mut i);
                    let bits = body[i] - 0x3f;
                    i += 1;
                    for _ in 0..count {
                        paint(&mut out, width, height, x, y, bits, palette[colour]);
                        x += 1;
                    }
                }
                b'$' => {
                    x = 0;
                    i += 1;
                }
                b'-' => {
                    x = 0;
                    y += 6;
                    i += 1;
                }
                0x1b => break,
                b => {
                    assert!((0x3f..=0x7e).contains(&b), "unexpected byte {b:#x}");
                    paint(&mut out, width, height, x, y, b - 0x3f, palette[colour]);
                    x += 1;
                    i += 1;
                }
            }
        }
        out
    }

    fn paint(out: &mut [u8], width: u32, height: u32, x: u32, y: u32, bits: u8, rgb: [u8; 3]) {
        for r in 0..6 {
            if bits & (1 << r) == 0 || y + r >= height || x >= width {
                continue;
            }
            let i = (((y + r) * width + x) * 3) as usize;
            out[i..i + 3].copy_from_slice(&rgb);
        }
    }

    #[test]
    fn encodes_a_flat_image_compactly() {
        // Six red pixels in one row: one palette entry, one run.
        let rgb: Vec<u8> = std::iter::repeat_n([255, 0, 0], 6).flatten().collect();
        let out = encode(&rgb, 6, 1);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\x1bPq\"1;1;6;1#0;2;100;0;0#0!6@-\x1b\\"
        );
    }

    /// Sixel carries colour as a percentage per channel, so a perfect byte
    /// round trip is not available -- half a percent, about 1.3 levels, is the
    /// format's own floor.
    #[track_caller]
    fn assert_close(got: &[u8], want: &[u8], tolerance: u8) {
        assert_eq!(got.len(), want.len(), "size");
        if let Some((i, (g, w))) = got
            .iter()
            .zip(want)
            .enumerate()
            .find(|(_, (g, w))| g.abs_diff(**w) > tolerance)
        {
            panic!("byte {i}: decoded {g}, encoded {w} (tolerance {tolerance})");
        }
    }

    #[test]
    fn round_trips_a_multi_colour_image() {
        // 240 pixels, each a different colour: fewer than the palette holds, so
        // quantisation is lossless and only the format's own colour precision
        // stands between the input and what comes back.
        let (w, h) = (15u32, 16u32);
        let mut rgb = Vec::new();
        for y in 0..h {
            for x in 0..w {
                rgb.extend_from_slice(&[(x * 11) as u8, (y * 17) as u8, 64]);
            }
        }
        let decoded = decode(&encode(&rgb, w, h), w, h);
        assert_close(&decoded, &rgb, 2);
    }

    #[test]
    fn round_trips_an_image_taller_than_one_band() {
        // Height not a multiple of six exercises the short final band.
        let (w, h) = (8u32, 20u32);
        let mut rgb = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let v = ((x + y) % 2) as u8 * 255;
                rgb.extend_from_slice(&[v, 255 - v, 128]);
            }
        }
        // 0, 128 and 255 all land on exact percentages, so this one is lossless.
        assert_eq!(decode(&encode(&rgb, w, h), w, h), rgb);
    }

    #[test]
    fn quantises_down_to_the_palette_limit() {
        // Every pixel a different colour: more than the palette can hold, so
        // the result must be capped and every pixel still mapped.
        let (w, h) = (40u32, 40u32);
        let mut rgb = Vec::new();
        for i in 0..(w * h) {
            rgb.extend_from_slice(&[(i % 256) as u8, (i / 7 % 256) as u8, (i / 13 % 256) as u8]);
        }
        let (palette, indices) = quantise(&rgb);
        assert!(palette.len() <= MAX_COLOURS, "{} colours", palette.len());
        assert_eq!(indices.len() as u32, w * h);
        assert!(indices.iter().all(|i| usize::from(*i) < palette.len()));
    }

    #[test]
    #[allow(unknown_lints)]
    #[allow(clippy::chunks_exact_to_as_chunks)]
    fn quantisation_error_stays_small() {
        // Photographic-ish content: after quantisation no channel should be far
        // off, which is the property that makes the palette worth building.
        let (w, h) = (60u32, 60u32);
        let mut rgb = Vec::new();
        for y in 0..h {
            for x in 0..w {
                rgb.extend_from_slice(&[
                    (x * 4 % 256) as u8,
                    (y * 4 % 256) as u8,
                    ((x + y) * 2 % 256) as u8,
                ]);
            }
        }
        let (palette, indices) = quantise(&rgb);
        let worst = rgb
            .chunks_exact(3)
            .zip(&indices)
            .map(|(px, i)| {
                let p = palette[usize::from(*i)];
                (0..3).map(|c| px[c].abs_diff(p[c])).max().unwrap()
            })
            .max()
            .unwrap();
        assert!(worst <= 16, "worst channel error was {worst}");
    }

    #[test]
    fn encoding_is_deterministic() {
        let rgb: Vec<u8> = (0..300).map(|i| (i % 251) as u8).collect();
        assert_eq!(encode(&rgb, 10, 10), encode(&rgb, 10, 10));
    }
}
