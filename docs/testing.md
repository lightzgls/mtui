# Testing MTUI

The goal is to find bugs through **sequences of actions and event ordering**, alongside testing individual features. The number of passing tests is not proof that all bugs have been found.

Automated regression tests live in the source tree. Store the results of each run in `target/test-runs/` or the corresponding issue.

## Running the Test Suites

Run these commands from the project directory using PowerShell 5.1 or PowerShell 7:

```powershell
# User flows, seek/recovery, sessions, and UI components
./scripts/test.ps1 -Suite Smoke

# The entire workspace, all targets, followed by Clippy
./scripts/test.ps1 -Suite Full

# Run all tests repeatedly to find timing/thread-dependent bugs
./scripts/test.ps1 -Suite Stress -Iterations 10 -Threads 8

# Three AAC probes on localhost: decoder, recovery/Stop, and stalled seek; volume 0
./scripts/test.ps1 -Suite Audio -AudioFixture ./target/playback-regression.m4a
```

By default, the runner uses dependencies already available on the machine and `Cargo.lock`. If dependencies have not yet been downloaded on a new machine, add `-Online`. This flag allows Cargo to download dependencies; **it does not enable tests marked `#[ignore]`**. CI runs Full on Windows, Linux, and macOS.

The Audio suite runs exactly three preselected local AAC tests. It requires an existing fixture file and does not download audio. The recovery probe opens an output device with the volume set to 0.

Each run creates `target/test-runs/<timestamp>-<id>/`, containing:

- `summary.json`: the commit, whether the working tree has changes, operating system, timestamps, thread count, individual check steps, exit codes, and test counts.
- `inventory.log`, `ignored-inventory.log`: the names of available tests and tests not run.
- `tests-*.log` / `smoke-*.log`, `clippy.log`: evidence from each step.
- A separate configuration profile, cache, and temporary directory for the run. Process environment variables are restored afterward; the runner does not delete user data.

The exit code is nonzero if any step fails. Inventory steps report zero passing tests because they only list tests. The Stress pass count represents **test executions**, not distinct tests.

A separate profile does not isolate hardware, `PATH`, or process networking.

If a sandbox blocks loopback HTTP or temporary directories, record this as an environment failure and rerun in an environment that allows localhost access. Do not skip tests just to turn a failing result into a passing one. `--offline` applies only to Cargo; loopback tests still use sockets.

## Testing Graphics on Windows

Test Windows graphics using an existing build:

```powershell
./scripts/test-graphics.ps1 -Exe ./target/release/mtui.exe -ExpectSixel
```

The probe opens and closes temporary consoles, checking both the Windows Terminal code path and the native fallback. It does not create an audio device, access an account, or replace the currently running player.

`-ExpectSixel` requires Windows Terminal 1.22+ to be installed; omit this flag when testing only the fallback. The Full suite also checks focus records arriving before capability replies and bitmap thumbnails on Home/search. These tests do not replace verification of actual rendering.

## Testing Linux Installation and Updates

Run ShellCheck and the installer test harness in an environment with Bash:

```sh
shellcheck scripts/install-linux.sh scripts/test-linux-installer.sh scripts/test-linux-release.sh scripts/package-linux.sh
bash scripts/test-linux-installer.sh
```

The test harness covers APT/DNF/pacman/Zypper, Debian before and after the time64 transition, the latest and pinned releases, checksum mismatches, download failures, declined dependency installation, files conflicting with another installation, unsupported distributions, ARM/musl, and the `curl | bash` flow.

Package managers are mocked: no packages are installed and no host data is changed.

Newer ShellCheck versions may report SC2317 for mocks invoked indirectly through command arrays. Suppress the warning only for the specific mock, rather than disabling checks for the entire script.

Test actual releases **only inside disposable containers**. Do not run `scripts/test-linux-release.sh` directly on a desktop machine. The script installs the runtime and the app into HOME inside the container. The `dist` directory must contain the archive, `install.sh`, and `SHA256SUMS` from the same version. For example:

```sh
docker run --rm \
  --mount "type=bind,source=$PWD/scripts,target=/source/scripts,readonly" \
  --mount "type=bind,source=$PWD/dist,target=/packages,readonly" \
  ubuntu:24.04 bash /source/scripts/test-linux-release.sh 0.11.4 /packages
```

The release matrix includes Ubuntu 24.04, Fedora 44, Arch, and openSUSE Tumbleweed. Checks cover real dependencies, the loaders for both executables, the launcher/icon, the sign-in helper with invalid arguments, operation without Rust or a compiler, and the real updater's replacement path using a `/bin/true` fixture. The fixture does not open Google or play audio.

Also test as a regular user when changing permissions or paths. If the required libraries are already installed, the installer must not require root or invoke a package manager. On Arch, missing libraries must be installed using `pacman -Syu`; do not create a partial upgrade with `pacman -Sy`.

Updater regression tests must cover archives with missing or duplicate helpers, invalid symlinks or paths, invalid bodies or checksums, incompatible ELF files, unwritable directories, rollback of both executables, and preservation of settings, sessions, and caches. Desktop tests must confirm that the terminal still receives input after a restart. Root-managed packages must instruct users to use the package manager rather than modifying `/usr/bin` themselves. The original Linux v0.11.3 build requires a one-time manual installation; the refreshed build can update to v0.11.4 from within the app.

CI runs separate script checks, Full/Clippy on three operating systems, and Linux packaging checks. Releases are published only after the Windows/Linux builds and four distribution smoke jobs pass. Check Windows builds with `dumpbin`: they must not depend on `WebView2Loader.dll`, `VCRUNTIME*.dll`, or `MSVCP*.dll`; sign-in still requires the OS WebView2 runtime. Verify that a new `.deb` can upgrade the previous version, and check the checksums of all assets. See the [release process](releasing.md).

Passing headless tests **does not confirm** GUI-based Google sign-in, audio hardware, or tray behavior. Run account-write/live tests only with test accounts and test playlists.

## Policy for Selecting a Track and Immediately Seeking to the End

When the duration is known, retain **the latest seek position for the currently selected track** while its URL is being resolved. Apply it after Play. Selecting another track, issuing Stop, or failed resolution must clear that seek. If the duration is unknown, do not invent an end position.

Seeking does not change the paused state. When the current track reaches its end:

| Queue / repeat | Required outcome |
| --- | --- |
| Repeat Off, tracks remaining | Advance to exactly one next track |
| Repeat Off, queue exhausted | Stop; do not replay the previous track |
| Repeat All, end of queue | Return to the start of the queue exactly once |
| Repeat One | Replay the same track exactly once |
| Waiting for continuation | Wait for a bounded time; the response must match the queue epoch |
| Already stopped / another track selected | A stale response must not restart the previous track |
| Paused | Remain paused; continue only after a resume command |

Also test cases where the decoder finishes before the UI observes a Playing frame. An error-free Buffering → Idle transition can also represent track completion.

A Buffering → Idle transition with an error must be reported as a failure, rather than interpreted as normal completion.

## Test Layers

1. **State and commands:** `src/app/testing/scenarios.rs` uses fake player/source services to control snapshots, responses, and events in a deterministic order. It does not create an audio device, network worker, WebView, or Discord worker. Tests exercise the real App's click/key handling and response-application paths; they do not verify actual audio output.

2. **Components and loopback HTTP:** existing tests cover decoder packets, range/retry behavior, body limits, parsers, sessions, journals, configuration, and Ratatui. HTTP fixtures point only to localhost; account-write operations use mocked callbacks.

3. **Audio/Windows integration:** real AAC, real devices, consoles/tray, sleep/wake, resizing while dragging, and listening to compare playback against the clock and lyrics. Record actions, devices, terminal, timing, and results separately.

4. **Live provider/account testing:** verify real content, search, and account behavior when possible. Use test accounts and test playlists for cases that write data. History tests may write to the currently signed-in account, so do not run the entire ignored test list with one command.

Ignored tests remain unchanged, except for two UI/action regression tests converted to fully offline fixtures. Preview, live API, real audio, and history-write tests appearing in the inventory **does not mean they have passed**.

The seek regression uses a localhost AAC stream for which access is denied after 64 KiB. While a seek to the middle of the track is waiting for bytes, test selecting another track, seeking again, Stop, and Quit. The same player must be able to play the next track; Quit must complete within a bounded time.

## Data and Environments Required for Integration Tests

| Category | Data / variants |
| --- | --- |
| Tracks | Normal AAC, 1-second tracks, tracks longer than 1 hour, live streams, missing/zero/incorrect duration; deleted, private, or region-restricted tracks |
| Queue | Empty, 1 track, 2 tracks, 86 tracks, pages of 60 tracks, duplicate IDs, no new tracks remaining; large playlists |
| Responses | Immediate, delayed by 1/5/30 seconds, A before B, B before A, duplicates, failure followed by success, responses after Stop/logout |
| Seek | 0%, 1%, 50%, 99.9%, 100%, positions outside the progress bar; the latest seek wins |
| UI | 0×0/1×1 to verify no panic; 24×8/48×18/64×20 to test shrinking; 100×36, 160×42, 216×50 |
| Text | Vietnamese diacritics, combining marks, CJK, emoji with ZWJ/skin tones/flags, RTL, long titles, missing metadata |
| Network | 401/403/404/416/429/500/503, DNS/TLS failures, offline, truncated bodies, ignored Range headers, incorrect ranges, oversized compressed bodies |
| Storage | Old/corrupt/truncated JSON, incomplete final journal lines, no write permission, full disks, interrupted saves |
| OS | Windows 10/11, Ubuntu 24.04, Fedora 44, Arch, openSUSE Tumbleweed, and macOS; different terminal/display sessions; desktop audio output plus headphones/Bluetooth, or no audio output |
| Installation | Windows installer/portable; Linux curl, writable archive installations, root-managed `.deb`; regular user/root; runtime present/missing; HOME containing spaces; immutable hosts; supported x64/glibc and unsupported ARM/musl |
| Session | Guest, signed in, expired, logout during a request, retry cooldown, clock moved backward/forward |

The small dimensions above are robustness cases. Product requirements have not yet defined a minimum terminal size for every operation. Do not automatically classify missing buttons at 1×1 as a bug.

## Test Matrix and Improving Bug Detection

For selecting a track and immediately seeking, combine these dimensions: state before the click (Idle/Buffering/Playing), duration (0/1 second/240 seconds/unknown), repeat (Off/All/One), queue position (first/last), response timing (before/after the click), and the next action (none/Next/Stop/seek again/Pause). Prioritize P0/P1 combinations according to risk:

- URL still pending + click 100% + snapshot still Idle.
- URL still pending + click 100% + click 0%/50% before the response.
- Click 100% + Next/Stop + stale URL/recovery/lyrics responses arrive afterward.
- Click 100% + Buffering → Idle before a Playing frame is observed.
- EOF + Repeat All/One + twenty polls of the same Idle state.
- Clear all upcoming tracks + continuation request already sent but not yet returned.
- Output changes during seek/recovery + network loss + correct preservation of pause state and track position.

The new automated tests comprise 19 functions, with loops over multiple states, durations, pointer positions, repeat modes, and queue positions, plus **16,384 queue operations** across four fixed seeds. Queue operations check the current track ID, history, duplicates, and memory limits after every action. Shuffle order varies by runtime; assertions check invariants rather than requiring a specific shuffle order.

Combinations requiring audio, a Windows machine, or a real provider still need integration testing. A passing event simulation must not be taken as proof that the corresponding audio flow passes.

## Criteria for Completing a Test Cycle

- Full and Clippy pass; Stress passes with the iteration and thread counts recorded.
- For installer/updater changes: ShellCheck, installer simulation tests, release smoke tests on four distributions, and checksum checks pass. Record desktop cases that have not been run separately. Check the Settings labels: tray is Windows-only, Updates applies to writable Windows/Linux installations, and the remaining options apply to both platforms.
- Every P0/P1 case has a Pass/Fail/Blocked/Not run result, along with evidence and environment details.
- Reproducible bugs have regression tests before fixes are applied; retain results from before and after the fix.
- Both keyboard-only and mouse-only operation complete launch → search → play → seek → queue → background → restore → quit on a real Windows system.
- Record the 100-track dataset and the 8-hour soak test separately, including start success rates on the first attempt and after fallback, confirmed account/region restrictions, steady-state memory, and memory growth after the soak.
- Do not record Pass for nonexistent buttons or unimplemented features; check actual behavior before drawing conclusions.

## Result / Bug Report Template

```text
Case ID:
Build/commit + working tree:
OS / terminal / size / output:
Guest or test account:
Data / duration / repeat / queue:
Steps and timing (ms):
Expected:
Actual:
Pass / Fail / Blocked / Not run:
Reproduction rate (x/n), seed, response order:
Logs / screenshots / recordings with session information removed:
Regression test / issue:
```