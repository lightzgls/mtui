# MTUI Product Requirements

**Status:** Active baseline\
**Version:** 0.2\
**Date:** 2026-10-06\
**Primary interface:** Terminal UI (TUI)

## 1. Purpose

MTUI is a Windows-first YouTube Music client built around a fast, mouse-capable
terminal interface. It should feel like a complete music application while
remaining substantially lighter than an Electron client.

The project is personal today and may become open source. Requirements,
architecture, and user-visible behavior must therefore stay understandable and
testable without knowledge held only by the maintainer.

## 2. Product vision

> Launch MTUI, sign in once, and get reliable, ad-free YouTube Music playback,
> personalized content, and complete player controls in a polished TUI using
> roughly 50 MiB of steady-state memory.

Priorities, in order:

1. Reliable and uninterrupted playback.
2. Low memory and CPU use.
3. Fast, forgiving startup, installation, and sign-in.
4. Clear visual hierarchy and complete keyboard and mouse navigation.
5. Feature completeness for everyday YouTube Music use.

## 3. Product principles

- The TUI is the product interface, not a diagnostic fallback.
- Every important action works with both keyboard and mouse.
- The interface uses only components that help the listening task.
- The current album artwork supplies the accent color; there are no gradients.
- The user avatar and account status belong in an account menu, not a permanent
  side tab.
- Playback continues independently of page navigation and temporary failures.
- Queues, artwork caches, logs, retries, and background work remain bounded.
- YouTube Music is the only music source.

## 4. Scope

### 4.1 Must-have capabilities

- Anonymous search and playback.
- Easy Google/YouTube Music sign-in and sign-out.
- Personalized Home, recommendations, library, likes, and history when the
  account endpoints support them.
- Song, album, playlist, artist, and radio browsing.
- Reliable playback, seek, volume, repeat, shuffle, previous, next, and output
  selection.
- Queue inspection, play next, add, remove, reorder, clear, and continuation.
- A dedicated Now Playing view with artwork, synchronized lyrics, queue,
  comments, and related music.
- Artwork display through supported terminal image protocols with a colored
  text fallback.
- Discord Rich Presence, disabled by default.
- Windows tray controls and media-key integration.
- Installer and portable package.
- Keyboard and mouse support for every primary journey.

### 4.2 Later capabilities

- Like/dislike and playlist mutations where the account API is reliable.
- Crossfade, gapless playback, equalizer, compressor, playback speed, and skip
  silence as bounded optional playback features.
- Native notifications, scrobbling, SponsorBlock, global shortcuts, and a local
  authenticated control API.
- Lyric offset, provider selection, translation, and romanization.
- Downloads only after a separate product and legal decision.

### 4.3 Non-goals

- A permanent browser or WebView-based music interface.
- Video playback in the initial public-quality release.
- Supporting music services other than YouTube Music.
- Copying Spotify, YouTube Music, or Pear visual assets and branding.
- Adding decorative panels, metrics, or controls without a real user task.

## 5. Users and environment

The initial user is the maintainer on Windows 10 or 11 x64. Future users are
people who want a small unofficial YouTube Music client and are comfortable
with a terminal-style interface. Search and playback work without an account;
personalized and account-bound features require a Google account.

The installed app may open its own console window from the Start menu. The user
does not need to type commands, edit cookie files, or install developer tools.

## 6. Core journeys

### 6.1 First launch and sign-in

1. The user runs the installer, portable executable, or development binary.
2. MTUI opens directly into a usable TUI.
3. The user may search immediately or open the account menu and sign in.
4. Sign-in opens the real Google page in a short-lived helper window.
5. Runtime preparation and failures appear as clear progress or repair dialogs
   inside the TUI.
6. The personalized Home refreshes after a usable session is captured.

### 6.2 Everyday listening

1. MTUI paints the shell immediately and restores the previous safe state.
2. The user opens Home, Search, or Library using visible navigation or keys.
3. A click or Enter starts playback; the persistent player shows progress.
4. The user can drag or click the progress bar, change volume, and open the
   dedicated Now Playing view.
5. The queue prefetches and playback continues during navigation.

### 6.3 Playback recovery

1. MTUI classifies the resolver, authentication, format, network, or region
   failure without freezing the interface.
2. It refreshes an expired URL, retries from the current position, tries the
   next supported resolver or format, and renews the session when useful.
3. It resumes automatically or shows an actionable error with Retry and Skip.

### 6.4 Another computer

1. The user installs or unpacks MTUI on the second computer.
2. The same sign-in flow connects that machine; cookies are not copied.
3. YouTube Music supplies account-bound Home, library, likes, and history.
4. Non-secret settings may be exported and imported separately.

## 7. Required UI surfaces

| Surface | Required content and actions |
|---|---|
| App shell | Back, forward, current location, search entry, account status/menu, help/menu entry |
| Home | Personalized or public shelves, refresh, loading, partial, offline, and empty states |
| Search | Search entry, suggestions, grouped songs/albums/artists/playlists, filters, continuation |
| Library | Playlists, albums, songs, artists, likes, and history with sort/filter where available |
| Artist | Identity, top songs, albums, singles, related artists, play/shuffle/radio |
| Album | Artwork, metadata, track list, play/shuffle, queue and library actions |
| Playlist | Artwork, owner/metadata, tracks, play/shuffle, queue and supported edit actions |
| Now Playing | Large artwork, title, artist/album, transport, time, seek bar, volume, repeat/shuffle |
| Queue tab | Playing item, upcoming items, continuation, reorder, remove, clear, save when supported |
| Lyrics tab | Synchronized/plain lyrics, current-line follow, manual scroll, return to current line |
| Comments tab | Comment list, replies where supported, loading, unavailable, and error states |
| Related tab | Related songs, radios, albums, playlists, and direct queue actions |
| Account | Sign in/out, connection state, refresh/repair, account identity; never a side tab |
| Settings | Playback, appearance, account, integrations, storage, and advanced settings |
| Diagnostics | Component health, sanitized report, runtime repair, cache sizes and clear actions |
| Dialogs | First run, confirmation, output picker, errors, progress, keyboard help, context actions |

The shell stays minimal. Secondary actions live in the app menu or a contextual
action menu instead of occupying permanent navigation space.

## 8. Functional requirements

### 8.1 Account and session

| ID | Requirement |
|---|---|
| FR-AUTH-01 | Offer sign-in from first run and the account menu using Google's real page in a temporary helper. |
| FR-AUTH-02 | Close the helper immediately after a usable session is captured. |
| FR-AUTH-03 | Refresh sessions silently when possible and request attention only when required. |
| FR-AUTH-04 | Store session material using user-scoped protection rather than plain text. |
| FR-AUTH-05 | Show signed-out, connecting, connected, refreshing, expired, offline, and error states. |
| FR-AUTH-06 | Remove local session data and the private browser profile on complete logout. |
| FR-AUTH-07 | Permit anonymous search and playback where technically available. |

### 8.2 Browse, search, and library

| ID | Requirement |
|---|---|
| FR-BRW-01 | Load public or personalized Home shelves without blocking input. |
| FR-BRW-02 | Search songs, videos when unavoidable as audio, albums, artists, and playlists. |
| FR-BRW-03 | Open album, playlist, artist, and radio pages with continuation support. |
| FR-BRW-04 | Provide a coherent Library for playlists, albums, songs, artists, likes, and history. |
| FR-BRW-05 | Preserve page selection and scroll position when navigating back and forward. |
| FR-BRW-06 | Expose play, play next, queue, radio, open, and supported library actions consistently. |

### 8.3 Playback and queue

| ID | Requirement |
|---|---|
| FR-PLY-01 | Use native audio-only playback without a long-lived browser process. |
| FR-PLY-02 | Support play/pause, previous, next, seek, volume, mute, repeat, and shuffle. |
| FR-PLY-03 | Keep playing while navigating or while the terminal is minimized. |
| FR-PLY-04 | Prefetch the next item and keep buffering independent of track length. |
| FR-PLY-05 | Refresh expired URLs and fall back across supported resolvers and formats. |
| FR-PLY-06 | Let the user choose an available output device. |
| FR-QUE-01 | Show previous, current, and upcoming queue items with source context. |
| FR-QUE-02 | Add, play next, remove, reorder, clear, and extend the queue. |
| FR-QUE-03 | Preserve a structurally valid queue across a crash or restart. |

### 8.4 Player content

| ID | Requirement |
|---|---|
| FR-CNT-01 | Show current artwork, title, artist, album, duration, and state. |
| FR-CNT-02 | Show synchronized lyrics and plain lyrics as fallback. |
| FR-CNT-03 | Display related music with open, play-next, and queue actions. |
| FR-CNT-04 | Display comments and replies supported by the endpoint. |
| FR-CNT-05 | Derive one readable accent color from artwork and retain a safe fallback palette. |

### 8.5 TUI interaction

| ID | Requirement |
|---|---|
| FR-UI-01 | Render correctly at the documented minimum terminal size and adapt at larger sizes. |
| FR-UI-02 | Give every primary action a visible label, menu item, or contextual hint. |
| FR-UI-03 | Support complete keyboard navigation with a visible focus/selection state. |
| FR-UI-04 | Support click, double-click where useful, wheel scrolling, progress seeking, volume adjustment, tabs, lists, and menus with the mouse. |
| FR-UI-05 | Use a dedicated Now Playing layout and a persistent, mouse-seekable progress bar. |
| FR-UI-06 | Avoid gradients and unnecessary permanent navigation components. |
| FR-UI-07 | Keep wide glyphs, narrow terminals, long titles, and missing metadata within their assigned regions. |
| FR-UI-08 | Show loading, empty, offline, partial, and error states at the affected surface. |
| FR-UI-09 | Retain readable monochrome behavior when terminal color or image support is limited. |

### 8.6 Windows, settings, and diagnostics

| ID | Requirement |
|---|---|
| FR-WIN-01 | Provide tray show, play/pause, previous, next, and quit actions. |
| FR-WIN-02 | Support media keys and publish current metadata where feasible. |
| FR-WIN-03 | Provide optional Discord Rich Presence, disabled by default. |
| FR-SET-01 | Persist versioned settings and migrate them automatically. |
| FR-SET-02 | Export/import non-secret settings without session credentials. |
| FR-SET-03 | Show cache sizes and clear artwork, metadata, and runtime caches independently. |
| FR-SET-04 | Keep rotating, redacted diagnostics and copy a sanitized health report. |

## 9. Non-functional requirements

### 9.1 Reliability

- At least 98% of a maintained 100-track set starts on the first attempt and
  99% after automatic fallback, excluding confirmed account/region limits.
- No operation can remain in an indefinite loading state; it has timeout,
  cancellation, or bounded retry behavior.
- Queue and pending-history data remain structurally valid after interruption.

### 9.2 Performance

- Ordinary playback targets approximately **50 MiB** private working set and
  must remain within a **40–60 MiB** band after caches settle.
- After an eight-hour soak, working set grows by no more than 8 MiB and remains
  below 60 MiB.
- Steady playback averages below 2% of one representative four-core system.
- A warm launch paints the TUI within one second and accepts input within two.
- Artwork and audio buffers have separate hard budgets.
- Sign-in and resolver helpers are measured separately and exit after use.

### 9.3 Maintainability and testability

- Rendering performs no blocking network, resolver, disk, or decoder work.
- User intent is represented by commands shared by keyboard, mouse, tray, and
  media-key input.
- YouTube response parsing uses typed adapters and saved-fixture tests.
- Every major surface has Ratatui `TestBackend` coverage at narrow, normal, and
  wide terminal sizes.
- Hit targets are derived from the same layout used for rendering.
- No feature creates a second playback engine.

## 10. Release acceptance

The next public-quality release is acceptable when:

1. Must-have surfaces and states exist in the TUI.
2. Playback compatibility and the eight-hour resource soak meet their targets.
3. A clean Windows user can install, sign in, play, quit, relaunch, and
   uninstall without editing a file or entering a shell command.
4. Keyboard-only and mouse-only walkthroughs complete all core journeys.
5. Layout tests pass at documented terminal sizes with long and wide-character
   content.
6. No GUI-framework, Slint, Figma-export, or browser-shell dependency remains
   in the production path.

## 11. Open decisions

### Player and browse refinements — 2026-10-08

- Search must include songs, artists, albums, playlists, and music videos, with category filters and the correct play or browse action.
- Home must load further sections from YouTube Music, preserving the service's headings and order.
- Artist and album names in Now Playing must open their canonical Music pages when available.
- The current cover must color the whole application: background, surfaces, navigation, playback bar, and lyric emphasis. Use solid colors, no gradients, and retain readable contrast.
- Lyrics should have comfortable spacing and a clear current line, with manual scrolling and automatic following.
- Investigate optional window transparency with readable text and sharp artwork. Terminal support must determine availability.
- The persistent playback bar must expose Like, Share, Save to playlist, Shuffle, Repeat, and Output device alongside transport, progress, and volume. Mouse and keyboard access are required. Likes and playlist saves must reflect confirmed server state; decorative or nonfunctional buttons do not satisfy this requirement.
- Intermittent HTTP 403 responses must trigger bounded automatic recovery without requiring a page refresh or restarting a track that is already playing.

1. The exact minimum terminal dimensions for the full player layout.
2. Whether downloads belong in the product.
3. Whether multi-account switching is needed for the first public release.
4. Which Windows media integration can remain small enough for the memory goal.
