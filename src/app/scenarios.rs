//! User action sequences with controlled worker timing. No audio or network.
use super::*;
use crate::player::TestDriver;
use std::sync::mpsc::Receiver;

pub(super) fn fixture() -> (App, TestDriver, Receiver<Request>) {
    let (player, driver) = Player::for_test();
    let (source, requests) = SourceWorker::for_test();
    let mut app = App::with_services(
        player, source, Graphics::blocks(), config::Settings::default(),
        Vec::new(), Presence::for_test(),
    );
    app.home_pending = false;
    app.view = View::Tracks;
    app.results = (0..4).map(track).collect();
    requests.try_iter().for_each(drop);
    (app, driver, requests)
}

fn track(index: usize) -> Track {
    Track {
        id: format!("song-{index}"), title: format!("Song {index}"), uploader: "Artist".into(),
        duration: Some(Duration::from_secs(240)), album: None, artist_ref: None,
    }
}

#[test]
fn home_order_refresh_and_grid_input_keep_the_selected_song() {
    let (mut app, _driver, _requests) = fixture();
    let shelf = |title: &str, start| Shelf {
        title: title.into(),
        cards: (start..start + 9).map(|index| Card {
            title: format!("Song {index}"), subtitle: "Artist".into(), art: None,
            duration: None, artist_ref: None,
            target: Target::Play { video_id: format!("song-{index}") },
        }).collect(),
    };
    let feed = vec![shelf("Quick picks", 0), shelf("Albums for you", 20), shelf("Listen again", 40)];
    app.apply_home(feed.clone());
    app.view = View::Home;
    assert_eq!(app.home.iter().map(|shelf| shelf.title.as_str()).collect::<Vec<_>>(),
        ["Listen again", "Quick picks", "Albums for you"]);
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(160, 42)).unwrap();
    let mut mouse = crate::ui::MouseMap::default();
    terminal.draw(|frame| crate::ui::render(frame, &mut app, &mut mouse)).unwrap();
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)).unwrap();
    assert_eq!(app.home_shelf, 1);
    terminal.draw(|frame| crate::ui::render(frame, &mut app, &mut mouse)).unwrap();
    assert!(app.home_grid_rows > 1, "Quick picks should use a grid");
    app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)).unwrap();
    assert_eq!(app.home_card, app.home_grid_rows);
    let point = (0..42).find_map(|y| (0..160).find_map(|x|
        (mouse.context_action_at(x, y) == Some(MouseAction::SelectHomeCard { shelf: 1, card: 2 })).then_some((x, y)))).unwrap();
    app.handle_mouse_action(mouse.context_action_at(point.0, point.1).unwrap()).unwrap();
    assert_eq!(app.home_card().unwrap().art_key(), "song-2");
    app.apply_home(vec![feed[1].clone(), feed[0].clone(), feed[2].clone()]);
    assert_eq!(app.home_shelf, 1);
    assert_eq!(app.home_card().unwrap().art_key(), "song-2", "a refresh must retain the song, not the API position");
}

#[test]
fn home_sections_can_be_paged_with_only_mouse_clicks() {
    for (width, height) in [(160, 42), (80, 32), (28, 24)] {
        let (mut app, driver, _requests) = fixture();
        driver.commands.try_iter().for_each(drop);
        let shelf = |title: &str| Shelf {
            title: title.into(),
            cards: (0..20).map(|index| Card {
                title: format!("Song {index}"), subtitle: "Artist".into(), art: None,
                duration: None, artist_ref: None,
                target: Target::Play { video_id: format!("song-{index}") },
            }).collect(),
        };
        app.apply_home(vec![shelf("Listen again"), shelf("Quick picks")]);
        app.view = View::Home;
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        let mut mouse = crate::ui::MouseMap::default();
        for section in 0..2 {
            // A short window first reaches Quick picks by the normal vertical navigation.
            app.home_shelf = section;
            app.home_card = 0;
            terminal.draw(|frame| crate::ui::render(frame, &mut app, &mut mouse)).unwrap();
            let next = |mouse: &crate::ui::MouseMap, first: usize, forward: bool| {
                (0..height).find_map(|y| (0..width).find_map(|x| {
                    match mouse.action_at(x, y) {
                        Some(action @ MouseAction::PageHomeShelf { shelf, offset, .. })
                            if shelf == section && if forward { offset > first } else { offset < first } => Some((x, y, action)),
                        _ => None,
                    }
                }))
            };
            assert!(next(&mouse, 0, false).is_none(), "the first page has no previous action");
            let mut pages = 0;
            while let Some((x, y, action)) = next(&mouse, app.home_scroll[section], true) {
                assert_eq!(mouse.action_at(x + 2, y), Some(action), "the whole button is clickable");
                assert_ne!(mouse.action_at(x, y + 1), Some(action), "the heading gap cannot trigger a page");
                app.handle_mouse_action(action).unwrap();
                let first = app.home_scroll[section];
                terminal.draw(|frame| crate::ui::render(frame, &mut app, &mut mouse)).unwrap();
                assert_eq!(app.home_scroll[section], first, "redraw must keep the requested page");
                assert_eq!(app.home_card, first);
                assert_eq!(first % app.home_grid_rows, 0, "grid pages start at a whole column");
                pages += 1;
                assert!(pages <= 20, "paging must terminate");
            }
            assert!(pages > 0, "{width}x{height}: section {section} has offscreen songs");
            assert!((0..height).any(|y| (0..width).any(|x|
                mouse.action_at(x, y) == Some(MouseAction::OpenHomeCard { shelf: section, card: 19 }))),
                "the last card must be reachable with the mouse");
            while let Some((_, _, action)) = next(&mouse, app.home_scroll[section], false) {
                app.handle_mouse_action(action).unwrap();
                terminal.draw(|frame| crate::ui::render(frame, &mut app, &mut mouse)).unwrap();
            }
            assert_eq!(app.home_scroll[section], 0);
        }
        assert_eq!(driver.commands.try_iter().count(), 0, "browsing a shelf must not start playback");
        app.handle_mouse_action(MouseAction::PageHomeShelf { shelf: 99, offset: 10, columns: 4, rows: 3 }).unwrap();
        assert_eq!(app.home_shelf, 1, "stale actions for a replaced shelf must be ignored");
    }
}

#[test]
fn lyrics_scrolling_follow_and_expand_work_through_real_keyboard_actions() {
    let (mut app, _driver, _requests) = fixture();
    queue(&mut app, RepeatMode::Off, 0);
    let now = app.now.as_mut().unwrap();
    now.open(Tab::Lyrics);
    now.lyrics = Panel::Ready(Lyrics {
        text: "An invented test line".into(), source: None,
        timed: vec![crate::source::watch::TimedLine { text: "An invented test line".into(), start: Duration::ZERO }],
    });
    for key in [KeyCode::Down, KeyCode::Char(' '), KeyCode::Char('+')] {
        app.handle_key(KeyEvent::new(key, KeyModifiers::NONE)).unwrap();
        assert!(!app.now.as_ref().unwrap().follow_lyrics, "scrolling must keep control of the lyrics");
    }
    app.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE)).unwrap();
    assert!(app.now.as_ref().unwrap().follow_lyrics);
    app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE)).unwrap();
    assert!(!app.now.as_ref().unwrap().follow_lyrics);
    app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE)).unwrap();
    assert!(app.now.as_ref().unwrap().lyrics_expanded);
    assert!(!app.now.as_ref().unwrap().follow_lyrics, "expanding must not change follow mode");
    app.handle_mouse_action(MouseAction::ToggleLyricsFollow).unwrap();
    assert!(app.now.as_ref().unwrap().follow_lyrics);
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)).unwrap();
    assert!(!app.now.as_ref().unwrap().lyrics_expanded);
    assert_eq!(app.view, View::Playing, "first Esc restores the artwork");
}

#[test]
fn the_lyrics_reading_view_survives_track_changes_and_ignores_queue_actions() {
    let (mut app, _driver, _requests) = fixture();
    queue(&mut app, RepeatMode::Off, 0);
    app.now.as_mut().unwrap().open(Tab::Lyrics);
    app.cover_size = CoverSize::Full;
    app.handle_mouse_action(MouseAction::ToggleLyricsExpanded).unwrap();
    assert_eq!(app.cover_size, CoverSize::Side);
    app.advance(1, false);
    assert!(app.now.as_ref().unwrap().lyrics_expanded);
    app.handle_mouse_action(MouseAction::OpenTab(Tab::UpNext)).unwrap();
    app.handle_mouse_action(MouseAction::ToggleLyricsExpanded).unwrap();
    assert!(app.now.as_ref().unwrap().lyrics_expanded, "hidden lyrics controls must not alter another panel");
}

fn resolve(app: &mut App, index: usize) {
    app.apply(Response::Resolved {
        id: track(index).id, title: track(index).title,
        stream: Ok(StreamUrl {
            url: format!("https://fixture.invalid/song-{index}.m4a"),
            expires_at: None, content_length: Some(1024),
            format: mtui_resolver::AudioFormat { itag: Some(140) },
            source: mtui_resolver::ResolveSource::Cache,
        }),
    });
}

fn state(driver: &TestDriver, state: PlayState, position: Duration) {
    *driver.snapshot.lock().unwrap() = Snapshot { state, position, ..Default::default() };
}

fn queue(app: &mut App, repeat: RepeatMode, at: usize) {
    let mut now = NowPlaying::new(&track(at));
    now.queue = (0..4).map(track).collect();
    now.playing = Some(at);
    now.repeat = repeat;
    now.queue_epoch = 7;
    now.continuation = Some("next-page".into());
    app.queue_epoch = 7;
    app.now = Some(now);
    app.view = View::Playing;
}

// SEEK-01: the click can precede both URL resolution and the Load snapshot.
#[test]
fn selecting_a_song_then_immediately_seeking_to_end_keeps_the_seek() {
    for initial in [PlayState::Idle, PlayState::Buffering, PlayState::Playing] {
        let (mut app, driver, _requests) = fixture();
        state(&driver, initial, Duration::ZERO);
        app.handle_mouse_action(MouseAction::PlayTrack(0)).unwrap();
        app.handle_mouse_action(MouseAction::SeekTo(POINTER_SCALE)).unwrap();
        resolve(&mut app, 0);
        let commands: Vec<_> = driver.commands.try_iter().collect();
        assert!(matches!(commands.as_slice(), [Command::Load { .. }, Command::Play { id, .. }, Command::Seek(target)]
            if id == "song-0" && *target == Duration::from_secs(240)), "{initial:?}: {commands:?}");
    }
}

#[test]
fn an_end_click_after_resolution_but_before_the_player_snapshot_is_kept() {
    let (mut app, driver, _requests) = fixture();
    app.handle_mouse_action(MouseAction::PlayTrack(0)).unwrap();
    resolve(&mut app, 0);
    assert_eq!(app.snapshot().state, PlayState::Idle);
    app.handle_mouse_action(MouseAction::SeekTo(POINTER_SCALE)).unwrap();
    let commands: Vec<_> = driver.commands.try_iter().collect();
    assert!(matches!(commands.as_slice(), [Command::Load { .. }, Command::Play { .. }, Command::Seek(target)]
        if *target == Duration::from_secs(240)), "{commands:?}");
}

// SEEK-02: only the final scrub position belongs to the song being loaded.
#[test]
fn scrubbing_during_resolution_uses_the_last_position() {
    let (mut app, driver, _requests) = fixture();
    app.handle_mouse_action(MouseAction::PlayTrack(0)).unwrap();
    for position in [POINTER_SCALE, 0, POINTER_SCALE / 2] {
        app.handle_mouse_action(MouseAction::SeekTo(position)).unwrap();
    }
    resolve(&mut app, 0);
    let commands: Vec<_> = driver.commands.try_iter().collect();
    assert!(matches!(commands.as_slice(), [Command::Load { .. }, Command::Play { .. }, Command::Seek(target)]
        if *target == Duration::from_secs(120)), "{commands:?}");
}

// SEEK-03: Next wins; neither the old URL nor its end seek may reach the new song.
#[test]
fn selecting_another_song_discards_the_previous_seek_and_late_url() {
    let (mut app, driver, _requests) = fixture();
    app.play_track(track(0), false);
    app.handle_mouse_action(MouseAction::SeekTo(POINTER_SCALE)).unwrap();
    app.play_track(track(1), false);
    resolve(&mut app, 0);
    resolve(&mut app, 1);
    let commands: Vec<_> = driver.commands.try_iter().collect();
    assert!(matches!(commands.as_slice(), [Command::Load { .. }, Command::Load { .. }, Command::Play { id, .. }]
        if id == "song-1"), "{commands:?}");
    assert_eq!(app.now.as_ref().unwrap().video_id, "song-1");
}

// SEEK-04: Stop owns silence even when resolution succeeds afterwards.
#[test]
fn stop_after_an_end_click_cancels_the_song_and_late_resolution() {
    let (mut app, driver, _requests) = fixture();
    app.play_track(track(0), false);
    app.handle_mouse_action(MouseAction::SeekTo(POINTER_SCALE)).unwrap();
    app.stop();
    resolve(&mut app, 0);
    let commands: Vec<_> = driver.commands.try_iter().collect();
    assert!(matches!(commands.as_slice(), [Command::Load { .. }, Command::Stop]), "{commands:?}");
    assert!(app.now.is_none() && app.pending.is_none());
}

// SEEK-05: live/unknown duration has no invented endpoint or absolute seek.
#[test]
fn clicking_the_end_of_an_unknown_duration_does_not_invent_a_seek() {
    for playback in [PlayState::Buffering, PlayState::Playing, PlayState::Paused] {
        let (mut app, driver, _requests) = fixture();
        app.results[0].duration = None;
        app.handle_mouse_action(MouseAction::PlayTrack(0)).unwrap();
        state(&driver, playback, Duration::ZERO);
        app.handle_mouse_action(MouseAction::SeekTo(POINTER_SCALE)).unwrap();
        resolve(&mut app, 0);
        assert!(driver.commands.try_iter().all(|cmd| !matches!(cmd, Command::Seek(_))));
    }
}

// SEEK-06: zero-duration metadata and out-of-range pointers stay bounded.
#[test]
fn seeks_with_known_duration_are_bounded_in_all_active_states() {
    for playback in [PlayState::Buffering, PlayState::Playing, PlayState::Paused] {
        for total in [Duration::ZERO, Duration::from_millis(1), Duration::from_secs(240)] {
            for pointer in [0, 1, POINTER_SCALE - 1, POINTER_SCALE, u16::MAX] {
                let (mut app, driver, _requests) = fixture();
                app.now = Some(NowPlaying::new(&track(0)));
                app.now.as_mut().unwrap().duration = Some(total);
                state(&driver, playback, Duration::ZERO);
                app.handle_mouse_action(MouseAction::SeekTo(pointer)).unwrap();
                let commands: Vec<_> = driver.commands.try_iter().collect();
                assert!(matches!(commands.as_slice(), [Command::Seek(target)] if *target <= total), "{commands:?}");
                assert_eq!(app.snapshot().state, playback, "seeking must not toggle pause");
            }
        }
    }
}

// SEEK-07: duplicate EOF observations cannot advance two tracks.
#[test]
fn an_end_seek_advances_exactly_once_for_each_repeat_mode() {
    for repeat in [RepeatMode::Off, RepeatMode::All, RepeatMode::One] {
        for at in [0, 3] {
            let (mut app, driver, requests) = fixture();
            queue(&mut app, repeat, at);
            state(&driver, PlayState::Playing, Duration::ZERO);
            app.tick_playback();
            app.handle_mouse_action(MouseAction::SeekTo(POINTER_SCALE)).unwrap();
            driver.commands.try_iter().for_each(drop);
            state(&driver, PlayState::Idle, Duration::ZERO);
            for _ in 0..20 { app.tick_playback(); }
            let expected = match (repeat, at) {
                (RepeatMode::One, _) => Some(at),
                (RepeatMode::All, 3) => Some(0),
                (RepeatMode::Off, 3) => None,
                _ => Some(at + 1),
            };
            let resolved: Vec<_> = requests.try_iter().filter_map(|request| match request {
                Request::Resolve { id, .. } => Some(id), _ => None,
            }).collect();
            assert_eq!(resolved, expected.map(|index| vec![track(index).id]).unwrap_or_default(), "{repeat:?} at {at}");
        }
    }
}

// SEEK-08: EOF while a newly chosen song is pending cannot skip that choice.
#[test]
fn an_old_eof_while_resolving_a_new_choice_cannot_advance_it() {
    let (mut app, driver, requests) = fixture();
    queue(&mut app, RepeatMode::Off, 0);
    state(&driver, PlayState::Playing, Duration::ZERO);
    app.tick_playback();
    app.advance(1, false);
    state(&driver, PlayState::Idle, Duration::ZERO);
    app.tick_playback();
    assert_eq!(app.pending.as_deref(), Some("song-1"));
    assert_eq!(app.listening.as_ref().map(|(track, _)| track.id.as_str()), Some("song-1"),
        "an old EOF must not discard the new song's history/recovery identity");
    assert_eq!(requests.try_iter().filter(|r| matches!(r, Request::Resolve { .. })).count(), 1);
}

// SEEK-10: an immediate end seek can drain before any Playing frame is drawn.
#[test]
fn an_end_seek_that_finishes_during_buffering_still_advances_once() {
    let (mut app, driver, requests) = fixture();
    queue(&mut app, RepeatMode::Off, 0);
    app.play_track(track(0), false);
    state(&driver, PlayState::Buffering, Duration::ZERO);
    app.tick_playback();
    app.handle_mouse_action(MouseAction::SeekTo(POINTER_SCALE)).unwrap();
    resolve(&mut app, 0);
    requests.try_iter().for_each(drop);
    state(&driver, PlayState::Idle, Duration::ZERO);
    for _ in 0..20 { app.tick_playback(); }
    assert_eq!(app.pending.as_deref(), Some("song-1"));
    assert_eq!(requests.try_iter().filter(|r| matches!(r, Request::Resolve { .. })).count(), 1);
}

// SEEK-11: a failed open is never an end-of-track event.
#[test]
fn a_buffering_failure_is_reported_without_advancing_the_queue() {
    let (mut app, driver, requests) = fixture();
    queue(&mut app, RepeatMode::Off, 0);
    state(&driver, PlayState::Buffering, Duration::ZERO);
    app.tick_playback();
    state(&driver, PlayState::Idle, Duration::ZERO);
    driver.snapshot.lock().unwrap().error = Some("network timeout".into());
    app.tick_playback();
    assert!(app.playback_error.is_some());
    assert_eq!(app.now.as_ref().unwrap().video_id, "song-0");
    assert!(!requests.try_iter().any(|r| matches!(r, Request::Resolve { .. })));
}

#[test]
fn a_failed_resolve_during_buffering_is_not_mistaken_for_an_end_seek() {
    let (mut app, driver, requests) = fixture();
    queue(&mut app, RepeatMode::Off, 0);
    app.play_track(track(0), false);
    state(&driver, PlayState::Buffering, Duration::ZERO);
    app.tick_playback();
    requests.try_iter().for_each(drop);
    app.apply(Response::Resolved { id: "song-0".into(), title: "Song 0".into(), stream: Err("network timeout".into()) });
    state(&driver, PlayState::Idle, Duration::ZERO);
    app.tick_playback();
    assert!(app.playback_error.is_some());
    assert_eq!(app.now.as_ref().unwrap().video_id, "song-0");
    assert!(!requests.try_iter().any(|r| matches!(r, Request::Resolve { .. })));
}

// SEEK-09: a late recovery event from the old song cannot reopen it.
#[test]
fn a_late_recovery_event_after_next_is_ignored() {
    let (mut app, driver, _requests) = fixture();
    app.play_track(track(0), false);
    app.play_track(track(1), false);
    driver.commands.try_iter().for_each(drop);
    driver.events.send(PlayerEvent::NeedsUrl { id: "song-0".into(), from: Duration::from_secs(240) }).unwrap();
    app.poll_player();
    assert!(app.resuming.is_none());
    assert_eq!(app.pending.as_deref(), Some("song-1"));
    assert!(driver.commands.try_recv().is_err());
}

// QUE-01: Clear invalidates an in-flight page even if no upcoming rows remain.
#[test]
fn clearing_a_queue_with_only_the_current_song_rejects_a_late_page() {
    let (mut app, driver, _requests) = fixture();
    queue(&mut app, RepeatMode::Off, 0);
    let now = app.now.as_mut().unwrap();
    now.queue.truncate(1);
    now.topping_up = true;
    state(&driver, PlayState::Playing, Duration::ZERO);
    app.clear_upcoming_queue();
    app.apply_more_queue(7, Ok(QueuePage { tracks: vec![track(1)], continuation: None, title: None }));
    let now = app.now.as_ref().unwrap();
    assert_eq!(now.queue.len(), 1, "Clear must cancel an in-flight continuation too");
    assert!(!now.topping_up);
}

// QUE-02: a continuation cannot autoplay through Pause, or resurrect Stop.
#[test]
fn a_late_queue_page_respects_pause_and_stop() {
    let (mut app, driver, _requests) = fixture();
    queue(&mut app, RepeatMode::Off, 0);
    state(&driver, PlayState::Paused, Duration::from_secs(120));
    app.apply_more_queue(7, Ok(QueuePage { tracks: vec![track(4)], continuation: None, title: None }));
    assert!(driver.commands.try_recv().is_err());
    app.stop();
    driver.commands.try_iter().for_each(drop);
    app.apply_more_queue(7, Ok(QueuePage { tracks: vec![track(5)], continuation: None, title: None }));
    assert!(app.now.is_none());
    assert!(driver.commands.try_recv().is_err());
}

// META-01: late lyrics and artwork retain the identity of the current song.
#[test]
fn late_content_for_a_skipped_song_does_not_replace_the_current_song() {
    let (mut app, _driver, _requests) = fixture();
    app.play_track(track(0), false);
    app.play_track(track(1), false);
    app.apply(Response::Lyrics { video_id: "song-0".into(), lyrics: Ok(Lyrics { text: "old lyric".into(), source: None, timed: Vec::new() }) });
    app.apply(Response::Cover { id: "song-0".into(), art: Some(Cover::from_rgb(1, 1, vec![255, 0, 0])) });
    assert!(matches!(app.now.as_ref().unwrap().lyrics, Panel::Idle));
    assert!(app.cover.is_none());
    assert_eq!(app.now.as_ref().unwrap().video_id, "song-1");
}

// SEEK-12: a seek cannot resume a paused song or masquerade as a finished play.
#[test]
fn an_end_click_while_paused_does_not_autoplay_the_next_song() {
    let (mut app, driver, requests) = fixture();
    queue(&mut app, RepeatMode::Off, 0);
    state(&driver, PlayState::Paused, Duration::from_secs(120));
    app.tick_playback();
    app.handle_mouse_action(MouseAction::SeekTo(POINTER_SCALE)).unwrap();
    app.tick_playback();
    assert_eq!(app.now.as_ref().unwrap().video_id, "song-0");
    assert_eq!(app.snapshot().state, PlayState::Paused);
    assert!(!requests.try_iter().any(|r| matches!(r, Request::Resolve { .. })));
    assert!(matches!(driver.commands.try_recv().unwrap(), Command::Seek(target) if target == Duration::from_secs(240)));
}

// QUE-03: changing repeat during a top-up invalidates the older queue epoch.
#[test]
fn enabling_repeat_all_rejects_an_in_flight_radio_page() {
    let (mut app, driver, _requests) = fixture();
    queue(&mut app, RepeatMode::Off, 0);
    state(&driver, PlayState::Playing, Duration::ZERO);
    app.now.as_mut().unwrap().topping_up = true;
    app.cycle_repeat();
    app.apply_more_queue(7, Ok(QueuePage { tracks: vec![track(4)], continuation: None, title: None }));
    let now = app.now.as_ref().unwrap();
    assert_eq!(now.repeat, RepeatMode::All);
    assert_eq!(now.queue.len(), 4);
    assert!(!now.topping_up);
}

// QUE-04: mixed operations exercise bounds and identity over reproducible seeds.
#[test]
fn mixed_queue_actions_keep_current_identity_and_memory_bounded() {
    use std::collections::HashSet;
    for initial_seed in [1_u64, 7, 42, 0xBAD5EED] {
        let mut seed = initial_seed;
        let mut now = NowPlaying::new(&track(0));
        now.queue = (0..30).map(track).collect();
        now.playing = Some(0);
        for step in 0..4096 {
            seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17;
            let at = now.playing.unwrap();
            let prefix: Vec<_> = now.queue[..=at].iter().map(|t| t.id.clone()).collect();
            let action = seed % 7;
            match action {
                0 => { now.insert_user_track(track(1000 + step), true); }
                1 => { now.insert_user_track(track(1000 + step), false); }
                2 => {
                    now.cursor[Tab::UpNext.index()] = (seed as usize) % now.queue.len();
                    now.remove_selected_upcoming();
                }
                3 => {
                    now.cursor[Tab::UpNext.index()] = (seed as usize) % now.queue.len();
                    now.move_selected_upcoming(if step % 2 == 0 { -1 } else { 1 });
                }
                4 => { now.shuffle_upcoming(); }
                5 => { now.clear_upcoming(); }
                _ => { now.absorb(vec![track(2000 + step), track(2000 + step)]); }
            }
            assert_eq!(now.queue[..=at].iter().map(|t| t.id.clone()).collect::<Vec<_>>(), prefix,
                "seed {initial_seed}, step {step}, action {action} changed the current song/history");
            if step % 5 == 0 && now.next_in_queue().is_some() {
                now.playing = Some(at + 1);
                let current = now.queue[at + 1].id.clone();
                now.trim();
                assert_eq!(now.queue[now.playing.unwrap()].id, current);
            }
            assert!(now.queue.len() <= QUEUE_BEHIND + 1 + QUEUE_AHEAD);
            assert!(now.dropped.len() <= QUEUE_MEMORY);
            assert_eq!(now.queue.iter().map(|t| &t.id).collect::<HashSet<_>>().len(), now.queue.len());
        }
    }
}
