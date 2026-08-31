<p align="center">
  <img src="assets/mtui-icon.svg" width="132" alt="MTUI icon">
</p>

<h1 align="center">MTUI</h1>

<p align="center">
  <strong>A small, native YouTube Music player for your terminal.</strong>
</p>

<p align="center">
  Fast navigation, native audio playback, bounded caches, and an optional browser window used only while signing in.
</p>

<p align="center">
  <a href="https://github.com/lightzgls/mtui/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/lightzgls/mtui/actions/workflows/ci.yml/badge.svg"></a>
  <a href="https://github.com/lightzgls/mtui/releases/latest"><img alt="Latest release" src="https://img.shields.io/github/v/release/lightzgls/mtui?color=49d7f2"></a>
  <a href="LICENSE"><img alt="MIT license" src="https://img.shields.io/badge/license-MIT-65f5b5"></a>
  <img alt="Rust 1.85+" src="https://img.shields.io/badge/rust-1.85%2B-f5a97f">
</p>

<!-- README_DEMO: Replace this paragraph with the GitHub video attachment URL on its own line. -->
<p align="center">
  <strong>Demo video coming soon.</strong><br>
  <sub>Search, artist pages, colored ASCII covers, and Now Playing.</sub>
</p>

> [!NOTE]
> MTUI is a personal, unofficial project built in public. It is not affiliated with or endorsed by YouTube, Google, or Discord. Ideas, bug reports, documentation improvements, and pull requests are welcome.

## Highlights

| Browse | Play | Personalize |
|---|---|---|
| Home, search, artists, albums, and playlists | Native AAC playback with bounded buffering | Kitty images, pixel art, or colored ASCII |
| Personalized shelves with optional sign-in | Continuous queues, prefetching, and seeking | Discord Rich Presence and four icon themes |
| Synced lyrics, comments, and related music | One current cover and bounded artwork cache | Windows notification-area controls |

```text
Home / Search
          |
          v
 Artist -> Album / Playlist -> Track
                               |
                               v
                    Queue / Lyrics / Related / Comments
```

Search and playback work without an account. Signing in adds your personalized YouTube Music Home.

Discord Rich Presence is off by default. MTUI publishes playback details only after you enable it with `D` or in Settings.

## Quick Start

### Windows

Download `mtui.exe` from the [latest release](https://github.com/lightzgls/mtui/releases/latest), then run:

```powershell
.\mtui.exe
```

The executable is self-contained. Personalized Home sign-in uses the Microsoft Edge WebView2 runtime included with current Windows installations.

### Linux

Install Rust and the native audio, TLS, and WebKitGTK development packages.

Debian or Ubuntu:

```sh
sudo apt install build-essential pkg-config libssl-dev libasound2-dev libwebkit2gtk-4.1-dev
```

Fedora:

```sh
sudo dnf install gcc pkgconf-pkg-config openssl-devel alsa-lib-devel webkit2gtk4.1-devel
```

Arch Linux:

```sh
sudo pacman -S --needed base-devel pkgconf openssl alsa-lib webkit2gtk-4.1
```

Clone the repository, then build and install MTUI with one Cargo command:

```sh
git clone https://github.com/lightzgls/mtui.git
cd mtui
cargo install --path . --bins --root ~/.local --locked --force
mtui
```

Cargo installs the player and its private sign-in helper together. You never need
to launch or install the helper separately. Ensure `~/.local/bin` is on `PATH`.

### macOS

Install the Xcode command-line tools and Rust, then use the same clone and
`cargo install` commands. WKWebView is provided by macOS.

## First Run

MTUI finds `yt-dlp` on `PATH` or downloads a private copy on first run. If `yt-dlp` cannot find a supported JavaScript runtime, MTUI reuses Deno, Node.js, or Bun from `PATH`, or installs Deno privately. MTUI also installs a pinned local copy of the GPL-3.0 `bgutil-ytdlp-pot-provider` and its production dependencies so YouTube's protected audio streams can play at normal quality. These automatic installs require network access but not administrator access.

| Experience | Account needed | How to connect |
|---|---:|---|
| Search and playback | No | Start typing with `/` or `i` |
| Personalized Home | Optional | Press `M` and sign in |

## Sign In

### Personalized Home

Press `M`. MTUI opens `music.youtube.com` in a temporary native sign-in window.
Complete Google's flow and the window closes as soon as the session is ready.
MTUI stores the resulting YouTube session cookies in its private configuration
directory; it does not receive the password entered into the page.

On Linux and macOS, the WebKit sign-in code lives in the small
`mtui-sign-in` companion installed automatically with MTUI. It runs only during
sign-in and exits immediately afterward, keeping the long-running player free
of the browser runtime. Windows keeps the same behavior inside its single
published executable.

MTUI can open the same window automatically when Home has no usable session or
YouTube rejects the saved one. A complete `Cookie` request-header value may
still be placed manually in `cookies.txt` as a compatibility fallback. Treat
that file like a password.

To log out, open **App Menu → Account & Sessions → Log out of YouTube Music**. MTUI removes its saved session, the manual-cookie fallback, and its private sign-in profile without stopping current playback.

## Controls

`Ctrl-K` opens the global App Menu. Outside search entry, `.` opens actions for the current page or selection.

The terminal UI also accepts the mouse: use the wheel to navigate, click the search box to edit, and click visible cards, rows, player tabs, or queue entries to open them.

| Key | Action |
|---|---|
| `Ctrl-K` | Open the App Menu for navigation, accounts, settings, help, tray, and quit |
| `.` | Open page and selection actions, including available artist links |
| `?` | Open keyboard help |
| Arrows or `hjkl` | Move through rows, cards, shelves, and tabs |
| `g` / `G` | Jump to the beginning or end |
| `Page Up` / `Page Down` | Move by a page |
| `Enter` | Play or open the selected item |
| `/` or `i` | Search |
| `Esc` or `H` | Back or Home |
| `P` | Open the player |
| `Space` | Pause or resume |
| `+` / `-` | Change volume |
| Left / Right | Seek five seconds while browsing tracks or the player |
| `n` / `p` | Next or previous track on the player |
| `1`-`4` or `Tab` | Open Queue, Lyrics, Related, or Comments |
| `c` | Change cover-art size |
| `M` | Import or refresh the personalized Home session |
| `D` | Toggle Discord Rich Presence directly |
| `S` or `Ctrl-S` | Open settings for tray, song cover, app icon, and Discord presence |
| `B` | Continue in the Windows notification area |
| `q` or `Ctrl-C` | Quit |

## Windows Tray

Press `B` while music is playing to close the terminal and continue in the notification area. Closing the terminal with its `X` button also moves MTUI to the tray without interrupting playback. The tray menu can show MTUI, pause or resume, move between tracks, and quit.

Enable **Keep notification-area icon** under Settings to retain the icon while the terminal UI is open. Windows may place new icons under the hidden-icons `^` menu.

## Configuration

| Platform | Configuration directory |
|---|---|
| Windows | `%APPDATA%\mtui` |
| Linux | `$XDG_CONFIG_HOME/mtui`, or `~/.config/mtui` |

Downloaded tools are kept separately under `%LOCALAPPDATA%\mtui` on Windows or `$XDG_CACHE_HOME/mtui` on Linux.

### Resource Use

Playback uses a fixed 1 MiB audio ring buffer rather than downloading a whole
song into memory. MTUI holds one current cover, and its Home/artist artwork LRU
is capped at 32 images with a raw RGB budget of about 6 MiB. Late image replies
for evicted cards are discarded. Process monitors may report additional shared
audio, TLS, and system-library pages as resident memory, but caches do not grow
with the number of songs played.

MTUI detects Kitty graphics and Sixel support automatically. The Settings panel's **Image renderer** choice can keep automatic detection, force Kitty (useful when a multiplexer hides terminal capabilities), or use universal terminal-cell pixel art. Kitty rendering applies to artwork throughout the home feed, artist pages, and player. **Song cover** separately switches the large current-song cover between bitmap/pixel rendering and colored ASCII. Artwork is center-cropped to a square sleeve. `MTUI_GRAPHICS=blocks`, `MTUI_GRAPHICS=kitty`, and `MTUI_GRAPHICS=sixel` remain available as startup overrides for automatic detection.

### Diagnostics

MTUI records startup, shutdown, crashes, and subsystem failures in `mtui.log` inside the configuration directory. The log rotates at 1 MiB and keeps one backup as `mtui.log.1`. URLs and credential-bearing messages are redacted; cookies, tokens, and song titles are not intentionally logged.

## Contributing

MTUI is a personal project, not a company-backed product, and contributions are genuinely useful.

- Found a bug or rough edge? [Open an issue](https://github.com/lightzgls/mtui/issues/new).
- Have an improvement? [Start a pull request](https://github.com/lightzgls/mtui/compare).
- Unsure whether an idea fits? Open an issue first and describe the user problem.

Please keep changes focused, explain behavior changes, and add tests when the affected code has a practical test seam.

### Development

MTUI uses Rust 2024 and requires Rust 1.85 or newer.

```sh
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
```

Network and live-account tests are ignored by default.

Windows releases use the MSVC target so WebView2's loader is linked into `mtui.exe`. GNU Windows builds remain supported, but require the generated `WebView2Loader.dll` beside the executable.

## License

Released under the [MIT License](LICENSE).
