<a id="readme-top"></a>

<!--
*** MTUI README — structured after othneildrew/Best-README-Template.
*** Reference-style links are collected at the bottom of this file.
-->

<!-- PROJECT LOGO -->
<br />
<div align="center">
  <a href="https://github.com/lightzgls/mtui">
    <img src="assets/mtui-icon.svg" alt="MTUI icon" width="132" height="132">
  </a>

  <h3 align="center">MTUI</h3>

  <p align="center">
    A small, native YouTube Music player for your terminal.
    <br />
    Fast navigation, native audio playback, bounded caches, and an optional
    browser window used only while signing in.
    <br />
    <br />
    <a href="https://github.com/lightzgls/mtui/releases/latest">View Demo</a>
    &middot;
    <a href="https://github.com/lightzgls/mtui/issues/new?labels=bug">Report Bug</a>
    &middot;
    <a href="https://github.com/lightzgls/mtui/issues/new?labels=enhancement">Request Feature</a>
  </p>
</div>
<img width="1901" height="1025" alt="image" src="https://github.com/user-attachments/assets/fdbfd164-ec9a-4e50-a0da-1b33ae060bd8" />
<img width="1906" height="1023" alt="image" src="https://github.com/user-attachments/assets/97d485f4-91df-4344-b6cf-2028758b75f6" />
<img width="1893" height="1020" alt="image" src="https://github.com/user-attachments/assets/c1f943fb-cae9-460c-afb8-241e93f9425f" />

> [!NOTE]
> MTUI is a personal, unofficial project built in public. It is not affiliated
> with or endorsed by YouTube, Google, or Discord. Ideas, bug reports,
> documentation improvements, and pull requests are welcome.

<!-- TABLE OF CONTENTS -->
<details>
  <summary>Table of Contents</summary>
  <ol>
    <li>
      <a href="#about-the-project">About The Project</a>
      <ul>
        <li><a href="#highlights">Highlights</a></li>
        <li><a href="#built-with">Built With</a></li>
      </ul>
    </li>
    <li>
      <a href="#getting-started">Getting Started</a>
      <ul>
        <li><a href="#prerequisites">Prerequisites</a></li>
        <li><a href="#installation">Installation</a></li>
      </ul>
    </li>
    <li>
      <a href="#usage">Usage</a>
      <ul>
        <li><a href="#first-run">First Run</a></li>
        <li><a href="#sign-in">Sign In</a></li>
        <li><a href="#controls">Controls</a></li>
        <li><a href="#windows-tray">Windows Tray</a></li>
        <li><a href="#configuration">Configuration</a></li>
      </ul>
    </li>
    <li><a href="#roadmap">Roadmap</a></li>
    <li><a href="#contributing">Contributing</a></li>
    <li><a href="#license">License</a></li>
    <li><a href="#contact">Contact</a></li>
    <li><a href="#acknowledgments">Acknowledgments</a></li>
  </ol>
</details>

<!-- ABOUT THE PROJECT -->
## About The Project

MTUI is a terminal music player for YouTube Music, built to stay small in memory.
It offers fast navigation, native AAC playback, bounded caches, colored ASCII or
image covers, and an optional native browser window that is used only while you
sign in.

Search and playback work without an account. Signing in adds your personalized
YouTube Music Home.

Discord Rich Presence is off by default. MTUI publishes playback details only
after you enable it with `D` or in Settings.

### Highlights

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

### Built With

- [Rust][rust-url] (2024 edition)
- [ratatui][ratatui-url] and [crossterm][crossterm-url] for the terminal UI
- [Tokio][tokio-url] for the streaming runtime
- [rodio][rodio-url] for native AAC playback
- [reqwest][reqwest-url] for HTTP
- [wry / tao][wry-url] for the sign-in window

<p align="right">(<a href="#readme-top">back to top</a>)</p>

<!-- GETTING STARTED -->
## Getting Started

### Prerequisites

**Windows** — nothing to install. The executable is self-contained, and
personalized Home sign-in uses the Microsoft Edge WebView2 runtime included with
current Windows installations.

**Linux** — install Rust and the native audio, TLS, and WebKitGTK development
packages.

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

**macOS** — install the Xcode command-line tools and Rust. WKWebView is provided
by macOS.

### Installation

**Windows** — download `mtui.exe` from the
[latest release](https://github.com/lightzgls/mtui/releases/latest), then run:

```powershell
.\mtui.exe
```

**Linux and macOS** — clone the repository, then build and install MTUI with one
Cargo command:

```sh
git clone https://github.com/lightzgls/mtui.git
cd mtui
cargo install --path . --bins --root ~/.local --locked --force
mtui
```

Cargo installs the player and its private sign-in helper together. You never need
to launch or install the helper separately. Ensure `~/.local/bin` is on `PATH`.

On first run, MTUI finds `yt-dlp` on `PATH` or downloads a private copy. If
`yt-dlp` cannot find a supported JavaScript runtime, MTUI reuses Deno, Node.js, or
Bun from `PATH`, or installs Deno privately. MTUI also installs a pinned local
copy of the GPL-3.0 `bgutil-ytdlp-pot-provider` and its production dependencies so
YouTube's protected audio streams can play at normal quality. These automatic
installs require network access but not administrator access.

<p align="right">(<a href="#readme-top">back to top</a>)</p>

<!-- USAGE -->
## Usage

`Ctrl-K` opens the global App Menu. Outside search entry, `.` opens actions for
the current page or selection. The terminal UI also accepts the mouse: use the
wheel to navigate, click the search box to edit, and click visible cards, rows,
player tabs, or queue entries to open them.

### First Run

| Experience | Account needed | How to connect |
|---|---:|---|
| Search and playback | No | Start typing with `/` or `i` |
| Personalized Home | Optional | Press `M` and sign in |

### Sign In

Press `M`. MTUI opens `music.youtube.com` in a temporary native sign-in window.
Complete Google's flow and the window closes as soon as the session is ready.
MTUI stores the resulting YouTube session cookies in its private configuration
directory; it does not receive the password entered into the page.

On Linux and macOS, the WebKit sign-in code lives in the small `mtui-sign-in`
companion installed automatically with MTUI. It runs only during sign-in and
exits immediately afterward, keeping the long-running player free of the browser
runtime. Windows keeps the same behavior inside its single published executable.

MTUI can open the same window automatically when Home has no usable session or
YouTube rejects the saved one. A complete `Cookie` request-header value may still
be placed manually in `cookies.txt` as a compatibility fallback. Treat that file
like a password.

To log out, open **App Menu → Account & Sessions → Log out of YouTube Music**.
MTUI removes its saved session, the manual-cookie fallback, and its private
sign-in profile without stopping current playback.

### Controls

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

### Windows Tray

Press `B` while music is playing to close the terminal and continue in the
notification area. Closing the terminal with its `X` button also moves MTUI to the
tray without interrupting playback. The tray menu can show MTUI, pause or resume,
move between tracks, and quit.

Enable **Keep notification-area icon** under Settings to retain the icon while the
terminal UI is open. Windows may place new icons under the hidden-icons `^` menu.

### Configuration

| Platform | Configuration directory |
|---|---|
| Windows | `%APPDATA%\mtui` |
| Linux | `$XDG_CONFIG_HOME/mtui`, or `~/.config/mtui` |

Downloaded tools are kept separately under `%LOCALAPPDATA%\mtui` on Windows or
`$XDG_CACHE_HOME/mtui` on Linux.

**Resource use.** Playback uses a fixed 1 MiB audio ring buffer rather than
downloading a whole song into memory. MTUI holds one current cover, and its
Home/artist artwork LRU is capped at 32 images with a raw RGB budget of about
6 MiB. Late image replies for evicted cards are discarded. Process monitors may
report additional shared audio, TLS, and system-library pages as resident memory,
but caches do not grow with the number of songs played.

**Graphics.** MTUI detects Kitty graphics and Sixel support automatically. The
Settings panel's **Image renderer** choice can keep automatic detection, force
Kitty (useful when a multiplexer hides terminal capabilities), or use universal
terminal-cell pixel art. **Song cover** separately switches the large current-song
cover between bitmap/pixel rendering and colored ASCII. Artwork is center-cropped
to a square sleeve. `MTUI_GRAPHICS=blocks`, `MTUI_GRAPHICS=kitty`, and
`MTUI_GRAPHICS=sixel` remain available as startup overrides for automatic
detection.

**Diagnostics.** MTUI records startup, shutdown, crashes, and subsystem failures
in `mtui.log` inside the configuration directory. The log rotates at 1 MiB and
keeps one backup as `mtui.log.1`. URLs and credential-bearing messages are
redacted; cookies, tokens, and song titles are not intentionally logged.

<p align="right">(<a href="#readme-top">back to top</a>)</p>

<!-- ROADMAP -->
## Roadmap

- [x] Native AAC playback with bounded buffering
- [x] Personalized YouTube Music Home with optional sign-in
- [x] Synced lyrics, comments, and related music
- [x] Discord Rich Presence and four icon themes
- [x] Windows notification-area controls
- [x] Unified YouTube Music sign-in on every desktop
- [ ] Demo video

See the [open issues](https://github.com/lightzgls/mtui/issues) for a full list of
proposed features and known issues. Unsure whether an idea fits? Open an issue
first and describe the user problem.

<p align="right">(<a href="#readme-top">back to top</a>)</p>

<!-- CONTRIBUTING -->
## Contributing

MTUI is a personal project, not a company-backed product, and contributions are
genuinely useful.

- Found a bug or rough edge? [Open an issue](https://github.com/lightzgls/mtui/issues/new).
- Have an improvement? [Start a pull request](https://github.com/lightzgls/mtui/compare).
- Unsure whether an idea fits? Open an issue first and describe the user problem.

The standard flow:

1. Fork the project
2. Create your feature branch (`git checkout -b feature/amazing-feature`)
3. Commit your changes (`git commit -m 'Add some amazing feature'`)
4. Push to the branch (`git push origin feature/amazing-feature`)
5. Open a pull request

Please keep changes focused, explain behavior changes, and add tests when the
affected code has a practical test seam.

### Development

MTUI uses Rust 2024 and requires Rust 1.85 or newer.

```sh
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
```

Network and live-account tests are ignored by default.

Windows releases use the MSVC target so WebView2's loader is linked into
`mtui.exe`. GNU Windows builds remain supported, but require the generated
`WebView2Loader.dll` beside the executable.



<p align="right">(<a href="#readme-top">back to top</a>)</p>

<!-- LICENSE -->
## License

Distributed under the MIT License. See [`LICENSE`](LICENSE) for more information.

<p align="right">(<a href="#readme-top">back to top</a>)</p>

<!-- CONTACT -->
## Contact

Lightzgls — [@lightzgls](https://github.com/lightzgls)

Project Link: [https://github.com/lightzgls/mtui](https://github.com/lightzgls/mtui)

<p align="right">(<a href="#readme-top">back to top</a>)</p>

<!-- ACKNOWLEDGMENTS -->
## Acknowledgments

- [Best-README-Template](https://github.com/othneildrew/Best-README-Template)
- [yt-dlp](https://github.com/yt-dlp/yt-dlp)
- [bgutil-ytdlp-pot-provider](https://github.com/Brainicism/bgutil-ytdlp-pot-provider)
- [ratatui](https://ratatui.rs)
- [rodio](https://github.com/RustAudio/rodio)
- [contrib.rocks](https://contrib.rocks)

<p align="right">(<a href="#readme-top">back to top</a>)</p>

<!-- MARKDOWN LINKS & IMAGES -->
[rust-url]: https://www.rust-lang.org
[ratatui-url]: https://ratatui.rs
[crossterm-url]: https://github.com/crossterm-rs/crossterm
[tokio-url]: https://tokio.rs
[rodio-url]: https://github.com/RustAudio/rodio
[reqwest-url]: https://github.com/seanmonstar/reqwest
[wry-url]: https://github.com/tauri-apps/wry
