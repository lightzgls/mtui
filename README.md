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

## Install

Download `mtui.exe` from [releases](https://github.com/lightzgls/mtui/releases)
and run it. There is nothing else to install.

Most playback is handled in-process. For the cases it cannot cover — age-gated,
region-locked, and the capped URLs that licensed music resolves to — the app
needs [`yt-dlp`](https://github.com/yt-dlp/yt-dlp), and fetches it for you on
first run into `%LOCALAPPDATA%\mtui\bin` (`~/.cache/mtui/bin` elsewhere). That
takes a few seconds and happens once. A `yt-dlp` already on your `PATH` is used
as-is and never shadowed.

It is downloaded rather than bundled on purpose: yt-dlp ships roughly monthly to
keep up with YouTube, so a copy frozen into this binary would break within
months. If the download fails, the app says why and installs nothing.

A JavaScript runtime (Deno, Node, or Bun) is optional. `yt-dlp` uses one to
solve YouTube's nsig/PO-token challenges, so without one the fallback path can
still fail on a minority of tracks. The fast path never needs it.

## Build from source

```sh
cargo build --release
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
