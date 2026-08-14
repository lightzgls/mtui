# MTUI

MTUI is a keyboard-driven YouTube Music player for the terminal, written in Rust and designed to keep memory use bounded while streaming.

- Personalized YouTube Music Home with shelves, albums, playlists, and recommendations
- Search by title, YouTube URL, or video ID without signing in
- Continuous queues with prefetching, related music, synced lyrics, and comments
- Library access for liked songs and playlists through optional Google OAuth
- Cover art through sixel, with a Unicode block fallback
- Discord Rich Presence and Windows notification-area playback controls

## Install

### Windows

Download `mtui.exe` from the [latest release](https://github.com/lightzgls/mtui/releases/latest) and run:

```powershell
.\mtui.exe
```

Personalized Home sign-in uses the Microsoft Edge WebView2 runtime included with current Windows installations.

### Linux

Install Rust and the native audio and TLS development packages.

Debian or Ubuntu:

```sh
sudo apt install build-essential pkg-config libssl-dev libasound2-dev
```

Fedora:

```sh
sudo dnf install gcc pkgconf-pkg-config openssl-devel alsa-lib-devel
```

Build and install MTUI:

```sh
git clone https://github.com/lightzgls/mtui.git
cd mtui
cargo build --release
install -Dm755 target/release/mtui ~/.local/bin/mtui
mtui
```

Ensure `~/.local/bin` is on `PATH`.

## First Run

MTUI finds `yt-dlp` on `PATH` or downloads a private copy on first run. If `yt-dlp` cannot find a supported JavaScript runtime, MTUI reuses Deno, Node.js, or Bun from `PATH`, or installs Deno privately. Neither automatic install requires administrator access.

Search and playback work without an account. Personalized Home and library features use separate sign-ins because Google exposes them through different APIs.

## Sign In

### Personalized Home

On Windows, press `M` and complete sign-in in MTUI's YouTube Music window. MTUI opens the real `music.youtube.com` page, never receives form values or passwords, and stores only the resulting YouTube session cookies in its private configuration directory.

MTUI prompts for this session automatically when Home has no usable session. OAuth tokens cannot authenticate YouTube Music's private Home feed.

The embedded Home sign-in is currently Windows-only. A session can instead be supplied manually by placing the complete `Cookie` request header from a signed-in `music.youtube.com` request in `cookies.txt` inside the configuration directory. Treat this file like a password.

### Library

Press `A` to connect liked songs, playlists, and playlist editing through the YouTube Data API. This requires your own Google OAuth client:

1. Create a project in the [Google Cloud Console](https://console.cloud.google.com/) and enable **YouTube Data API v3**.
2. Configure the OAuth consent screen and add your account as a user. Publishing the app avoids Google's seven-day token expiry for apps left in testing.
3. Create an OAuth client with application type **TV and Limited Input devices**.
4. Create `client.json` in MTUI's configuration directory:

```json
{
  "client_id": "your-client-id",
  "client_secret": "your-client-secret"
}
```

Press `A` again and follow the device-code prompt. MTUI stores refreshed tokens locally and does not bundle a shared Google client.

## Controls

Keys are context-sensitive. The footer always shows the controls available in the current view.

| Key | Action |
|---|---|
| Arrows or `hjkl` | Move through rows, cards, shelves, and tabs |
| `g` / `G` | Jump to the beginning or end |
| `Page Up` / `Page Down` | Move by a page |
| `Enter` | Play or open the selected item |
| `/` or `i` | Search |
| `Esc` or `H` | Back or Home |
| `P` | Open the player |
| `L` or `Ctrl-L` | Open the library |
| `Space` | Pause or resume |
| `+` / `-` | Change volume |
| Left / Right | Seek five seconds while browsing tracks or the player |
| `n` / `p` | Next or previous track on the player |
| `1`-`4` or `Tab` | Open Up Next, Lyrics, Comments, or Related |
| `a` | Add the selected track to a playlist |
| `f` | Like or unlike the selected track |
| `c` | Change cover-art size |
| `A` | Start OAuth library sign-in |
| `M` | Start YouTube Music Home sign-in |
| `D` | Toggle Discord Rich Presence |
| `S` or `Ctrl-S` | Open settings |
| `B` | Continue in the Windows notification area |
| `q` or `Ctrl-C` | Quit |

## Windows Tray

Press `B` while music is playing to close the terminal and continue in the notification area. The tray menu can show MTUI, pause or resume, move between tracks, and quit. Enable **Keep notification-area icon** under `S` Settings to retain the icon while the terminal UI is open. Windows may place it under the hidden-icons `^` menu.

## Configuration

MTUI stores configuration in:

| Platform | Directory |
|---|---|
| Windows | `%APPDATA%\mtui` |
| Linux | `$XDG_CONFIG_HOME/mtui`, or `~/.config/mtui` |

Downloaded tools are kept separately under `%LOCALAPPDATA%\mtui` on Windows or `$XDG_CACHE_HOME/mtui` on Linux.

MTUI detects sixel support automatically. Set `MTUI_GRAPHICS=blocks` to force the block renderer or `MTUI_GRAPHICS=sixel` to force sixel output.

## Development

MTUI uses Rust 2024 and requires Rust 1.85 or newer.

```sh
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
```

Network and live-account tests are ignored by default.

Windows releases use the MSVC target so WebView2's loader is linked into `mtui.exe`. GNU Windows builds remain supported, but require the generated `WebView2Loader.dll` beside the executable.

## License

[MIT](LICENSE)
