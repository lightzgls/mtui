# MTUI

A small terminal music player for YouTube Music, written in Rust.

- Search and stream music without signing in
- Browse playlists, lyrics, comments, and recommendations
- Render cover art in sixel-capable terminals
- Optionally sync likes, playlists, history, and your YouTube Music feed
- Keep audio memory bounded with in-process AAC streaming

## Windows

Download `mtui.exe` from the [latest release](https://github.com/lightzgls/mtui/releases/latest), then run:

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
| `B` | Background tray mode (Windows only) |
| `q` | Quit |

## Notes

- MTUI downloads `yt-dlp` on first run when it is not already on `PATH`.
- If no JavaScript runtime is found, MTUI installs Deno privately without administrator access. Existing Deno, Node.js, or Bun installations are reused.
- Signing in is optional. Press `A` for setup instructions.
- Configuration is stored in `%APPDATA%\mtui` on Windows and `$XDG_CONFIG_HOME/mtui` or `~/.config/mtui` on Linux.
- Firefox cookie import works on both platforms. Chromium cookie import may fail on Windows due to App-Bound Encryption.

## Development

```sh
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
```

Licensed under the [MIT License](LICENSE).
