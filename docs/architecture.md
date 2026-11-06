# MTUI Architecture and Code Organization

MTUI is organized by feature and execution boundary: the terminal UI, application orchestration, audio, and background work. A module can contain multiple files; there is no need to introduce another crate just to split a long file.

## Assessment of the Current Code

Several solid foundations are already in place:

- `ui` handles rendering with Ratatui; `app` handles actions and state. Audio belongs to `player`, while HTTP and subprocess work belong to workers in `source`.
- `Request`, `Response`, `Command`, and `PlayerEvent` represent communication between components through messages. Request IDs, queue epochs, and session generations handle late responses instead of assuming that operations finish in order.
- The queue, navigation history, and caches are bounded. These limits must be preserved when refactoring an application that prioritizes low memory usage.
- Scenario tests use inert player/source services to control events. Each orchestration test does not need to open an audio device or access an account.
- `mtui-resolver` already provides a separate crate boundary for stream resolution.

Before this reorganization, `app.rs` contained approximately 7,200 lines and `ui.rs` nearly 7,000 lines, including tests. They combined many reasons for change in the same file: changes to menus, seeking, sign-in, navigation, or lyric rendering all touched a central file. Splitting the code into modules makes the relevant parts easier to find and modify, but does not yet divide `App` into fully independent state components.

## Structure

```text
src/
  main.rs                  Startup and foreground/background loops
  app/
    mod.rs                 App, service initialization, overlays, and shared APIs
    account.rs             Sign-in, session renewal, and sign-out state and handling
    artwork.rs             Artwork state, caching, and terminal image placement plans
    browse/                Home, Library, artists, search, navigation, and track selection
    controls/              Input, menus, settings, and track actions
    desktop.rs             Tray, media controls, and Discord activity
    playback/              Current track, queue, play/seek/stop, recovery, and prefetch
      state.rs             Playback state, pending seeks, and listening
    responses.rs           Polling and applying responses from source workers
    testing/               Unit tests, scenarios with fake services, and performance probes
  ui/
    mod.rs                 Frame composition and image plans
    pages/                 Home, Search, Library, artist, album, and playlist pages
    player/                Now Playing, sidebar, queue, lyrics, comments, and related tracks
    modals/                Menus, settings, sign-in, notifications, and track dialogs
    artwork.rs             Image fitting, cropping, and pixel/ASCII rendering
    cards.rs               Shared cards and shelf geometry
    mouse.rs               Hit testing and positioning for continuous controls
    paging.rs              Pagination controls
    palette.rs             Artwork-derived colors and readable text colors
    shell.rs               Header, content area, and shared geometry
    split.rs               Sizes of regions that can be resized by dragging
    status.rs              Footer, mini player, transport, and volume
    text.rs                Terminal text measurement, truncation, wrapping, and marquees
    tests.rs               Layout/render tests with TestBackend
  player/                  Audio thread, decoder, output, and HTTP streaming
  source/
    mod.rs                 Track, BrowseEndpoint, and ArtistRef types
    catalog/               Home, artists, albums, playlists, Library, and search
    http/                  HTTP client, response body limits, and test fixtures
    listening/             Listening progress tracking, journal, and history submission
    worker/                Background work coordination and request queues
    youtube/               yt-dlp, Innertube, sign-in sessions, and now-playing data
    cover.rs               Cover image downloading and decoding
    errors.rs              Provider error messages
    lrclib.rs              Lyric lookup through LRCLIB
  session/                 Sessions, renewal, protocol, and sign-in webview
  desktop/                 Media controls, tray, Discord, and clipboard
  runtime/                 Configuration, diagnostics, process locking, and updates
  terminal/                Console, input, frame pacing, and terminal graphics support
    graphics/              Terminal capability detection, image caching, Kitty, and Sixel
  bin/mtui-sign-in.rs       Standalone sign-in executable
crates/mtui-resolver/       Stream URL resolution and validation
```

`main.rs` is the only file directly under `src/`. Each group places its module entry point in `mod.rs`, alongside the group's implementation files.

## Code Placement Rules

1. Place **behavior** under `app/` and **rendering** under `ui/`, in the corresponding feature groups. Matching filenames make both sides of a flow easier to find, for example `app/browse/library.rs` and `ui/pages/library.rs`.

2. Shared formatting, text measurement, and geometry functions belong in specifically named modules such as `ui/text.rs`, `ui/cards.rs`, and `ui/artwork.rs`. Avoid collecting everything in `utils.rs`, `helpers.rs`, or a `common` directory with no clear owner.

3. `app` dispatches background work; provider parsers and HTTP code do not belong in `ui`. Workers return data rather than directly controlling the interface.

4. Import explicitly from the owning module. Use `pub(super)` for implementation details that need to be shared with the parent module, and restrict visibility to the subsystem for details shared between groups, for example `pub(in crate::app)`. In `app/mod.rs` and `ui/mod.rs`, re-export only names needed elsewhere or already used as part of the internal API. Do not use `include!` to split files.

5. Keep tests close to the feature or in a separate test file within the module. Keep scenarios that span multiple features in `app/testing/scenarios.rs`. Update the runner's filters when test module paths change.

6. Create a separate crate when there is a genuinely independent API and dependency set, a need for reuse, or a build boundary. Line count alone is not a sufficient reason.

## SOLID in Rust

SOLID can be applied as a set of design principles. Rust expresses them through modules, composition, traits, and behavioral contracts; there is no need to build a class hierarchy.

| Principle | Application to MTUI |
| --- | --- |
| Single Responsibility | Separate navigation, menus, playback, accounts, and rendering. The next step is to reduce the amount of state each component is allowed to modify. |
| Open/Closed | Use traits when multiple implementations need to be interchangeable under the same contract. For a finite set of states or commands, enums and exhaustive matching remain appropriate. |
| Liskov Substitution | Implementations of the same trait must preserve the same contract, including error behavior, cancellation, and event ordering. The compiler checks types and signatures; tests must still verify behavior. |
| Interface Segregation | When abstraction is needed, divide it by capability, such as playback or search. Avoid a single provider trait that includes every account, cover, lyric, and audio function. |
| Dependency Inversion | Messages between App and workers already reduce coupling to background work. App still holds concrete `Player` and `SourceWorker` types; add ports or traits only when there is a clear need to replace implementations. |

Traits do not automatically make code cleaner. A trait with only one implementation and no requirement for substitution can add reading overhead without providing a benefit. Ownership does not automatically ensure a sound division of responsibilities either.

Official references: [modules and privacy](https://doc.rust-lang.org/book/ch07-02-defining-modules-to-control-scope-and-privacy.html), [traits and shared behavior](https://doc.rust-lang.org/book/ch10-02-traits.html).

## Areas for Further Improvement

- `App` still has many fields for browsing, accounts, and settings. Transport and sign-in session state already have `PlaybackState` and `SessionState`. Page loading status is derived directly from the pending navigation request. Browsing data could be grouped further so that changes are restricted to methods.
- The renderer still receives `&mut App` to update the viewport, selection, and image plans. Separating read-only view data from layout feedback would strengthen this boundary.
- `source` still contains both shared models and YouTube-specific models. If support for another provider is actually added, identify the common models and each provider's capabilities before introducing abstractions.
- `app/playback/mod.rs` is still relatively large. Recovery and queue dispatch could be split out further once ownership of their state is clear. Avoid splitting code solely by line count while continuing to pass the entire App into every function.

## Playback and Prefetch State

`App` holds two internal state components with private fields, separate from the rendering data in `NowPlaying`:

- `PlaybackState` owns the pending resolve request, deferred seek, recovery, playback errors, EOF edge handling, auto-skip budget, and listening progress. A pending seek belongs to the request for its specific track; a response with a different ID cannot consume it. Selecting a new track, issuing Stop, or cancelling resolution removes the seek along with its request.
- `PrefetchState` owns selection debouncing, in-flight probes, and warmed IDs. Queue and selection responses release only the probe with the matching ID; an old failed response does not erase the known cache status of another track.

State components determine transitions; `App` still dispatches commands, sends requests, and writes the journal. A listening checkpoint is marked as sent only after it has been successfully written to the durable outbox. Canonical metadata does not change the nonce or the number of seconds listened. The UI continues to use `App::now`, snapshots, and the existing APIs; it must not modify transport or prefetch flags directly.

The refactor preserves existing playback policies, persisted data formats, and dependencies.

## Post-Refactor Checks

```powershell
./scripts/test.ps1 -Suite Smoke
./scripts/test.ps1 -Suite Full
```

The Full suite runs all targets across the entire workspace and Clippy with `-D warnings`, uses an isolated data profile, and leaves the designated live/account/audio tests ignored.

See the [testing guide](testing.md) for the limitations of headless checks and instructions for testing audio, terminal behavior, and the tray on a real operating system.