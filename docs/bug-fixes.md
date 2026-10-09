# October 2026 bug fixes

This pass addresses the failures identified in the playback, artwork, browsing,
account-action, and settings audit. The output-reset report was reproduced at
the decoder boundary: a temporary empty AAC packet span stopped the resume
skip after one packet, while the progress bar kept the saved position.

| Area | Change | Verification |
| --- | --- | --- |
| Audio ranges | Validate partial ranges; discard the prefix of full responses when a server ignores Range | Loopback reproduction now returns `ABCDEF`, previously `ABCABCDEF` |
| Interrupted audio | Retry interrupted transport bodies and incomplete advertised ranges within the existing retry budget | Truncated and chunked fixtures retry the same range without duplicating partial bytes |
| Output changes | Keep packet spans valid through actual EOF so reopening audio skips to the saved position; preserve pause state and outstanding seeks | PCM packet regressions reproduce the restart; a real AAC fixture resumes at eight seconds with 2,048 samples exactly matching the original audio; device probes cover playing, paused, and seeked playback |
| Downloads | Enforce 2 MiB artwork and 8 MiB metadata limits while reading decoded bytes | Unknown-length, advertised-length, and compressed-body regressions |
| JPEG memory | Reject excessive dimensions before decoding; bound decode buffers, scale early, and reduce copies | The 4096-square fixture's measured peak increase fell from about 102 MiB to about 0.4 MiB through early rejection |
| Cover colours | Use wider weighted palette accumulators | A saturated 720-square cover retains its expected colour |
| Session signing | Skip empty signing cookies and use the valid fallback | Empty-only sessions rejected; fallback and duplicate-cookie regressions |
| Account isolation | Bind queued actions to their session and invalidate them on logout or refresh | Account identity regressions; authenticated playlist and Like reads |
| Automatic sign-in maintenance | Renew saved browser sessions every 48 hours and after explicit authentication failures; persist retry cooldowns and request verification only for confirmed Google sign-in | Scheduler, helper deadline, guest-response, cancellation, and recovery classification regressions |
| Responsiveness | Coalesce/cancel obsolete reads, isolate account actions, and fetch missing search categories two at a time | Queue and response-body cancellation regressions |
| Artwork backlog | Queue only visible cards, prioritize recent requests, and limit batches to four | Rapid page changes keep a bounded backlog; revisiting unfinished tiles requeues them |
| Playlist saves | Route Liked Music through Like; preserve duplicate checks; accept explicit successful acknowledgements; verify ambiguous outcomes using actual tracks | Modern menu fixtures, duplicate dialogs, exact-song checks, and continuation tests |
| Provider errors | Distinguish expired sessions, permissions, rate limits, and server failures; redact and bound details | Error classification and redaction regressions |
| Settings | Replace files atomically and debounce volume saves | Concurrent readers see complete JSON throughout replacements |

Playlist handling follows the current [ytmusicapi request and response contract](https://github.com/sigma67/ytmusicapi/blob/main/ytmusicapi/mixins/playlists.py).
No test in this pass adds or removes music from the user's account. An actual
save from the rebuilt application still needs confirmation with the account's
chosen playlist. Memory figures above describe the isolated JPEG fixture, not
the application's overall working set.

The hidden sign-in helper also verified the existing saved browser profile in
a live smoke test. Its captured credentials stayed in the private process pipe;
the probe did not rewrite the saved import or display a sign-in window.

Validation commands:

```powershell
cargo test --workspace --all-targets --offline
cargo clippy --workspace --all-targets --offline -- -D warnings
cargo build --release --offline --bin mtui
```

The Windows build remains one `mtui.exe`, including the sign-in and terminal
helpers. Detailed local probe output is kept under ignored `target/bug-hunt/`.
