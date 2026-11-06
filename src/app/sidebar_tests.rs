//! Persistent player behavior exercised through real frame composition and input.

use std::time::{Duration, Instant};

use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{Terminal, backend::TestBackend};

use super::*;
use crate::player::{PlayState, Snapshot};
use crate::pointer::Pointer;
use crate::source::watch::Lyrics;
use crate::source::worker::Request;
use crate::ui::MouseMap;

fn setup() -> (App, crate::player::TestDriver, std::sync::mpsc::Receiver<Request>) {
    let (mut app, driver, requests) = super::scenarios::fixture();
    let mut now = NowPlaying::new(&app.results[0]);
    now.queue = app.results.clone();
    now.playing = Some(0);
    now.lyrics = Panel::Ready(Lyrics {
        text: (0..40).map(|i| format!("Lyric line {i}\n")).collect(),
        source: None, timed: Vec::new(),
    });
    app.now = Some(now);
    (app, driver, requests)
}

fn draw(app: &mut App, width: u16, height: u16, name: &str) -> (Terminal<TestBackend>, MouseMap) {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let mut mouse = MouseMap::default();
    terminal.draw(|frame| crate::ui::render(frame, app, &mut mouse)).unwrap();
    if std::env::var_os("MTUI_PREVIEW_DIR").is_some() {
        crate::ui::preview_buffer(&format!("{name}-{width}x{height}"), terminal.backend().buffer());
    }
    (terminal, mouse)
}

fn locate(mouse: &MouseMap, width: u16, height: u16, action: MouseAction) -> (u16, u16) {
    (0..height).find_map(|y| (0..width).find_map(|x|
        (mouse.action_at(x, y) == Some(action)).then_some((x, y))))
        .unwrap_or_else(|| panic!("missing {action:?} at {width}x{height}"))
}

fn event(kind: MouseEventKind, (column, row): (u16, u16)) -> MouseEvent {
    MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE }
}

#[test]
fn sidebar_lyrics_have_space_between_lines_in_a_medium_window() {
    let (mut app, _, _) = setup();
    app.now.as_mut().unwrap().open(Tab::Lyrics);
    let (terminal, _) = draw(&mut app, 120, 32, "sidebar-lyric-spacing");
    let buffer = terminal.backend().buffer();
    let rows: Vec<String> = (0..32)
        .map(|y| (0..120).map(|x| buffer[(x, y)].symbol()).collect())
        .collect();
    let first = rows.iter().position(|row| row.contains("Lyric line 0")).unwrap();
    assert!(rows[first + 2].contains("Lyric line 1"));
}

#[test]
fn dragging_each_divider_moves_its_rendered_boundary() {
    use crate::ui::Split;
    let (mut app, _, _) = setup();
    let mut pointer = Pointer::default();
    for (view, split, dx, dy) in [
        (View::Tracks, Split::SidebarWidth, -8, 0),
        (View::Tracks, Split::SidebarArtwork, 0, 4),
        (View::Playing, Split::PlayerWidth, -8, 0),
        (View::Playing, Split::PlayerDetails, 0, 4),
    ] {
        app.view = view;
        let find = |mouse: &MouseMap| {
            (0..36).find_map(|y| (0..120).find_map(|x| {
                mouse.divider_at(x, y).is_some_and(|(found, _)| found == split).then_some((x, y))
            })).unwrap()
        };
        let (_, mouse) = draw(&mut app, 120, 36, "divider-before");
        let start = find(&mouse);
        let end = (start.0.saturating_add_signed(dx), start.1.saturating_add_signed(dy));
        for (kind, point) in [
            (MouseEventKind::Down(MouseButton::Left), start),
            (MouseEventKind::Drag(MouseButton::Left), end),
            (MouseEventKind::Up(MouseButton::Left), end),
        ] {
            pointer.handle(&mut app, &mouse, event(kind, point)).unwrap();
        }
        let (_, mouse) = draw(&mut app, 120, 36, "divider-after");
        let moved = find(&mouse);
        if dx != 0 { assert_eq!(moved.0, end.0, "{split:?}"); }
        if dy != 0 { assert_eq!(moved.1, end.1, "{split:?}"); }
        assert_eq!(app.now.as_ref().unwrap().video_id, "song-0");
    }
}

#[test]
fn sidebar_survives_navigation_pause_and_missing_artwork_and_disappears_after_stop() {
    let (mut app, driver, _) = setup();
    for state in [PlayState::Playing, PlayState::Paused, PlayState::Buffering] {
        *driver.snapshot.lock().unwrap() = Snapshot { state, ..Default::default() };
        for view in [View::Home, View::Tracks, View::Library, View::Artist] {
            app.view = view;
            for (width, height) in [(120, 36), (80, 24), (48, 18)] {
                let (terminal, mouse) = draw(&mut app, width, height, "sidebar");
                locate(&mouse, width, height, MouseAction::OpenTab(Tab::UpNext));
                locate(&mouse, width, height, MouseAction::OpenTab(Tab::Lyrics));
                let (x, y) = locate(&mouse, width, height, MouseAction::SelectPageRow(0));
                let row: String = (x..width).map(|x| terminal.backend().buffer()[(x, y)].symbol()).collect();
                assert!(row.contains("Song 0"), "a narrow queue must retain its song title: {row}");
                assert!((0..height).any(|y| (width / 2..width).any(|x|
                    mouse.player_panel_at(x, y).is_some())));
            }
        }
    }
    app.stop();
    let (_, mouse) = draw(&mut app, 120, 36, "sidebar-stopped");
    assert!(app.now.is_none());
    assert!((0..36).all(|y| (0..120).all(|x| mouse.player_panel_at(x, y).is_none())));
}

#[test]
fn sidebar_tabs_and_wheel_preserve_the_search_draft_and_main_selection() {
    let (mut app, _, _) = setup();
    app.mode = Mode::Editing;
    app.query = "unfinished search".into();
    app.query_cursor = app.query.len();
    app.selected = 2;
    app.handle_mouse_action(MouseAction::OpenTab(Tab::Lyrics)).unwrap();
    let (_, mouse) = draw(&mut app, 120, 36, "sidebar-lyrics");
    let point = (0..36).find_map(|y| (0..120).find_map(|x|
        (mouse.player_panel_at(x, y) == Some(Tab::Lyrics)).then_some((x, y)))).unwrap();
    Pointer::default().handle(&mut app, &mouse, event(MouseEventKind::ScrollDown, point)).unwrap();
    let now = app.now.as_ref().unwrap();
    assert_eq!(now.cursor[Tab::Lyrics.index()], 1);
    assert!(!now.follow_lyrics);
    assert_eq!(app.mode, Mode::Editing);
    assert_eq!(app.view, View::Tracks);
    assert_eq!(app.query, "unfinished search");
    assert_eq!(app.selected, 2);
}

#[test]
fn sidebar_queue_double_click_plays_without_leaving_the_browsing_page() {
    let (mut app, driver, requests) = setup();
    app.query = "retained query".into();
    app.selected = 1;
    let (_, mouse) = draw(&mut app, 120, 36, "sidebar-queue");
    let point = locate(&mouse, 120, 36, MouseAction::SelectPageRow(2));
    let mut pointer = Pointer::default();
    let start = Instant::now();
    for (delay, kind) in [(0, MouseEventKind::Down(MouseButton::Left)),
        (20, MouseEventKind::Up(MouseButton::Left)),
        (100, MouseEventKind::Down(MouseButton::Left)),
        (120, MouseEventKind::Up(MouseButton::Left))]
    {
        pointer.handle_at(&mut app, &mouse, event(kind, point), start + Duration::from_millis(delay)).unwrap();
    }
    assert_eq!(app.now.as_ref().unwrap().video_id, "song-2");
    assert_eq!(app.view, View::Tracks);
    assert_eq!(app.query, "retained query");
    assert_eq!(app.selected, 1);
    assert_eq!(driver.commands.try_iter().filter(|cmd| matches!(cmd, crate::player::Command::Load { .. })).count(), 1);
    assert_eq!(requests.try_iter().filter(|req| matches!(req, Request::Resolve { .. })).count(), 1);
}

#[test]
fn expanded_player_keeps_three_tabs_and_independent_lyrics_visible() {
    let (mut app, _, _) = setup();
    app.view = View::Playing;
    app.now.as_mut().unwrap().cursor[Tab::UpNext.index()] = 2;
    for tab in [Tab::UpNext, Tab::Related, Tab::Comments, Tab::Lyrics] {
        app.now.as_mut().unwrap().open(tab);
        let (terminal, mouse) = draw(&mut app, 120, 36, "expanded-player");
        for tab in [Tab::UpNext, Tab::Related, Tab::Comments] {
            locate(&mouse, 120, 36, MouseAction::OpenTab(tab));
        }
        assert!((0..36).any(|y| (0..120).any(|x| mouse.player_panel_at(x, y) == Some(Tab::Lyrics))));
        let text: String = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect();
        assert!(text.contains("Lyric line 0"));
    }
}

#[test]
fn opening_the_expanded_player_fetches_lyrics_without_switching_the_detail_tab() {
    let (mut app, _, requests) = setup();
    let now = app.now.as_mut().unwrap();
    now.lyrics = Panel::Idle;
    now.watched = true;
    app.tick_page();
    assert!(requests.try_recv().is_err(), "a hidden sidebar tab remains lazy");
    app.handle_mouse_action(MouseAction::OpenPlayer).unwrap();
    app.tick_page();
    let lyrics = requests.try_iter().filter(|req| matches!(req, Request::Lyrics { .. })).count();
    assert_eq!(lyrics, 1);
    assert_eq!(app.now.as_ref().unwrap().tab, Tab::UpNext);
}

#[test]
fn expanded_related_panel_loads_even_when_lyrics_have_keyboard_focus() {
    let (mut app, _, requests) = setup();
    app.view = View::Playing;
    let now = app.now.as_mut().unwrap();
    now.open(Tab::Related);
    now.open(Tab::Lyrics);
    now.related_id = Some("related-page".into());
    app.tick_page();
    assert_eq!(requests.try_iter().filter(|req| matches!(req, Request::Related { .. })).count(), 1);
    assert_eq!(app.now.as_ref().unwrap().tab, Tab::Lyrics);
}

#[test]
fn clicking_the_visible_lyrics_follow_control_can_resume_and_pause_following() {
    let (mut app, _, _) = setup();
    app.view = View::Playing;
    let now = app.now.as_mut().unwrap();
    now.follow_lyrics = false;
    now.lyrics = Panel::Ready(Lyrics { text: "Words".into(), source: None,
        timed: vec![crate::source::watch::TimedLine { text: "Words".into(), start: Duration::ZERO }] });
    let mut pointer = Pointer::default();
    for following in [true, false] {
        let (_, mouse) = draw(&mut app, 120, 36, "expanded-follow");
        let point = locate(&mouse, 120, 36, MouseAction::ToggleLyricsFollow);
        pointer.handle(&mut app, &mouse, event(MouseEventKind::Down(MouseButton::Left), point)).unwrap();
        pointer.handle(&mut app, &mouse, event(MouseEventKind::Up(MouseButton::Left), point)).unwrap();
        assert_eq!(app.now.as_ref().unwrap().follow_lyrics, following);
    }
}

#[test]
#[ignore = "exports actual persistent-player terminal cells for visual inspection"]
fn preview_persistent_player() {
    let (mut app, driver, _) = setup();
    *driver.snapshot.lock().unwrap() = Snapshot { state: PlayState::Playing, ..Default::default() };
    app.cover = Some(crate::source::cover::Cover::from_rgb(32, 32,
        (0..32).flat_map(|y| (0..32).flat_map(move |x| [60 + x * 4, 80 + y * 3, 140])).collect()));
    for view in [View::Tracks, View::Playing] {
        app.view = view;
        for (width, height) in [(120, 36), (80, 24), (48, 18)] {
            draw(&mut app, width, height, &format!("persistent-{view:?}"));
        }
    }
}
