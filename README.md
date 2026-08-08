# MTUI

A terminal music player, written in Rust, built to stay small in memory.

Search YouTube, hit Enter, and audio starts before the track has finished
downloading. Cover art renders as sixel pixels in terminals that support it.
Signing in with a Google account is optional and adds your playlists and
liked tracks.

## How it works

Nothing that can block ever sits on the render path:

- the main thread draws with [ratatui](https://ratatui.rs) and reads input
- a player thread owns audio — [rodio](https://github.com/RustAudio/rodio)
  decoding AAC-LC over a chunked HTTP `Read + Seek` source, so playback starts
  on the first chunk rather than the last
- a source worker shells out to `yt-dlp` to turn a video id into a signed
  stream URL, then exits
- cover art, the library, and the player page's panels each get a thread of
  their own, so a slow thumbnail or a two-request comment fetch can never sit
  in front of the resolve that actually produces audio

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

## The landing page

It opens on a page of shelves — the same shape YouTube Music's home page has,
drawn as rows of cards. `hjkl` moves around the grid, `Enter` plays a song or
opens an album or playlist into a track list, and `H` or `Esc` comes back.

How personal that page is depends on what you have configured:

| | Shelves |
|---|---|
| nothing | Trending, New releases, charts, community playlists |
| played a few tracks | **Listen again**, **Quick picks**, **Similar to …**, **Forgotten favorites**, then the above |
| cookie saved | your real feed, exactly as the web player shows it — including **Heard in Shorts** |

Without a cookie the shelves are built here rather than fetched, and they are
ranked from what you have actually played. MTUI keeps a journal of its own plays
— what, when, and how far through — and scores each track on how often it comes
up, how recently, and how often you skip it. Likes count too, but as a prior
rather than as the whole answer.

That distinction is the point. YouTube's own shelves come from watch history,
which no API this program can reach exposes — the Data API's history playlist
has returned empty since 2016. The history it *can* reach is the one it makes.

- **Listen again** — best-scoring tracks you have played before, minus anything
  from the last six hours. Suggesting back a song you played twenty minutes ago
  is what makes a shelf look like it is not paying attention.
- **Quick picks** — four radio stations seeded from four *different* artists and
  interleaved, so the first screenful carries all four. One seed is one mood;
  this is why the shelf no longer reads the same every launch.
- **Forgotten favorites** — genuinely loved, genuinely stale: played properly,
  then untouched for two months.
- Nothing appears on the page twice, and `r` rebuilds it from different seeds.

The journal lives in `plays.jsonl` beside the other config, is never uploaded on
its own, and deleting it resets the page to its first-run behaviour. On a first
run there is nothing to rank, so the shelves fall back to your likes and improve
from there.

A saved cookie gets the real thing. It is the only way: **OAuth cannot reach
this feed.** A Bearer token on an InnerTube call is routed through Google's API
gateway and refused with `INVALID_ARGUMENT`, whatever client identity travels
with it, while the same token works perfectly against the Data API. There is a
test that pins this (`cargo test oauth_is_refused -- --ignored`) so the next
person to wonder does not have to find out the hard way.

### Getting a cookie

**Normally you do nothing.** On first launch MTUI reads the session out of your
browser itself, using the yt-dlp it already downloaded for other reasons — it
knows how to open a cookie jar on every platform, including the decryption that
makes it awkward. The landing page reloads as soon as it succeeds, and the
browser it used is named in the status bar.

Every browser is tried and the first with a YouTube session wins, so this keeps
working if you switch. Which ones *can* work is not up to MTUI:

| | |
|---|---|
| Firefox | works everywhere — a plain SQLite jar |
| Chrome, Edge, most Chromium forks | **fail on Windows.** Chrome 127 added App-Bound Encryption, sealing cookies with a key tied to the browser binary specifically so other programs cannot read them ([yt-dlp#10927](https://github.com/yt-dlp/yt-dlp/issues/10927)) |
| macOS, Linux | fine across the board, though a keychain may prompt once |

The browser that worked is remembered, so later launches go straight to it
instead of probing the list — that matters, since each probe costs a process
spawn. When a cookie expires, MTUI notices YouTube has stopped recognising it
and re-reads the browser by itself. Nothing to re-paste.

### Pasting one by hand

Only needed if the automatic path cannot work — Chrome on Windows, or a browser
that is signed out. In a browser signed in to music.youtube.com: open the
network tab, reload, click any request to `music.youtube.com`, and copy the
whole `cookie:` request header. Save it as one line in `cookies.txt` next to the
other config:

```
%APPDATA%\mtui\cookies.txt          # Windows
$XDG_CONFIG_HOME/mtui/cookies.txt   # elsewhere
```

A hand-written `cookies.txt` always wins over an imported one — if you pasted a
header deliberately, that is the session you meant.

Requests are signed the way Google's own web player signs them — a SHA-1 over
the timestamp, the `SAPISID` cookie and the origin. The cookie never leaves your
machine except back to YouTube.

### Why a cookie at all

Because there is no alternative, and this has been measured rather than assumed.
InnerTube refuses OAuth outright — not just a client you registered yourself,
but Google's own living-room TV client too. Two ignored tests pin it:

```
cargo test oauth_is_refused -- --ignored    # your Cloud Console client → 400
cargo test tv_client       -- --ignored    # Google's TV client        → 400
```

The root cause is that Google publishes no OAuth scope for watch history or the
music feed at all. The Data API covers likes, playlists, subscriptions and
uploads; the `HL` history playlist has returned empty since 2016. No sign-in can
grant a permission that does not exist, which is why "just add a proper login"
is not an available fix.

### What syncs back

With a cookie saved, plays made in MTUI are reported to YouTube the same way the
web player reports them, so they land in your real listening history and feed
everywhere else. Liking a track with `f` already wrote through to your account
via the Data API, cookie or not.

Both are best-effort by nature. The reporting endpoints are undocumented and
answer `204` whether they accepted a play or discarded it, so the only real
check is whether your history fills up — there is an ignored test that reports
one play and tells you where to look:

```
cargo test reports_a_play -- --ignored --nocapture
```

Nothing here is load-bearing. A play that fails to report is still played, still
journalled, and still ranked on MTUI's own shelves.

Nothing is fetched for the page beyond one round trip at launch, plus three
more behind it for the built shelves. No cover art is fetched per card: that
would be a request per box on screen, for pictures a terminal cell grid cannot
show many of anyway.

## The player page

Playing anything opens it: the cover on the left with the title, artist and a
progress bar under it, and beside it the same four panels YouTube Music puts
there.

| Tab | |
|---|---|
| **Up next** | the queue — the radio YouTube builds around the track, or the playlist it was played from |
| **Lyrics** | when the track has any; instrumentals and most videos do not |
| **Comments** | the first page, top-level only |
| **Related** | "You might also like" and the recommended playlists, as one list |

`h` / `l` or `Tab` walk the tabs and `1`–`4` jump straight to one. Only the
queue is fetched when a track starts, because it decides what plays next; the
other three are one request each, made the first time you open that tab.

**The lyrics follow the singer.** When the line timings are available the panel
marks the line being sung, dims the rest of the song around it, and scrolls
itself to keep it in the middle. Scrolling it yourself hands it back — it stays
where you put it — and pressing `2` again picks the song back up.

YouTube publishes timings only for its own catalogue, so for a cover, a live
take or anything not uploaded by a label they come from
[LRCLIB](https://lrclib.net) instead, credited in the panel. When neither has
them the tab still shows the words, just without the highlight.

**The queue plays itself.** When a track ends the next one starts, and it has
already been resolved in the background while the current one played — so the
gap is a channel round trip rather than the several seconds a cold resolve
costs. `n` and `p` skip through it by hand, `Enter` on a row jumps to it, and
`s` stops and closes the page. `Esc` leaves the page with the music still
playing; `P` brings it back from wherever you went, and the hints along the
bottom name it for as long as something is playing off-page.

A track the queue offers that will not resolve is stepped over rather than
allowed to end the session — up to three in a row, after which it stops and
says so.

## Playing without a window

`B`, while something is playing, hands the terminal back and leaves MTUI running
with an icon in the notification area. Clicking the icon brings the interface
back in a new window; right-clicking it offers play/pause, next, previous and
quit. The queue keeps advancing the whole time, so a radio started before
backgrounding carries on by itself.

It has to be a key rather than the window's close button, and that is a limit of
Windows rather than a choice. Closing a console window sends every process
attached to it `CTRL_CLOSE_EVENT` and then kills them; detaching from inside
that handler does not save the process, which was measured against Windows
Terminal on Windows 11 rather than assumed. Detaching has to happen *before* the
close, so it has to be asked for. Close the window without pressing `B` and the
music stops with it.

Windows only, and refused elsewhere with a message saying so: detaching from a
controlling terminal on Unix is a different mechanism that has to happen at
startup, and there is no notification area to put the result in.

Comments come from youtube.com rather than YouTube Music, which does not serve
them. On a network that enforces YouTube's Restricted Mode — some ISPs and most
schools do, by pointing `www.youtube.com` at `restrictmoderate.youtube.com` —
the panel will say so, and no request from here can change it. The other three
tabs are unaffected.

## Keys

| | |
|---|---|
| `/` or `i` | search |
| `h` / `j` / `k` / `l`, arrows | move (on the landing page) |
| `j` / `k`, arrows | move (in a list, or scroll a player-page panel) |
| `h` / `l`, `Tab`, `1`–`4` | switch tabs (on the player page) |
| `H` or `Esc` | back to the landing page, or out of the player page |
| `r` | refresh the landing page — rebuilds it from different seeds |
| `Enter` | play, or open what the card stands for |
| `Space` | pause |
| `n` / `p` | next / previous in the queue |
| `P` or `p` | back to the player page (in a list; `p` is previous on the page itself) |
| `B` | keep playing without the terminal (Windows) |
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
