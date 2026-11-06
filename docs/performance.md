# Performance checks

The foreground polls workers every 200 ms, or 50 ms during interactive loading.
Rendering is separate: input, completed source work, playback state changes and
lyric cues redraw immediately on the next poll. Static content refreshes at most
once per second. Overflowing titles retain their 200 ms marquee animation.

On Windows, the foreground input pipe waits up to 8 ms between empty reads,
instead of 2 ms. This reduces idle pipe checks without changing audio scheduling.
The startup handshake keeps its separate 2 ms interval. OS scheduling can add
latency beyond these requested sleep durations.

Audio remains on its own thread with a single downloader worker and bounded
stream storage. Background mode draws no terminal frames and does not fetch
hidden lyrics or comments. Network/helper latency and audio decoding require
separate measurements from rendering.

## Artwork memory

Cell and ASCII player covers retain at most 128×128 RGB pixels (48 KiB).
Kitty/SIXEL player images keep up to the existing 1280-pixel edge. Card artwork
uses a 64-pixel edge in terminal-cell mode and 256 pixels with Kitty/SIXEL;
the existing 128-entry, 8 MiB pixel budget still applies. At 64×64, 128 card
bitmaps occupy at most 1.5 MiB of RGB data.

Switching to cells immediately shrinks cached bitmaps. Switching to real images
keeps the small preview while fetching larger current/visible artwork. Late
responses cannot replace a larger image with a small one or retain large pixels
after a downgrade. Google artwork URLs with explicit dimensions request smaller
copies in cell mode; opaque URLs are decoded/downscaled locally. Metadata URLs
used by desktop integrations retain their original dimensions.

These are pixel-storage bounds, excluding decoder/HTTP buffers, worker overhead,
allocator retention and terminal-owned image memory; they are not process RSS.

## Reproduce the headless comparison

```powershell
cargo test --release --offline --locked --bin mtui app::testing::performance::compare_foreground_frame_work -- --ignored --exact --nocapture
```

This explicitly selected test does not run any other ignored/live tests. It
compares the old policy of drawing on every poll with the adaptive redraw policy
using the current App maintenance and rendering code. Each policy processes 1,000
polls representing 200 seconds, at 160×42 cells with synthetic cover art and
lyrics. Playing, paused and synced-lyrics views are exercised. The final visible
buffers must agree.

The JSON output reports frame count and elapsed processing time in microseconds.
Time spent waiting between polls is simulated, not slept. TestBackend does not
write to a real terminal. These numbers exclude actual audio decoding, network,
console helper, Discord/media integration, terminal rendering, and GPU work;
they are not whole-application CPU percentages. Animation-heavy views can still
draw at the original cadence. Measure the release player and its helpers during
real playback, pause, browsing and background mode before claiming a CPU or
battery improvement for the whole app.

## Recorded headless result

One Windows release run on 2026-10-17, using Rust 1.98.1 and the existing
size-optimized release profile:

| View | Previous frames | Adaptive frames | Previous work | Adaptive work | Less processing time |
| --- | ---: | ---: | ---: | ---: | ---: |
| Playing queue | 1,000 | 200 | 882.610 ms | 161.010 ms | 81.8% |
| Paused queue | 1,000 | 200 | 822.460 ms | 161.829 ms | 80.3% |
| Synced lyrics | 1,000 | 240 | 1,210.228 ms | 302.310 ms | 75.0% |

This is a single local measurement of the fixture above, not a statistical
benchmark or an estimate of total CPU, GPU, startup or track-loading performance.
All three final-buffer comparisons passed. The input-polling change is outside
this headless test and is not included in these timings.
