# MTUI

A terminal music player, written in Rust, built to stay small in memory.

Search YouTube, hit Enter, and audio starts before the track has finished
downloading. Cover art renders as sixel pixels in terminals that support it.
Signing in with a Google account is optional and adds your playlists and
liked tracks.

## How it works

Three threads, so nothing that can block ever sits on the render path:

- the main thread draws with [ratatui](https://ratatui.rs) and reads input
- a player thread owns audio — [rodio](https://github.com/RustAudio/rodio)
  decoding AAC-LC over a chunked HTTP `Read + Seek` source, so playback starts
  on the first chunk rather than the last
- a source worker shells out to `yt-dlp` to turn a video id into a signed
  stream URL, then exits

`yt-dlp` is never kept alive: it costs ~80 MB while running, and all streaming
and decoding happens in-process once it has handed over a URL.

## Requirements

- Rust (edition 2024)
- [`yt-dlp`](https://github.com/yt-dlp/yt-dlp) on `PATH`
- a JavaScript runtime — Deno, Node, or Bun — which `yt-dlp` has needed since
  v2025.11.12 to solve YouTube's nsig/PO-token challenges

## Run

```sh
cargo run --release
```

## Keys

| | |
|---|---|
| `/` or `i` | search |
| `j` / `k`, arrows | move |
| `Enter` | play |
| `Space` | pause |
| `s` | stop |
| `←` / `→` | seek 5s |
| `+` / `-` | volume |
| `c` | cover size |
| `L` or `Ctrl-L` | library |
| `a` / `d` / `f` | add to playlist / remove / like |
| `A` | sign in |
| `q` | quit |

## Signing in

Optional — search and playback work without it. The OAuth client is not baked
into the binary, because Google treats the YouTube scopes as sensitive and a
publicly shipped client would need verification review. Launch the app and
press `A`; if no client is configured it prints the full setup procedure.

Credentials live in `%APPDATA%\mtui` on Windows, `$XDG_CONFIG_HOME/mtui`
elsewhere — never in this repo.
