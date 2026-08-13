# MTUI

A small terminal music player for YouTube Music, written in Rust.

- Search and stream music without signing in
- Browse playlists, lyrics, comments, and recommendations
- Render cover art in sixel-capable terminals
- Optionally sync likes, playlists, history, and your YouTube Music feed
- Keep audio memory bounded with in-process AAC streaming

## Windows

Download the Windows release folder from the [latest release](https://github.com/lightzgls/mtui/releases/latest). Keep `mtui.exe` and `WebView2Loader.dll` together, then run:

```powershell
.\mtui.exe
```

## Linux

Install [Rust](https://rustup.rs) and the native build dependencies.

Debian/Ubuntu:

```sh
sudo apt install build-essential pkg-config libssl-dev libasound2-dev
```

Fedora:

```sh
sudo dnf install gcc pkgconf-pkg-config openssl-devel alsa-lib-devel
```

Build and install:

```sh
git clone https://github.com/lightzgls/mtui.git
cd mtui
cargo build --release
install -Dm755 target/release/mtui ~/.local/bin/mtui
mtui
```

Ensure `~/.local/bin` is on your `PATH`.

## Usage

| Key | Action |
|---|---|
| `/` or `i` | Search |
| Arrows or `hjkl` | Navigate |
| `Enter` | Play or open |
| `Space` | Pause/resume |
| `n` / `p` | Next/previous |
| Left / Right | Seek 5 seconds |
| `+` / `-` | Volume |
| `1`-`4` or `Tab` | Switch player tabs |
| `H` or `Esc` | Back/Home |
| `L` | Library |
| `A` | Sign in |
| `M` | Sign in to YouTube Music Home (Windows) |
| `S` or `Ctrl-S` | Settings |
| `B` | Background tray mode (Windows only) |
| `q` | Quit |

## Notes

- MTUI downloads `yt-dlp` on first run when it is not already on `PATH`.
- If no JavaScript runtime is found, MTUI installs Deno privately without administrator access. Existing Deno, Node.js, or Bun installations are reused.
- On Windows, press `S` and enable **Keep notification-area icon** to leave the tray icon available while the terminal UI is open. Windows may place the icon under the `^` hidden-icons menu.
- Signing in is optional. Press `A` for OAuth library access. On Windows, press `M` to sign in to personalized YouTube Music Home in MTUI's WebView2 window; Google owns the credential form and MTUI saves only the resulting YouTube session.
- Configuration is stored in `%APPDATA%\mtui` on Windows and `$XDG_CONFIG_HOME/mtui` or `~/.config/mtui` on Linux.
- Personalized Home requires a YouTube Music web session. Windows setup uses MTUI's private WebView2 profile and does not inspect installed browser profiles.

## Development

```sh
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
```

Licensed under the [MIT License](LICENSE).
