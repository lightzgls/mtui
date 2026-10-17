//! Opt-in CPU work comparison. No network, audio device, GUI, or real sleeps.
//! This measures App maintenance and Ratatui work, not whole-process CPU %.
use super::*;
use crate::refresh::Refresh;
use ratatui::{Terminal, backend::TestBackend};

#[test]
#[ignore = "headless performance measurement; run in release mode with --nocapture"]
fn compare_foreground_frame_work() {
    const POLLS: u64 = 1_000;
    let mut measurements = Vec::new();
    for (name, state, tab) in [
        ("playing_queue", PlayState::Playing, Tab::UpNext),
        ("paused_queue", PlayState::Paused, Tab::UpNext),
        ("synced_lyrics", PlayState::Playing, Tab::Lyrics),
    ] {
        let mut final_buffers = Vec::new();
        for adaptive in [false, true] {
            let (mut app, driver, _requests) = super::scenarios::fixture();
            let mut now = NowPlaying::new(&app.results[0]);
            now.tab = tab;
            now.duration = Some(Duration::from_secs(240));
            now.queue = app.results.clone();
            now.playing = Some(0);
            let timed: Vec<_> = (0..80)
                .map(|index| crate::source::watch::TimedLine {
                    text: format!("Synthetic lyric line {index} with enough words to fill a row"),
                    start: Duration::from_millis(index * 2_500),
                })
                .collect();
            now.lyrics = Panel::Ready(Lyrics {
                text: timed
                    .iter()
                    .map(|line| line.text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
                timed,
                source: Some("Synthetic performance fixture".into()),
            });
            app.now = Some(now);
            app.view = View::Playing;
            app.cover = Some(Cover::solid(320, 320));
            let mut terminal = Terminal::new(TestBackend::new(160, 42)).unwrap();
            let mut mouse = crate::ui::MouseMap::default();
            let start = Instant::now();
            let mut refresh = Refresh::new(start);
            let mut frames = 0;
            for tick in 0..POLLS {
                let elapsed = Duration::from_millis(tick * 200);
                let position = if state == PlayState::Paused {
                    Duration::from_secs(10)
                } else {
                    elapsed
                };
                *driver.snapshot.lock().unwrap() = Snapshot {
                    state,
                    position,
                    volume: 0.8,
                    title: "Song 0".into(),
                    error: None,
                };
                let changed = app.poll_source();
                app.tick_playback();
                app.tick_page();
                app.tick_prefetch();
                app.tick_presence();
                let snapshot = app.snapshot();
                let lyric = (tab == Tab::Lyrics)
                    .then(|| match &app.now.as_ref().unwrap().lyrics {
                        Panel::Ready(lyrics) => lyrics.singing(snapshot.position),
                        _ => None,
                    })
                    .flatten();
                if !adaptive
                    || refresh.due(
                        snapshot,
                        lyric,
                        changed,
                        crate::ui::animated(),
                        start + elapsed,
                    )
                {
                    terminal
                        .draw(|frame| crate::ui::render(frame, &mut app, &mut mouse))
                        .unwrap();
                    frames += 1;
                }
            }
            let micros = start.elapsed().as_micros();
            measurements.push(serde_json::json!({
                "scenario": name, "adaptive": adaptive, "polls": POLLS,
                "frames": frames, "work_us": micros,
            }));
            terminal
                .draw(|frame| crate::ui::render(frame, &mut app, &mut mouse))
                .unwrap();
            final_buffers.push(terminal.backend().buffer().clone());
        }
        // Draw at the same final position before comparing pixels: the fast
        // path deliberately omits subsecond clock-only frames.
        assert_eq!(
            final_buffers[0], final_buffers[1],
            "{name} must end with the same visible content"
        );
    }
    println!("{}", serde_json::to_string_pretty(&measurements).unwrap());
}
