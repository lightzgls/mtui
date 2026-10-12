//! Short, reversible strip transitions and their small Win32 icon bitmaps.

use std::time::{Duration, Instant};

pub(super) const STEPS: usize = 14;
pub(super) const DURATION: Duration = Duration::from_millis(280);

pub(super) struct Transition {
    from: f32,
    target: f32,
    started: Instant,
}

impl Transition {
    pub(super) fn new(playing: bool, now: Instant) -> Self {
        let target = if playing { 1.0 } else { 0.0 };
        Self { from: target, target, started: now }
    }

    pub(super) fn retarget(&mut self, playing: bool, now: Instant) -> bool {
        let target = if playing { 1.0 } else { 0.0 };
        if target == self.target { return false; }
        self.from = self.value(now);
        self.target = target;
        self.started = now;
        true
    }

    pub(super) fn value(&self, now: Instant) -> f32 {
        let t = (now.saturating_duration_since(self.started).as_secs_f32()
            / DURATION.as_secs_f32()).min(1.0);
        let eased = t * t * (3.0 - 2.0 * t);
        self.from + (self.target - self.from) * eased
    }

    pub(super) fn running(&self, now: Instant) -> bool {
        self.from != self.target && now.saturating_duration_since(self.started) < DURATION
    }

    pub(super) fn playing(&self) -> bool { self.target == 1.0 }

    pub(super) fn finish(&mut self) { self.from = self.target; }
}

/// A DWORD-aligned, bottom-up, 32-bit icon DIB followed by its AND mask.
/// Render once per tray size; the timer only selects cached handles.
pub(super) fn icon_bits(ink: [u8; 3], amount: f32, width: u32, height: u32) -> Vec<u32> {
    let pixels = (width * height) as usize;
    let mask_words = width.div_ceil(32) as usize;
    let mut bits = vec![0; 10 + pixels + mask_words * height as usize];
    bits[..10].copy_from_slice(&[
        40, width, height * 2, 0x0020_0001, 0,
        (pixels * 4 + mask_words * height as usize * 4) as u32, 0, 0, 0, 0,
    ]);
    let quiet = [170.0, 181.0, 189.0];
    let color: [f32; 3] = std::array::from_fn(|i| {
        quiet[i] + (ink[i] as f32 - quiet[i]) * amount
    });
    let xs = [128.0, 212.0, 296.0, 380.0];
    let lengths = [128.0, 208.0, 264.0, 160.0];
    let strip_width = 60.0 - 20.0 * amount;
    let strips: [_; 4] = std::array::from_fn(|i| {
        let strip_height = 40.0 + (lengths[i] - 40.0) * amount;
        RoundedRect::new(xs[i], 256.0, strip_width, strip_height, 20.0)
    });
    let tile = RoundedRect::new(256.0, 256.0, 480.0, 480.0, 128.0);
    const SAMPLES: u32 = 4;
    for y in 0..height {
        let row = (height - y - 1) as usize;
        for x in 0..width {
            let mut rgb = [0.0; 3];
            let mut coverage = 0;
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let px = (x as f32 + (sx as f32 + 0.5) / SAMPLES as f32)
                        * 512.0 / width as f32;
                    let py = (y as f32 + (sy as f32 + 0.5) / SAMPLES as f32)
                        * 512.0 / height as f32;
                    if !tile.contains(px, py) { continue; }
                    coverage += 1;
                    let sample = if strips.iter().any(|strip| strip.contains(px, py)) {
                        color
                    } else { [21.0, 27.0, 35.0] };
                    for i in 0..3 { rgb[i] += sample[i]; }
                }
            }
            let pixel = if coverage == 0 { 0 } else {
                let [r, g, b] = rgb.map(|sum| (sum / coverage as f32).round() as u32);
                let alpha = coverage * 255 / (SAMPLES * SAMPLES);
                (alpha << 24) | (r << 16) | (g << 8) | b
            };
            bits[10 + row * width as usize + x as usize] = pixel;
            if pixel == 0 {
                // Mask bits are MSB-first within each byte, even in a LE DWORD.
                let bit = (x % 32 / 8) * 8 + 7 - x % 8;
                bits[10 + pixels + row * mask_words + x as usize / 32] |= 1 << bit;
            }
        }
    }
    bits
}

struct RoundedRect {
    center_x: f32,
    center_y: f32,
    inner_x: f32,
    inner_y: f32,
    radius: f32,
}

impl RoundedRect {
    fn new(center_x: f32, center_y: f32, width: f32, height: f32, radius: f32) -> Self {
        Self { center_x, center_y, inner_x: width / 2.0 - radius,
            inner_y: height / 2.0 - radius, radius }
    }

    fn contains(&self, x: f32, y: f32) -> bool {
        let dx = ((x - self.center_x).abs() - self.inner_x).max(0.0);
        let dy = ((y - self.center_y).abs() - self.inner_y).max(0.0);
        dx * dx + dy * dy <= self.radius * self.radius
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tooltip_updates_do_not_restart_a_collapse() {
        let start = Instant::now();
        let mut transition = Transition::new(true, start);
        assert!(!transition.running(start));
        assert!(transition.retarget(false, start));
        assert_eq!(transition.value(start), 1.0);
        assert!(!transition.retarget(false, start + DURATION / 4));
        assert_eq!(transition.value(start + DURATION / 2), 0.5);
        assert_eq!(transition.value(start + DURATION), 0.0);
        assert!(!transition.running(start + DURATION));
    }

    #[test]
    fn a_second_toggle_reverses_from_the_current_shape_without_a_jump() {
        let start = Instant::now();
        let mut transition = Transition::new(true, start);
        transition.retarget(false, start);
        let halfway = start + DURATION / 2;
        assert!(transition.retarget(true, halfway));
        assert_eq!(transition.value(halfway), 0.5);
        assert!(transition.value(halfway + DURATION / 2) > 0.5);
        assert_eq!(transition.value(halfway + DURATION), 1.0);
        assert!(!transition.running(halfway + DURATION));
    }
}
