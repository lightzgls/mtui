//! User action sequences with controlled worker timing. No audio or network.
use crate::app::*;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::runtime::config;
use crate::runtime::config::{CoverStyle, ImageRenderer};
use crate::desktop::discord::Presence;
use crate::terminal::graphics::Graphics;
use crate::player::{Command, PlayState, Player, PlayerEvent, Snapshot, TestDriver};
use crate::source::{ArtistRef, BrowseEndpoint, StreamUrl, Track};
use crate::source::catalog::artist::{ArtistPage, ArtistSong};
use crate::source::cover::Cover;
use crate::source::catalog::home::{Card, Shelf, Target};
use crate::source::youtube::watch::{Lyrics, QueuePage, Watch};
use crate::source::worker::{Request, Response, SourceWorker};
use crate::desktop::tray::TrayCommand;

use crate::app::browse::artist::ArtistView;
use crate::app::artwork::{CoverSize, ImageSource};
use crate::app::controls::input::{MouseAction, POINTER_SCALE};
use crate::app::controls::menu::{MenuAction, MenuPage};
use crate::app::browse::navigation::{Mode, PageRequestKind, View};
use crate::app::playback::now_playing::{
    NowPlaying, Panel, QUEUE_AHEAD, QUEUE_BEHIND, QUEUE_MEMORY, RepeatMode, Tab,
};
use crate::app::playback::mute_transition;

pub(in crate::app) fn fixture() -> (App, TestDriver, Receiver<Request>) {
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

fn artist_preview(app: &mut App) {
    let artist = ArtistRef { name: "Artist".into(), endpoint: BrowseEndpoint::new("UCartist") };
    let mut view = ArtistView::loading(artist.clone());
    view.set_page(Ok(ArtistPage {
        artist, audience: None, description: None, art: None,
        top_songs: (0..5).map(|i| ArtistSong { track: track(i), plays: None }).collect(),
        songs_endpoint: Some(BrowseEndpoint::new("VLsongs")), songs_truncated: false,
        shelves: Vec::new(),
    }));
    app.artist = Some(view);
    app.view = View::Artist;
}

fn ux_preview(app: &mut App, name: &str) {
    if std::env::var_os("MTUI_PREVIEW_DIR").is_none() { return; }
    for (width, height) in [(120, 36), (48, 18)] {
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        let mut mouse = crate::ui::MouseMap::default();
        terminal.draw(|frame| crate::ui::render(frame, app, &mut mouse)).unwrap();
        crate::ui::preview_buffer(&format!("{name}-{width}x{height}"), terminal.backend().buffer());
    }
}

#[test]
fn play_all_fetches_the_full_artist_list_once_and_keeps_order_and_back_state() {
    let (mut app, _driver, requests) = fixture();
    artist_preview(&mut app);
    ux_preview(&mut app, "artist-preview");
    app.handle_mouse_action(MouseAction::MoreArtistSongs).unwrap();
    app.handle_mouse_action(MouseAction::PlayArtistSongs).unwrap();
    let request_id = match requests.try_recv().unwrap() {
        Request::ArtistSongs { request_id, endpoint, .. } => { assert_eq!(endpoint.browse_id, "VLsongs"); request_id }
        _ => panic!("expected the full artist list"),
    };
    assert!(requests.try_recv().is_err());
    app.apply(Response::ArtistSongs { request_id, browse_id: "UCartist".into(), page: Box::new(Ok(
        crate::source::catalog::collection::Page { details: Default::default(), tracks: (0..80).map(track).collect() }
    )) });
    assert_eq!(app.view, View::Artist);
    assert_eq!(app.now.as_ref().unwrap().queue.len(), 80);
    ux_preview(&mut app, "artist-play-all");
    for index in 0..80 {
        assert_eq!(app.now.as_ref().unwrap().video_id, format!("song-{index}"));
        assert!(app.now.as_ref().unwrap().finite_queue);
        requests.try_iter().for_each(drop);
        app.top_up_queue();
        assert!(requests.try_recv().is_err(), "a finite artist list must not start radio");
        if index < 79 { app.advance(1, false); }
    }
    app.handle_mouse_action(MouseAction::OpenPlayer).unwrap();
    assert_eq!(app.view, View::Playing);
    app.handle_mouse_action(MouseAction::GoBack).unwrap();
    assert_eq!(app.view, View::Artist);
    let view = app.artist.as_ref().unwrap();
    assert!(view.songs_expanded);
    assert_eq!(view.content().unwrap().top_songs.len(), 80);
    assert!(view.songs_request.is_none());
    ux_preview(&mut app, "artist-expanded");
}

#[test]
fn artist_more_failure_preserves_the_preview_and_late_answers_cannot_change_a_new_page() {
    let (mut app, _driver, requests) = fixture();
    artist_preview(&mut app);
    app.more_artist_songs(false);
    let request_id = match requests.try_recv().unwrap() { Request::ArtistSongs { request_id, .. } => request_id, _ => unreachable!() };
    app.apply(Response::ArtistSongs { request_id, browse_id: "UCartist".into(), page: Box::new(Err("offline".into())) });
    assert_eq!(app.artist.as_ref().unwrap().content().unwrap().top_songs.len(), 5);
    assert!(app.artist.as_ref().unwrap().songs_error.is_some());
    app.more_artist_songs(true);
    let retry_id = match requests.try_recv().unwrap() { Request::ArtistSongs { request_id, .. } => request_id, _ => unreachable!() };
    assert_ne!(request_id, retry_id);
    app.go_home();
    let before = app.status.clone();
    app.apply(Response::ArtistSongs { request_id: retry_id, browse_id: "UCartist".into(), page: Box::new(Ok(
        crate::source::catalog::collection::Page { details: Default::default(), tracks: vec![track(10)] }
    )) });
    assert_eq!(app.view, View::Home);
    assert_eq!(app.status, before);
    assert!(app.now.is_none());
}

#[test]
fn adding_to_a_finite_artist_queue_keeps_its_original_tail_until_the_collection_limit() {
    let mut now = NowPlaying::new(&track(0));
    now.queue = (0..80).map(track).collect();
    now.playing = Some(0);
    now.finite_queue = true;
    assert!(now.insert_user_track(track(999), false));
    assert_eq!(now.queue.len(), 81);
    assert!(now.queue.iter().any(|track| track.id == "song-79"));
    for i in 1000..1300 { now.insert_user_track(track(i), true); }
    assert_eq!(now.queue.len(), crate::source::catalog::collection::MAX_TRACKS);
    assert_eq!(now.queue[0].id, "song-0");
}

#[test]
fn editing_search_keeps_the_origin_visible_and_rejects_results_for_an_old_draft() {
    let (mut app, _driver, requests) = fixture();
    artist_preview(&mut app);
    app.query = "bead".into();
    app.begin_search();
    assert_eq!(app.view, View::Artist);
    for code in [KeyCode::Home, KeyCode::Right, KeyCode::Char('r'), KeyCode::Enter] {
        app.handle_key(KeyEvent::new(code, KeyModifiers::NONE)).unwrap();
    }
    let request_id = match requests.try_recv().unwrap() {
        Request::Search { request_id, query, .. } => { assert_eq!(query, "bread"); request_id }
        _ => unreachable!(),
    };
    app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)).unwrap();
    app.apply(Response::Results { request_id, tracks: Ok(Vec::new()) });
    assert_eq!(app.mode, Mode::Editing);
    assert_eq!(app.view, View::Artist);
    app.handle_mouse_action(MouseAction::GoBack).unwrap();
    assert_eq!(app.mode, Mode::Browse);
    assert_eq!(app.view, View::Artist);
    assert_eq!(app.artist.as_ref().unwrap().content().unwrap().top_songs.len(), 5);
}

#[test]
fn queue_selection_and_context_actions_do_not_start_another_song() {
    let (mut app, driver, requests) = fixture();
    queue(&mut app, RepeatMode::Off, 0);
    app.view = View::Playing;
    state(&driver, PlayState::Playing, Duration::ZERO);
    driver.commands.try_iter().for_each(drop);
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 36)).unwrap();
    let mut mouse = crate::ui::MouseMap::default();
    terminal.draw(|frame| crate::ui::render(frame, &mut app, &mut mouse)).unwrap();
    let select = (0..36).find_map(|y| (0..120).find_map(|x| (mouse.action_at(x,y) == Some(MouseAction::SelectPageRow(2))).then_some((x,y)))).unwrap();
    app.handle_mouse_action(mouse.action_at(select.0,select.1).unwrap()).unwrap();
    terminal.draw(|frame| crate::ui::render(frame, &mut app, &mut mouse)).unwrap();
    for (code, modifiers) in [(KeyCode::Up, KeyModifiers::ALT), (KeyCode::Char('d'), KeyModifiers::NONE)] {
        app.handle_mouse_action(MouseAction::OpenPageActions).unwrap();
        assert!(app.menu.is_some());
        app.handle_key(KeyEvent::new(code, modifiers)).unwrap();
        assert!(app.menu.is_none());
        terminal.draw(|frame| crate::ui::render(frame, &mut app, &mut mouse)).unwrap();
    }
    assert_eq!(app.now.as_ref().unwrap().video_id, "song-0");
    assert!(!app.now.as_ref().unwrap().queue.iter().any(|track| track.id == "song-2"));
    assert!(driver.commands.try_iter().next().is_none());
    assert!(!requests.try_iter().any(|request| matches!(request, Request::Resolve { .. })));
    app.status = "Added a song to your queue".into();
    terminal.draw(|frame| crate::ui::render(frame, &mut app, &mut mouse)).unwrap();
    let text: String = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect();
    assert!(text.contains("Added a song to your queue"));
}

#[test]
fn moving_a_queue_song_changes_its_visible_row_instead_of_recentering_it() {
    let (mut app, _driver, _requests) = fixture();
    queue(&mut app, RepeatMode::Off, 0);
    app.now.as_mut().unwrap().queue = (0..30).map(track).collect();
    app.now.as_mut().unwrap().cursor[Tab::UpNext.index()] = 7;
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 36)).unwrap();
    let mut mouse = crate::ui::MouseMap::default();
    let row_for = |mouse: &crate::ui::MouseMap, index| (0..36).find(|y|
        (0..120).any(|x| mouse.action_at(x,*y) == Some(MouseAction::SelectPageRow(index)))).unwrap();
    terminal.draw(|frame| crate::ui::render(frame, &mut app, &mut mouse)).unwrap();
    let before = row_for(&mouse, 7);
    app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT)).unwrap();
    terminal.draw(|frame| crate::ui::render(frame, &mut app, &mut mouse)).unwrap();
    assert_eq!(app.now.as_ref().unwrap().queue[6].id, "song-7");
    assert!(row_for(&mouse, 6) < before, "moving up must visibly move the selected song upward");
}

#[test]
fn real_pointer_click_drag_release_and_keyboard_reorder_work_at_normal_and_short_heights() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    for (width, height) in [(120,36), (80,24), (48,18)] {
        let (mut app, driver, requests) = fixture();
        queue(&mut app, RepeatMode::Off, 0);
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width,height)).unwrap();
        let mut mouse = crate::ui::MouseMap::default();
        let mut pointer = crate::terminal::pointer::Pointer::default();
        let point = |mouse: &crate::ui::MouseMap, action| (0..height).find_map(|y|
            (0..width).find_map(|x| (mouse.action_at(x,y) == Some(action)).then_some((x,y)))).unwrap();
        let event = |kind, (column,row)| MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE };
        let mut draw = |app: &mut App, mouse: &mut crate::ui::MouseMap| {
            terminal.draw(|frame| crate::ui::render(frame, app, mouse)).unwrap();
        };
        draw(&mut app,&mut mouse);
        // Navigate to a future row even when only two songs fit in the short window.
        for _ in 0..2 { app.handle_key(KeyEvent::new(KeyCode::Down,KeyModifiers::NONE)).unwrap(); }
        draw(&mut app,&mut mouse);
        let title = point(&mouse,MouseAction::SelectPageRow(2));
        pointer.handle(&mut app,&mouse,event(MouseEventKind::Down(MouseButton::Left),title)).unwrap();
        draw(&mut app,&mut mouse);
        pointer.handle(&mut app,&mouse,event(MouseEventKind::Up(MouseButton::Left),title)).unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Up,KeyModifiers::ALT)).unwrap();
        assert_eq!(app.now.as_ref().unwrap().queue[1].id,"song-2");
        app.handle_key(KeyEvent::new(KeyCode::Down,KeyModifiers::ALT)).unwrap();
        draw(&mut app,&mut mouse);
        assert_eq!(app.now.as_ref().unwrap().queue[2].id,"song-2");
        app.handle_key(KeyEvent::new(KeyCode::Up,KeyModifiers::SHIFT)).unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('J'),KeyModifiers::SHIFT)).unwrap();
        draw(&mut app,&mut mouse);
        assert_eq!(app.now.as_ref().unwrap().queue[2].id,"song-2");
        // Drag the selected row downward using actual rendered hit targets.
        let source = point(&mouse,MouseAction::SelectPageRow(2));
        pointer.handle(&mut app,&mouse,event(MouseEventKind::Down(MouseButton::Left),source)).unwrap();
        draw(&mut app,&mut mouse);
        pointer.handle(&mut app,&mouse,event(MouseEventKind::ScrollDown,source)).unwrap();
        draw(&mut app,&mut mouse);
        let destination = point(&mouse,MouseAction::SelectPageRow(3));
        pointer.handle(&mut app,&mouse,event(MouseEventKind::Drag(MouseButton::Left),destination)).unwrap();
        draw(&mut app,&mut mouse);
        pointer.handle(&mut app,&mouse,event(MouseEventKind::Up(MouseButton::Left),destination)).unwrap();
        draw(&mut app,&mut mouse);
        assert_eq!(app.now.as_ref().unwrap().queue[3].id,"song-2");
        let before: Vec<_> = app.now.as_ref().unwrap().queue.iter().map(|t|t.id.clone()).collect();
        let other = point(&mouse,MouseAction::SelectPageRow(2));
        pointer.handle(&mut app,&mouse,event(MouseEventKind::Drag(MouseButton::Left),other)).unwrap();
        assert_eq!(app.now.as_ref().unwrap().queue.iter().map(|t|t.id.clone()).collect::<Vec<_>>(),before,
            "a released mouse must not keep moving the old selection");
        assert_eq!(app.now.as_ref().unwrap().video_id,"song-0");
        assert!(driver.commands.try_iter().next().is_none());
        assert!(!requests.try_iter().any(|request|matches!(request,Request::Resolve{..})));
    }
}

#[test]
fn a_long_queue_drag_preserves_other_song_order_and_rejects_stale_or_past_targets() {
    let (mut app,_driver,_requests) = fixture();
    queue(&mut app,RepeatMode::Off,1);
    app.now.as_mut().unwrap().queue = (0..8).map(track).collect();
    let (id,epoch) = app.queue_drag_identity(6).unwrap();
    app.drag_queue_song(&id,epoch,2);
    assert_eq!(app.now.as_ref().unwrap().queue.iter().map(|t|t.id.as_str()).collect::<Vec<_>>(),
        ["song-0","song-1","song-6","song-2","song-3","song-4","song-5","song-7"]);
    app.drag_queue_song(&id,epoch,0);
    assert_eq!(app.now.as_ref().unwrap().queue[2].id,"song-6");
    app.now.as_mut().unwrap().queue_epoch+=1;
    app.drag_queue_song(&id,epoch,4);
    assert_eq!(app.now.as_ref().unwrap().queue[2].id,"song-6");
    assert_eq!(app.now.as_ref().unwrap().video_id,"song-1");
}

#[test]
fn queue_drags_cannot_seek_and_progress_drags_cannot_reorder_the_queue() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    let (mut app,driver,_requests) = fixture();
    queue(&mut app,RepeatMode::Off,0);
    state(&driver,PlayState::Playing,Duration::ZERO);
    driver.commands.try_iter().for_each(drop);
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120,36)).unwrap();
    let mut mouse = crate::ui::MouseMap::default();
    let mut pointer = crate::terminal::pointer::Pointer::default();
    let event = |kind,(column,row)| MouseEvent { kind,column,row,modifiers:KeyModifiers::NONE };
    terminal.draw(|frame|crate::ui::render(frame,&mut app,&mut mouse)).unwrap();
    let source = (0..36).find_map(|y|(0..120).find_map(|x|
        (mouse.action_at(x,y)==Some(MouseAction::SelectPageRow(2))).then_some((x,y)))).unwrap();
    let progress = (0..36).find_map(|y|(0..120).find_map(|x|
        matches!(mouse.action_at(x,y),Some(MouseAction::SeekTo(_))).then_some((x,y)))).unwrap();
    pointer.handle(&mut app,&mouse,event(MouseEventKind::Down(MouseButton::Left),source)).unwrap();
    pointer.handle(&mut app,&mouse,event(MouseEventKind::Drag(MouseButton::Left),progress)).unwrap();
    pointer.handle(&mut app,&mouse,event(MouseEventKind::Up(MouseButton::Left),progress)).unwrap();
    assert!(driver.commands.try_recv().is_err());
    pointer.handle(&mut app,&mouse,event(MouseEventKind::Down(MouseButton::Left),progress)).unwrap();
    assert!(matches!(driver.commands.try_recv().unwrap(),Command::Seek(_)));
    pointer.handle(&mut app,&mouse,event(MouseEventKind::Up(MouseButton::Left),progress)).unwrap();
    assert!(driver.commands.try_recv().is_err(),"releasing a seek click must not issue the same seek twice");
    pointer.handle(&mut app,&mouse,event(MouseEventKind::Down(MouseButton::Left),progress)).unwrap();
    assert!(matches!(driver.commands.try_recv().unwrap(),Command::Seek(_)));
    pointer.handle(&mut app,&mouse,event(MouseEventKind::Drag(MouseButton::Left),source)).unwrap();
    pointer.handle(&mut app,&mouse,event(MouseEventKind::Up(MouseButton::Left),source)).unwrap();
    assert!(driver.commands.try_recv().is_err());
    assert_eq!(app.now.as_ref().unwrap().queue.iter().map(|t|t.id.as_str()).collect::<Vec<_>>(),
        ["song-0","song-1","song-2","song-3"]);
}

#[test]
fn the_queue_menu_honors_its_displayed_alt_arrow_move_shortcut() {
    let (mut app,driver,_requests) = fixture();
    queue(&mut app,RepeatMode::Off,0);
    app.now.as_mut().unwrap().cursor[Tab::UpNext.index()] = 2;
    app.open_menu(MenuPage::PageActions);
    app.handle_key(KeyEvent::new(KeyCode::Up,KeyModifiers::ALT)).unwrap();
    assert!(app.menu.is_none());
    assert_eq!(app.now.as_ref().unwrap().queue[1].id,"song-2");
    assert!(driver.commands.try_recv().is_err());
}

fn song_card(index: usize) -> Card {
    Card { title: format!("Song {index}"), subtitle: "Song • Artist".into(), art: None,
        duration: Some(Duration::from_secs(240)), artist_ref: None,
        target: Target::Play { video_id: format!("song-{index}") } }
}

#[test]
fn inline_artist_links_open_the_named_artist_and_back_restores_the_scrolled_origin() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    use crate::source::catalog::library::browse::{Category, Item};
    use ratatui::style::Modifier;
    for surface in ["playlist", "library songs", "library albums", "related"] {
        for (width, height) in [(120, 36), (80, 24), (48, 24)] {
            let (mut app, driver, requests) = fixture();
            queue(&mut app, RepeatMode::Off, 0);
            state(&driver, PlayState::Playing, Duration::from_secs(35));
            let artist = ArtistRef { name: "Artist Nine".into(), endpoint: BrowseEndpoint::new("UCnine") };
            let linked_track = |index| Track { uploader: artist.name.clone(),
                artist_ref: (index == 24).then(|| artist.clone()), album: Some("Evening Album".into()), ..track(index) };
            let linked_card = |index| Card { subtitle: format!("Song • {} • Evening Album", artist.name),
                artist_ref: (index == 24).then(|| artist.clone()), ..song_card(index) };
            let (origin, link_action, row_action, context_action) = match surface {
                "playlist" => {
                    app.view = View::Tracks;
                    app.browsing = Some("Evening Playlist".into());
                    app.results = (0..30).map(linked_track).collect();
                    app.selected = 24;
                    app.offset = 24;
                    (View::Tracks, MouseAction::OpenTrackArtist(24), MouseAction::PlayTrack(24), MouseAction::SelectTrack(24))
                }
                "library songs" | "library albums" => {
                    app.view = View::Library;
                    app.library.connected = true;
                    app.library.category = if surface == "library songs" { Category::Songs } else { Category::Albums };
                    let collection = app.library.collection_mut();
                    collection.loaded = true;
                    collection.selected = 24;
                    collection.offset = 24;
                    collection.items = (0..30).map(|index| {
                        let mut card = linked_card(index);
                        let track = if surface == "library songs" { Some(linked_track(index)) } else {
                            card.subtitle = format!("Album • {}", artist.name);
                            card.target = Target::Open { endpoint: BrowseEndpoint::new(format!("MPRE{index}")) };
                            None
                        };
                        Item { card, track, available: true }
                    }).collect();
                    (View::Library, MouseAction::OpenLibraryArtist(24), MouseAction::OpenLibraryItem(24), MouseAction::SelectLibraryItem(24))
                }
                "related" => {
                    let now = app.now.as_mut().unwrap();
                    now.tab = Tab::Related;
                    now.related = Panel::Ready(vec![Shelf { title: "Related songs".into(), cards: (0..30).map(linked_card).collect() }]);
                    now.cursor[Tab::Related.index()] = 25;
                    (View::Playing, MouseAction::OpenRelatedArtist(25), MouseAction::OpenPageRow(25), MouseAction::SelectPageRow(25))
                }
                _ => unreachable!(),
            };
            let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            let mut mouse = crate::ui::MouseMap::default();
            terminal.draw(|frame| crate::ui::render(frame, &mut app, &mut mouse)).unwrap();
            let point = |action| (0..height).find_map(|y| (0..width).find_map(|x|
                (mouse.action_at(x, y) == Some(action)).then_some((x, y))))
                .unwrap_or_else(|| panic!("missing {action:?} on {surface} at {width}x{height}"));
            let link = point(link_action);
            let title = point(row_action);
            let buffer = terminal.backend().buffer();
            for (offset, glyph) in artist.name.chars().enumerate() {
                let x = link.0 + offset as u16;
                assert_eq!(buffer[(x, link.1)].symbol(), glyph.to_string());
                assert!(buffer[(x, link.1)].modifier.contains(Modifier::UNDERLINED));
                assert_eq!(mouse.action_at(x, link.1), Some(link_action));
                assert_eq!(mouse.context_action_at(x, link.1), Some(context_action));
            }
            assert_ne!(mouse.action_at(link.0 + artist.name.len() as u16, link.1), Some(link_action));
            assert!(!buffer[(link.0, link.1 - 1)].modifier.contains(Modifier::UNDERLINED));
            let offset = if origin == View::Library { app.library.collection().offset } else { app.offset };
            let queue_before: Vec<_> = app.now.as_ref().unwrap().queue.iter().map(|track| track.id.clone()).collect();
            requests.try_iter().for_each(drop);
            driver.commands.try_iter().for_each(drop);
            let mut pointer = crate::terminal::pointer::Pointer::default();
            let event = |kind, (column, row)| MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE };
            // Crossing a link during a song drag must not follow it. Album
            // titles retain their existing single-click navigation behavior.
            if surface != "library albums" {
                pointer.handle(&mut app, &mouse, event(MouseEventKind::Down(MouseButton::Left), title)).unwrap();
                pointer.handle(&mut app, &mouse, event(MouseEventKind::Drag(MouseButton::Left), link)).unwrap();
                pointer.handle(&mut app, &mouse, event(MouseEventKind::Up(MouseButton::Left), link)).unwrap();
            }
            assert_eq!(app.view, origin);
            assert!(requests.try_recv().is_err());
            // Right-click keeps the song/album menu, including on linked text.
            pointer.handle(&mut app, &mouse, event(MouseEventKind::Down(MouseButton::Right), link)).unwrap();
            assert!(app.menu.is_some());
            assert_eq!(app.view, origin);
            app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)).unwrap();
            pointer.handle(&mut app, &mouse, event(MouseEventKind::Down(MouseButton::Left), link)).unwrap();
            pointer.handle(&mut app, &mouse, event(MouseEventKind::Up(MouseButton::Left), link)).unwrap();
            assert_eq!(app.view, View::Artist);
            let request_id = match requests.try_recv().unwrap() {
                Request::OpenArtist { request_id, artist: requested } => { assert_eq!(requested, artist); request_id }
                _ => panic!("expected the linked artist route"),
            };
            assert!(requests.try_recv().is_err());
            app.handle_mouse_action(MouseAction::GoBack).unwrap();
            assert_eq!(app.view, origin);
            match origin {
                View::Tracks => { assert_eq!(app.selected, 24); assert_eq!(app.offset, offset); assert_eq!(app.browsing.as_deref(), Some("Evening Playlist")); }
                View::Library => { assert_eq!(app.library.collection().selected, 24); assert_eq!(app.library.collection().offset, offset); }
                View::Playing => { assert_eq!(app.now.as_ref().unwrap().tab, Tab::Related); assert_eq!(app.now.as_ref().unwrap().cursor(), 25); }
                _ => unreachable!(),
            }
            let status = app.status.clone();
            app.apply(Response::Artist { request_id, browse_id: "UCnine".into(), page: Box::new(Err("late failure".into())) });
            assert_eq!(app.view, origin);
            assert_eq!(app.status, status);
            assert!(driver.commands.try_recv().is_err());
            assert_eq!(app.now.as_ref().unwrap().video_id, "song-0");
            assert_eq!(app.now.as_ref().unwrap().queue.iter().map(|track| track.id.clone()).collect::<Vec<_>>(), queue_before);
        }
    }
}

#[test]
fn inline_artist_links_require_visible_metadata_and_a_real_route() {
    for (width, height, name) in [(120, 36, "unlinked"), (48, 24, "Artist ".repeat(30).as_str()), (48, 12, "Artist")] {
        let (mut app, driver, requests) = fixture();
        let mut song = track(9);
        song.uploader = name.into();
        song.artist_ref = (name != "unlinked").then(|| ArtistRef { name: name.into(), endpoint: BrowseEndpoint::new("UCnine") });
        app.browsing = Some("Playlist".into());
        app.results = vec![song];
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        let mut mouse = crate::ui::MouseMap::default();
        terminal.draw(|frame| crate::ui::render(frame, &mut app, &mut mouse)).unwrap();
        assert!(!(0..height).any(|y| (0..width).any(|x| matches!(mouse.action_at(x, y), Some(MouseAction::OpenTrackArtist(_))))));
        for action in [MouseAction::OpenTrackArtist(usize::MAX), MouseAction::OpenLibraryArtist(0), MouseAction::OpenRelatedArtist(1)] {
            app.handle_mouse_action(action).unwrap();
        }
        assert_eq!(app.view, View::Tracks);
        assert!(driver.commands.try_recv().is_err());
        assert!(!requests.try_iter().any(|request| matches!(request, Request::OpenArtist { .. } | Request::Resolve { .. })));
    }
}

#[test]
fn loaded_pages_and_playback_keep_identity_in_one_place_and_preserve_failure_feedback() {
    let (mut app, driver, requests) = fixture();
    app.play_track(track(0), false);
    resolve(&mut app, 0);
    assert!(app.status.is_empty());
    state(&driver, PlayState::Playing, Duration::from_secs(35));
    app.now.as_mut().unwrap().tab = Tab::Lyrics;
    app.now.as_mut().unwrap().lyrics = Panel::Empty("No lyrics available".into());
    app.cover = Some(Cover::solid(32, 32));
    for view in [View::Playing, View::Home] {
        app.view = view;
        for (width, height) in [(120, 36), (48, 24)] {
            let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            let mut mouse = crate::ui::MouseMap::default();
            terminal.draw(|frame| crate::ui::render(frame, &mut app, &mut mouse)).unwrap();
            let text: String = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect();
            assert_eq!(text.matches("Song 0").count(), 1, "{view:?} at {width}x{height}");
            if view == View::Playing { assert!(!text.contains("NOW PLAYING")); }
            assert!(!text.contains("playing Song 0"));
        }
    }
    app.view = View::Playing;
    for (playback, label) in [(PlayState::Paused, "PAUSED"), (PlayState::Buffering, "LOADING…")] {
        state(&driver, playback, Duration::from_secs(35));
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 36)).unwrap();
        terminal.draw(|frame| crate::ui::render(frame, &mut app, &mut Default::default())).unwrap();
        let text: String = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect();
        assert!(text.contains(label), "{label} must remain visible");
    }
    artist_preview(&mut app);
    let page = app.artist.as_ref().unwrap().content().unwrap().clone();
    app.view = View::Home;
    app.open_artist(page.artist.clone());
    let request_id = app.pending_page_request.unwrap().0;
    app.apply(Response::Artist { request_id, browse_id: "UCartist".into(), page: Box::new(Ok(page)) });
    assert!(app.status.is_empty(), "the artist page already shows its songs and shelves");
    app.reload_artist();
    let request_id = app.pending_page_request.unwrap().0;
    app.apply(Response::Artist { request_id, browse_id: "UCartist".into(), page: Box::new(Err("Artist lookup failed".into())) });
    assert!(app.status.contains("Artist lookup failed"));
    app.go_back();
    app.play_track(track(1), false);
    app.apply(Response::Resolved { id: "song-1".into(), title: "Song 1".into(), stream: Err("Audio connection failed".into()) });
    assert!(app.status.contains("network interrupted playback"));
    assert!(app.status.contains("r retry"));
    requests.try_iter().for_each(drop);
    ux_preview(&mut app, "clean-feedback");
}

#[test]
fn key_hints_change_labels_without_hiding_controls_or_disabling_shortcuts() {
    for show in [true, false] {
        let (mut app, driver, requests) = fixture();
        queue(&mut app, RepeatMode::Off, 0);
        app.show_key_hints = show;
        app.now.as_mut().unwrap().queue_title = "Evening Mix".into();
        app.now.as_mut().unwrap().cursor[Tab::UpNext.index()] = 2;
        app.cover = Some(Cover::solid(32, 32));
        state(&driver, PlayState::Playing, Duration::from_secs(35));
        for (width, height) in [(160, 42), (120, 36), (80, 24), (48, 18)] {
            let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            let mut mouse = crate::ui::MouseMap::default();
            terminal.draw(|frame| crate::ui::render(frame, &mut app, &mut mouse)).unwrap();
            let text: String = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect();
            assert!(!text.contains("3 up next"));
            assert!(!text.contains("up next · Repeat"));
            if width >= 120 {
                for key in ["Space", "^L", "^Y", "^A", "^O", "^P"] {
                    assert_eq!(text.contains(key), show, "{key} at {width}x{height}");
                }
            }
            assert!(!text.contains(" Up "));
            assert!(!text.contains(" Down "));
            assert!(!text.contains(" Remove "));
            assert!((0..height).any(|y| (0..width).any(|x| mouse.action_at(x, y) == Some(MouseAction::OpenPlayerActions))),
                "missing player actions at {width}x{height}");
        }
        ux_preview(&mut app, if show { "key-hints-on" } else { "key-hints-off" });
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT)).unwrap();
        assert_eq!(app.now.as_ref().unwrap().queue[1].id, "song-2");
        app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL)).unwrap();
        assert!(matches!(app.overlay, Overlay::Share { .. }));
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)).unwrap();
        app.open_menu(MenuPage::Root);
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 36)).unwrap();
        let mut mouse = crate::ui::MouseMap::default();
        terminal.draw(|frame| crate::ui::render(frame, &mut app, &mut mouse)).unwrap();
        let text: String = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect();
        assert_eq!(text.contains("Ctrl+S"), show);
        app.open_menu(MenuPage::Help);
        terminal.draw(|frame| crate::ui::render(frame, &mut app, &mut mouse)).unwrap();
        let text: String = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect();
        assert!(text.contains("Enter"), "Help must remain a complete keyboard reference");
        app.menu = None;
        app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL)).unwrap();
        assert!(matches!(app.overlay, Overlay::Settings));
        assert_eq!(app.preferences().selected_setting(), preferences::Setting::Output);
        app.preferences.picker = None;
        app.preferences.selected = preferences::Setting::KeyHints.index();
        ux_preview(&mut app, if show { "settings-key-hints-on" } else { "settings-key-hints-off" });
        assert!(driver.commands.try_recv().is_err());
        assert!(!requests.try_iter().any(|request| matches!(request, Request::Resolve { .. })));
    }
}

#[test]
fn player_button_shortcuts_leave_search_text_entry_and_modals_in_control() {
    let (mut app, driver, requests) = fixture();
    queue(&mut app, RepeatMode::Off, 0);
    app.begin_search();
    app.query = "unchanged draft".into();
    app.query_cursor = app.query.len();
    app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)).unwrap();
    assert_eq!(app.query_cursor, 0);
    assert_eq!(app.mode, Mode::Editing);
    assert!(!app.overlay.is_open());
    app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL)).unwrap();
    assert!(!app.overlay.is_open());
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)).unwrap();
    app.handle_mouse_action(MouseAction::SharePlaying).unwrap();
    app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL)).unwrap();
    assert!(matches!(app.overlay, Overlay::Share { .. }), "a modal keeps keyboard ownership");
    assert!(driver.commands.try_recv().is_err());
    assert!(requests.try_recv().is_err());
}

#[test]
fn song_clicks_select_then_double_click_enter_or_context_menu_works_across_pages() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    for activation in ["double-click", "Enter", "context menu"] {
        for view in [View::Home, View::Tracks, View::Artist, View::Library, View::Playing] {
            let (mut app, driver, requests) = fixture();
            queue(&mut app, RepeatMode::Off, 0);
            let action = match view {
                View::Home => {
                    app.home = vec![Shelf { title: "Songs".into(), cards: vec![song_card(9)] }];
                    MouseAction::OpenHomeCard { shelf: 0, card: 0 }
                }
                View::Tracks => {
                    app.results = vec![track(9)];
                    app.browsing = Some("Playlist".into());
                    MouseAction::PlayTrack(0)
                }
                View::Artist => {
                    artist_preview(&mut app);
                    if let Panel::Ready(page) = &mut app.artist.as_mut().unwrap().page {
                        page.top_songs = vec![ArtistSong { track: track(9), plays: None }];
                    }
                    MouseAction::OpenArtistSong(0)
                }
                View::Library => {
                    app.library.connected = true;
                    app.library.collection_mut().items = vec![crate::source::catalog::library::browse::Item {
                        card: song_card(9), track: Some(track(9)), available: true,
                    }];
                    MouseAction::OpenLibraryItem(0)
                }
                View::Playing => {
                    let now = app.now.as_mut().unwrap();
                    now.tab = Tab::Related;
                    now.related = Panel::Ready(vec![Shelf { title: "Related".into(), cards: vec![song_card(9)] }]);
                    MouseAction::OpenPageRow(1)
                }
            };
            app.view = view;
            if activation == "Enter" {
                app.begin_search();
                app.query.push_str("unfinished draft");
            }
            let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 36)).unwrap();
            let mut mouse = crate::ui::MouseMap::default();
            terminal.draw(|frame| crate::ui::render(frame, &mut app, &mut mouse)).unwrap();
            let (column, row) = (0..36).find_map(|y| (0..120).find_map(|x|
                (mouse.action_at(x, y) == Some(action)).then_some((x, y)))).unwrap();
            let mut pointer = crate::terminal::pointer::Pointer::default();
            let start = Instant::now();
            for (ms, kind) in [(0, MouseEventKind::Down(MouseButton::Left)), (40, MouseEventKind::Up(MouseButton::Left))] {
                pointer.handle_at(&mut app, &mouse, MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE },
                    start + Duration::from_millis(ms)).unwrap();
            }
            assert_eq!(app.view, view, "a single song click must keep its page open");
            assert_eq!(app.mode, Mode::Browse, "clicking a song moves focus out of text entry");
            assert_eq!(app.selected_queue_track().unwrap().id, "song-9");
            assert_eq!(app.now.as_ref().unwrap().video_id, "song-0");
            assert!(driver.commands.try_recv().is_err());
            assert!(!requests.try_iter().any(|request| matches!(request, Request::Resolve { .. })));
            terminal.draw(|frame| crate::ui::render(frame, &mut app, &mut mouse)).unwrap();
            if activation == "context menu" {
                pointer.handle_at(&mut app, &mouse, MouseEvent { kind: MouseEventKind::Down(MouseButton::Right),
                    column, row, modifiers: KeyModifiers::NONE }, start + Duration::from_millis(120)).unwrap();
                assert!(app.menu.is_some());
                assert_eq!(app.selected_queue_track().unwrap().id, "song-9");
                assert_eq!(app.now.as_ref().unwrap().video_id, "song-0");
                assert!(driver.commands.try_recv().is_err());
                assert!(!requests.try_iter().any(|request| matches!(request, Request::Resolve { .. })));
            } else {
                if activation == "Enter" {
                    pointer.cancel();
                    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)).unwrap();
                } else {
                    for (ms, kind) in [(120, MouseEventKind::Down(MouseButton::Left)), (160, MouseEventKind::Up(MouseButton::Left))] {
                        pointer.handle_at(&mut app, &mouse, MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE },
                            start + Duration::from_millis(ms)).unwrap();
                    }
                }
                assert_eq!(app.view, view);
                assert_eq!(app.now.as_ref().unwrap().video_id, "song-9");
                assert_eq!(requests.try_iter().filter(|request| matches!(request, Request::Resolve { id, .. } if id == "song-9")).count(), 1);
            }
        }
    }
}

#[test]
fn mixed_search_artist_song_cards_and_queue_titles_share_double_click_playback() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    use crate::source::catalog::search::{Filter, Item};
    for surface in ["search", "artist shelf", "queue"] {
        let (mut app, _driver, requests) = fixture();
        queue(&mut app, RepeatMode::Off, 0);
        let action = match surface {
            "search" => {
                app.view = View::Tracks;
                app.query = "evening".into();
                let mut album = song_card(20);
                album.target = Target::Open { endpoint: BrowseEndpoint::new("MPREalbum") };
                app.search_items = vec![
                    Item { kind: Filter::Albums, card: album, track: None },
                    Item { kind: Filter::Songs, card: song_card(9), track: Some(track(9)) },
                ];
                app.results = vec![track(9)];
                MouseAction::PlayTrack(1)
            }
            "artist shelf" => {
                artist_preview(&mut app);
                if let Panel::Ready(page) = &mut app.artist.as_mut().unwrap().page {
                    page.top_songs.clear();
                    page.shelves = vec![Shelf { title: "Songs".into(), cards: vec![song_card(9)] }];
                }
                MouseAction::OpenArtistCard { shelf: 0, card: 0 }
            }
            "queue" => {
                let now = app.now.as_mut().unwrap();
                now.queue[2] = track(9);
                now.cursor[Tab::UpNext.index()] = 2;
                MouseAction::SelectPageRow(2)
            }
            _ => unreachable!(),
        };
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 36)).unwrap();
        let mut mouse = crate::ui::MouseMap::default();
        terminal.draw(|frame| crate::ui::render(frame, &mut app, &mut mouse)).unwrap();
        let (column, row) = (0..36).find_map(|y| (0..120).find_map(|x|
            (mouse.action_at(x, y) == Some(action)).then_some((x, y)))).unwrap();
        let mut pointer = crate::terminal::pointer::Pointer::default();
        let start = Instant::now();
        for ms in [0, 120, 240] {
            for (delay, kind) in [(0, MouseEventKind::Down(MouseButton::Left)), (30, MouseEventKind::Up(MouseButton::Left))] {
                pointer.handle_at(&mut app, &mouse, MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE },
                    start + Duration::from_millis(ms + delay)).unwrap();
            }
            if ms == 0 {
                assert_eq!(app.now.as_ref().unwrap().video_id, "song-0");
                assert!(!requests.try_iter().any(|request| matches!(request, Request::Resolve { .. })));
            } else if ms == 120 {
                assert_eq!(app.now.as_ref().unwrap().video_id, "song-9", "{surface}");
                assert_eq!(requests.try_iter().filter(|request| matches!(request, Request::Resolve { id, .. } if id == "song-9")).count(), 1);
            } else {
                assert!(!requests.try_iter().any(|request| matches!(request, Request::Resolve { .. })), "a third click starts a new pair");
            }
            terminal.draw(|frame| crate::ui::render(frame, &mut app, &mut mouse)).unwrap();
        }
    }
}

#[test]
fn drags_timeouts_page_changes_and_replaced_songs_cannot_complete_an_old_double_click() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    for scenario in ["timeout", "different row", "replacement", "page change", "cancel", "drag", "context menu"] {
        let (mut app, driver, requests) = fixture();
        queue(&mut app, RepeatMode::Off, 0);
        app.view = View::Tracks;
        app.browsing = Some("Playlist".into());
        app.results = vec![track(9), track(10)];
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 36)).unwrap();
        let mut mouse = crate::ui::MouseMap::default();
        terminal.draw(|frame| crate::ui::render(frame, &mut app, &mut mouse)).unwrap();
        let point = |index| (0..36).find_map(|y| (0..120).find_map(|x|
            (mouse.action_at(x, y) == Some(MouseAction::PlayTrack(index))).then_some((x, y)))).unwrap();
        let first = point(0);
        let other = point(1);
        let mut pointer = crate::terminal::pointer::Pointer::default();
        let start = Instant::now();
        let event = |kind, (column, row)| MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE };
        pointer.handle_at(&mut app, &mouse, event(MouseEventKind::Down(MouseButton::Left), first), start).unwrap();
        if scenario == "drag" {
            pointer.handle_at(&mut app, &mouse, event(MouseEventKind::Drag(MouseButton::Left), other), start + Duration::from_millis(20)).unwrap();
        }
        pointer.handle_at(&mut app, &mouse, event(MouseEventKind::Up(MouseButton::Left), first), start + Duration::from_millis(40)).unwrap();
        match scenario {
            "replacement" => app.results[0] = track(11),
            "page change" => { app.begin_page_request(PageRequestKind::Browse); }
            "cancel" => pointer.cancel(),
            "context menu" => {
                pointer.handle_at(&mut app, &mouse, event(MouseEventKind::Down(MouseButton::Right), first), start + Duration::from_millis(60)).unwrap();
                app.handle_mouse_action(MouseAction::CloseMenu).unwrap();
            }
            _ => {}
        }
        let ms = if scenario == "timeout" { 800 } else { 120 };
        let position = if scenario == "different row" { other } else { first };
        pointer.handle_at(&mut app, &mouse, event(MouseEventKind::Down(MouseButton::Left), position), start + Duration::from_millis(ms)).unwrap();
        pointer.handle_at(&mut app, &mouse, event(MouseEventKind::Up(MouseButton::Left), position), start + Duration::from_millis(ms + 40)).unwrap();
        assert_eq!(app.now.as_ref().unwrap().video_id, "song-0", "{scenario}");
        assert!(driver.commands.try_recv().is_err(), "{scenario}");
        assert!(!requests.try_iter().any(|request| matches!(request, Request::Resolve { .. })), "{scenario}");
    }
}

#[test]
fn selected_song_menus_queue_the_correct_song_on_every_browsing_page_without_interrupting_playback() {
    use crate::source::catalog::search::{Filter, Item};
    for view in [View::Home, View::Tracks, View::Artist, View::Library, View::Playing] {
        for next in [true, false] {
            let (mut app, driver, requests) = fixture();
            queue(&mut app, RepeatMode::Off, 0);
            match view {
                View::Home => app.home = vec![Shelf { title: "Songs".into(), cards: vec![song_card(9)] }],
                View::Tracks => {
                    let mut album = song_card(20);
                    album.target = Target::Open { endpoint: BrowseEndpoint::new("MPREalbum") };
                    app.search_items = vec![
                        Item { kind: Filter::Albums, card: album, track: None },
                        Item { kind: Filter::Songs, card: song_card(9), track: Some(track(9)) },
                    ];
                    app.results = vec![track(9)];
                    app.selected = 1;
                }
                View::Artist => {
                    artist_preview(&mut app);
                    if let Panel::Ready(page) = &mut app.artist.as_mut().unwrap().page {
                        page.top_songs = vec![ArtistSong { track: track(9), plays: None }];
                    }
                }
                View::Library => {
                    app.library.connected = true;
                    app.library.collection_mut().items = vec![crate::source::catalog::library::browse::Item {
                        card: song_card(9), track: Some(track(9)), available: true,
                    }];
                }
                View::Playing => {
                    let now = app.now.as_mut().unwrap();
                    now.tab = Tab::Related;
                    now.related = Panel::Ready(vec![Shelf { title: "Related".into(), cards: vec![song_card(9)] }]);
                    now.cursor[Tab::Related.index()] = 1;
                }
            }
            app.view = view;
            app.handle_key(KeyEvent::new(KeyCode::Char('.'), KeyModifiers::NONE)).unwrap();
            let label = if next { "Play next" } else { "Add to queue" };
            let selected = app.menu_items().iter().position(|item| item.label == label && item.enabled)
                .unwrap_or_else(|| panic!("{view:?} must expose {label} for the selected song"));
            app.handle_mouse_action(MouseAction::ActivateMenuItem(selected)).unwrap();
            let now = app.now.as_ref().unwrap();
            assert_eq!(now.queue[if next { 1 } else { 4 }].id, "song-9");
            assert_eq!(now.queue.len(), 5);
            assert_eq!(now.video_id, "song-0");
            assert_eq!(now.playing, Some(0));
            assert!(app.menu.is_none());
            assert!(driver.commands.try_recv().is_err());
            assert!(requests.try_iter().all(|request| matches!(request, Request::Prefetch { id } if next && id == "song-9")));
        }
    }
}

#[test]
fn queue_actions_skip_nonplayable_and_unavailable_items_and_wait_for_an_active_queue() {
    use crate::source::catalog::search::{Filter, Item};
    let (mut app, driver, requests) = fixture();
    queue(&mut app, RepeatMode::Off, 0);
    let mut album = song_card(9);
    album.target = Target::Open { endpoint: BrowseEndpoint::new("MPREalbum") };
    let no_queue_actions = |app: &App| assert!(app.page_action_items().iter().all(|item|
        !matches!(item.action, Some(MenuAction::PlaySelectionNext | MenuAction::QueueSelection))));
    app.view = View::Tracks;
    app.search_items = vec![Item { kind: Filter::Albums, card: album.clone(), track: None }];
    no_queue_actions(&app);
    artist_preview(&mut app);
    let artist = app.artist.as_mut().unwrap();
    artist.section = 1;
    if let Panel::Ready(page) = &mut artist.page {
        page.shelves = vec![Shelf { title: "Albums".into(), cards: vec![album.clone()] }];
    }
    no_queue_actions(&app);
    app.view = View::Library;
    app.library.collection_mut().items = vec![crate::source::catalog::library::browse::Item {
        card: song_card(9), track: Some(track(9)), available: false,
    }];
    no_queue_actions(&app);
    app.library.collection_mut().items[0] = crate::source::catalog::library::browse::Item { card: album.clone(), track: None, available: true };
    no_queue_actions(&app);
    app.view = View::Playing;
    let now = app.now.as_mut().unwrap();
    now.tab = Tab::Related;
    now.related = Panel::Ready(vec![Shelf { title: "Albums".into(), cards: vec![album] }]);
    now.cursor[Tab::Related.index()] = 0;
    no_queue_actions(&app);
    app.now.as_mut().unwrap().cursor[Tab::Related.index()] = 1;
    no_queue_actions(&app);

    app.view = View::Tracks;
    app.search_items.clear();
    app.results = vec![track(9)];
    for loading in [true, false] {
        if loading { app.now.as_mut().unwrap().playing = None; } else { app.now = None; }
        app.open_menu(MenuPage::PageActions);
        for label in ["Play next", "Add to queue"] {
            let selected = app.menu_items().iter().position(|item| item.label == label && !item.enabled).unwrap();
            app.handle_mouse_action(MouseAction::ActivateMenuItem(selected)).unwrap();
            assert!(app.menu.is_some());
        }
    }
    assert!(driver.commands.try_recv().is_err());
    assert!(requests.try_recv().is_err());
}

#[test]
fn artist_and_library_song_cards_support_the_same_queue_actions_as_track_rows() {
    for view in [View::Artist, View::Library] {
        let (mut app, driver, _requests) = fixture();
        queue(&mut app, RepeatMode::Off, 0);
        if view == View::Artist {
            artist_preview(&mut app);
            let artist = app.artist.as_mut().unwrap();
            artist.section = 1;
            if let Panel::Ready(page) = &mut artist.page {
                page.shelves = vec![Shelf { title: "Songs".into(), cards: vec![song_card(9)] }];
            }
        } else {
            app.library.collection_mut().items = vec![crate::source::catalog::library::browse::Item {
                card: song_card(9), track: None, available: true,
            }];
        }
        app.view = view;
        app.queue_selected_track(false);
        assert_eq!(app.now.as_ref().unwrap().queue.last().unwrap().id, "song-9");
        assert!(driver.commands.try_recv().is_err());
    }
}

#[test]
fn play_next_promotes_an_existing_song_and_keeps_the_queue_cursor_on_its_song() {
    let (mut app, driver, requests) = fixture();
    queue(&mut app, RepeatMode::Off, 0);
    app.now.as_mut().unwrap().cursor[Tab::UpNext.index()] = 2;
    app.view = View::Tracks;
    app.results = vec![track(3), track(9)];
    app.queue_selected_track(true);
    let now = app.now.as_ref().unwrap();
    assert_eq!(now.queue.iter().map(|song| song.id.as_str()).collect::<Vec<_>>(),
        ["song-0", "song-3", "song-1", "song-2"]);
    assert_eq!(now.queue[now.cursor[Tab::UpNext.index()]].id, "song-2");
    app.selected = 1;
    app.queue_selected_track(true);
    let now = app.now.as_ref().unwrap();
    assert_eq!(now.queue[1].id, "song-9");
    assert_eq!(now.queue[now.cursor[Tab::UpNext.index()]].id, "song-2");
    app.queue_selected_track(false);
    assert!(app.status.contains("already in the queue"));
    assert_eq!(app.now.as_ref().unwrap().queue.len(), 5);
    app.results = vec![track(0)];
    app.selected = 0;
    app.queue_selected_track(true);
    assert_eq!(app.now.as_ref().unwrap().queue.len(), 5);
    assert_eq!(app.now.as_ref().unwrap().queue[0].id, "song-0");
    assert!(driver.commands.try_recv().is_err());
    assert!(!requests.try_iter().any(|request| matches!(request, Request::Resolve { .. })));
}

fn open_lyrics(app: &mut App) {
    let mut now = NowPlaying::new(&track(0));
    now.tab = Tab::Lyrics;
    now.watched = true;
    now.lyrics_id = Some("lyrics-page".into());
    app.now = Some(now);
    app.view = View::Playing;
    app.tick_page();
}

#[test]
fn plain_lyrics_upgrade_to_visible_synced_lyrics_without_restarting_the_song() {
    let (mut app, driver, requests) = fixture();
    open_lyrics(&mut app);
    assert!(matches!(requests.try_recv().unwrap(), Request::Lyrics { .. }));
    app.apply(Response::Lyrics { video_id: "song-0".into(), lyrics: Ok(Lyrics {
        text: "plain words".into(), source: None, timed: Vec::new(),
    }) });
    let now = app.now.as_mut().unwrap();
    assert!(now.lyrics_retry_at.is_some());
    now.lyrics_retry_at = Some(Instant::now());
    app.tick_page();
    assert!(matches!(requests.try_recv().unwrap(), Request::Lyrics { video_id, .. } if video_id == "song-0"));
    assert!(matches!(&app.now.as_ref().unwrap().lyrics, Panel::Ready(lyrics) if lyrics.text == "plain words"));
    app.tick_page();
    assert!(requests.try_recv().is_err(), "only one retry may be in flight");

    app.apply(Response::Lyrics { video_id: "song-0".into(), lyrics: Ok(Lyrics {
        text: "synced words".into(), source: Some("Source: LRCLIB".into()),
        timed: vec![crate::source::youtube::watch::TimedLine { text: "synced words".into(), start: Duration::from_secs(1) }],
    }) });
    assert!(app.now.as_ref().unwrap().lyrics_retry_at.is_none());
    state(&driver, PlayState::Playing, Duration::from_secs(2));
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 36)).unwrap();
    let mut mouse = crate::ui::MouseMap::default();
    terminal.draw(|frame| crate::ui::render(frame, &mut app, &mut mouse)).unwrap();
    let rendered: String = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect();
    assert!(rendered.contains("synced words") && rendered.contains('▌'), "{rendered}");
    assert!(driver.commands.try_iter().next().is_none(), "upgrading lyrics must not restart audio");

    app.apply(Response::Lyrics { video_id: "song-0".into(), lyrics: Err("late failure".into()) });
    assert!(matches!(&app.now.as_ref().unwrap().lyrics, Panel::Ready(lyrics) if !lyrics.timed.is_empty()));
}

#[test]
fn lyrics_retries_are_bounded_preserve_words_and_wait_until_the_panel_is_visible() {
    let (mut app, _driver, requests) = fixture();
    open_lyrics(&mut app);
    requests.try_iter().for_each(drop);
    app.apply(Response::Lyrics { video_id: "song-0".into(), lyrics: Err("temporary failure".into()) });
    let now = app.now.as_mut().unwrap();
    now.lyrics_retry_at = Some(Instant::now());
    now.tab = Tab::UpNext;
    app.view = View::Tracks;
    app.tick_page();
    assert!(requests.try_recv().is_err(), "hidden panels must not spend requests");
    app.now.as_mut().unwrap().tab = Tab::Lyrics;
    app.tick_page();
    assert!(matches!(requests.try_recv().unwrap(), Request::Lyrics { .. }));
    app.apply(Response::Lyrics { video_id: "song-0".into(), lyrics: Ok(Lyrics {
        text: "plain words".into(), source: None, timed: Vec::new(),
    }) });
    app.now.as_mut().unwrap().lyrics_retry_at = Some(Instant::now());
    app.tick_page();
    assert!(matches!(requests.try_recv().unwrap(), Request::Lyrics { .. }));
    app.apply(Response::Lyrics { video_id: "song-0".into(), lyrics: Err("still offline".into()) });
    let now = app.now.as_ref().unwrap();
    assert_eq!(now.lyrics_attempts, 3);
    assert!(now.lyrics_retry_at.is_none());
    assert!(matches!(&now.lyrics, Panel::Ready(lyrics) if lyrics.text == "plain words"));
    for _ in 0..20 { app.tick_page(); }
    assert!(requests.try_recv().is_err());
    app.play_track(track(1), false);
    requests.try_iter().for_each(drop);
    app.apply(Response::Lyrics { video_id: "song-0".into(), lyrics: Err("obsolete".into()) });
    app.tick_page();
    assert!(app.now.as_ref().unwrap().lyrics_retry_at.is_none());
    assert!(requests.try_recv().is_err(), "old responses must not retry the previous song");
}

#[test]
fn system_media_controls_publish_track_and_use_the_existing_queue_and_seek_paths() {
    use crate::desktop::media::MediaCommand;
    let (mut app, driver, _requests) = fixture();
    assert!(app.media_info().id.is_empty());
    app.play_track(track(0), false);
    state(&driver, PlayState::Playing, Duration::from_secs(90));
    assert_eq!(app.media_info().state, PlayState::Buffering);
    assert_eq!(app.media_info().position, Duration::ZERO, "a new title must never carry the old decoder's position");
    resolve(&mut app, 0);
    state(&driver, PlayState::Playing, Duration::from_secs(30));
    app.now.as_mut().unwrap().queue = (0..4).map(track).collect();
    app.now.as_mut().unwrap().playing = Some(0);
    let info = app.media_info();
    assert_eq!((info.title.as_str(), info.artist.as_str()), ("Song 0", "Artist"));
    assert_eq!(info.duration, Some(Duration::from_secs(240)));
    assert_eq!(info.position, Duration::from_secs(30));
    assert!(info.can_next && info.can_seek && !info.can_previous);
    driver.commands.try_iter().for_each(drop);
    app.handle_media(MediaCommand::Pause);
    app.handle_media(MediaCommand::Pause);
    app.handle_media(MediaCommand::Play);
    app.handle_media(MediaCommand::Seek(Duration::from_secs(61)));
    app.handle_media(MediaCommand::Seek(Duration::from_secs(999)));
    let commands: Vec<_> = driver.commands.try_iter().collect();
    assert!(matches!(commands.as_slice(), [Command::SetPaused(true), Command::SetPaused(true),
        Command::SetPaused(false), Command::Seek(p), Command::SeekToEnd(end)]
        if *p == Duration::from_secs(61) && *end == Duration::from_secs(240)), "{commands:?}");
    app.handle_media(MediaCommand::Next);
    assert_eq!(app.now.as_ref().unwrap().video_id, "song-1");
    assert!(!app.media_info().can_seek);
    app.handle_media(MediaCommand::Previous);
    assert_eq!(app.now.as_ref().unwrap().video_id, "song-0");
    app.stop();
    assert!(app.media_info().id.is_empty());
}

#[test]
fn artist_songs_and_catalogue_are_clickable_and_page_without_starting_playback() {
    for (width, height) in [(160, 42), (80, 32), (28, 24)] {
        let (mut app, driver, requests) = fixture();
        driver.commands.try_iter().for_each(drop);
        let artist = ArtistRef { name: "Artist".into(), endpoint: BrowseEndpoint::new("UCartist") };
        app.artist = Some(ArtistView::loading(artist.clone()));
        app.artist.as_mut().unwrap().set_page(Ok(ArtistPage {
            artist, audience: None, description: None, art: None,
            top_songs: (0..20).map(|index| ArtistSong { track: track(index), plays: None }).collect(),
            songs_endpoint: None, songs_truncated: false,
            shelves: vec![Shelf { title: "Albums".into(), cards: (0..20).map(|index| Card {
                title: format!("Album {index}"), subtitle: "Artist".into(), art: None, duration: None, artist_ref: None,
                target: Target::Open { endpoint: BrowseEndpoint::new(format!("MPRE{index}")) },
            }).collect() }],
        }));
        app.view = View::Artist;
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        let mut mouse = crate::ui::MouseMap::default();
        for section in 0..2 {
            app.artist.as_mut().unwrap().section = section;
            terminal.draw(|frame| crate::ui::render(frame, &mut app, &mut mouse)).unwrap();
            let mut pages = 0;
            loop {
                let view = app.artist.as_ref().unwrap();
                let first = if section == 0 { view.song_offset } else { view.scroll[0] };
                let next = (0..height).find_map(|y| (0..width).find_map(|x| match mouse.action_at(x, y) {
                    Some(action @ MouseAction::PageArtistSongs { offset, .. }) if section == 0 && offset > first => Some(action),
                    Some(action @ MouseAction::PageArtistShelf { offset, .. }) if section == 1 && offset > first => Some(action),
                    _ => None,
                }));
                let Some(action) = next else { break; };
                app.handle_mouse_action(action).unwrap();
                terminal.draw(|frame| crate::ui::render(frame, &mut app, &mut mouse)).unwrap();
                pages += 1; assert!(pages < 21);
            }
            let action = if section == 0 { MouseAction::OpenArtistSong(19) } else { MouseAction::OpenArtistCard { shelf: 0, card: 19 } };
            let position = (0..height).find_map(|y| (0..width).find_map(|x| (mouse.action_at(x, y) == Some(action)).then_some((x, y))));
            let (x, y) = position.expect("the last artist item must be reachable using only mouse paging");
            let select = mouse.context_action_at(x, y).unwrap();
            app.handle_mouse_action(select).unwrap();
            assert_eq!(app.artist.as_ref().unwrap().section, section);
            assert!(driver.commands.try_iter().next().is_none(), "paging or right-click selection cannot play music");
        }
        app.handle_mouse_action(MouseAction::OpenArtistCard { shelf: 0, card: 19 }).unwrap();
        assert_eq!(app.browsing_endpoint.as_ref().unwrap().browse_id, "MPRE19");
        app.go_back();
        app.handle_mouse_action(MouseAction::OpenArtistSong(19)).unwrap();
        assert_eq!(app.now.as_ref().unwrap().video_id, "song-19");
        assert!(requests.try_iter().next().is_some());
    }
}

#[test]
fn window_close_keeps_the_tray_but_end_task_and_explicit_quit_stop_the_player() {
    let (mut app, _driver, _requests) = fixture();
    app.handle_window_loss(false);
    assert!(app.wants_background && !app.should_quit);
    app.wants_foreground = true;
    app.handle_window_loss(true);
    assert!(app.should_quit && !app.wants_background && !app.wants_foreground);
    app.handle_window_loss(false);
    assert!(app.should_quit && !app.wants_background, "a late close event cannot undo a quit");
}

#[test]
fn tray_click_pauses_and_resumes_without_clearing_the_song_or_queue() {
    let (mut app, driver, _requests) = fixture();
    app.play_track(track(0), false);
    resolve(&mut app, 0);
    let position = Duration::from_secs(83);
    state(&driver, PlayState::Playing, position);
    app.now.as_mut().unwrap().queue = (0..4).map(track).collect();
    driver.commands.try_iter().for_each(drop);
    app.handle_tray(TrayCommand::Click);
    assert!(app.wants_foreground && app.now.is_some());
    assert!(driver.commands.try_iter().next().is_none());

    app.wants_foreground = false;
    app.tray_click_pauses = true;
    app.handle_tray(TrayCommand::Click);
    let commands: Vec<_> = driver.commands.try_iter().collect();
    assert!(matches!(commands.as_slice(), [Command::TogglePause]), "{commands:?}");
    state(&driver, PlayState::Paused, position);
    assert!(!app.tray_playing());
    assert_eq!(app.snapshot().position, position);
    assert_eq!(app.now.as_ref().unwrap().video_id, "song-0");
    assert_eq!(app.now.as_ref().unwrap().queue.len(), 4);
    assert!(!app.wants_foreground && !app.should_quit);

    app.handle_tray(TrayCommand::Click);
    let commands: Vec<_> = driver.commands.try_iter().collect();
    assert!(matches!(commands.as_slice(), [Command::TogglePause]), "{commands:?}");
    state(&driver, PlayState::Playing, position);
    assert!(app.tray_playing());
    assert_eq!(app.snapshot().position, position);
    assert_eq!(app.now.as_ref().unwrap().video_id, "song-0");
    assert_eq!(app.now.as_ref().unwrap().queue.len(), 4);

    app.handle_tray(TrayCommand::Show);
    assert!(app.wants_foreground, "the menu can still show MTUI when clicks toggle pause");
}

#[test]
fn tray_wave_rests_on_pause_loading_and_stop_before_the_worker_catches_up() {
    let (mut app, driver, _requests) = fixture();
    assert!(!app.tray_playing());
    app.play_track(track(0), false);
    resolve(&mut app, 0);
    state(&driver, PlayState::Playing, Duration::from_secs(10));
    assert!(app.tray_playing());

    for playback in [PlayState::Paused, PlayState::Buffering, PlayState::Idle] {
        state(&driver, playback, Duration::from_secs(10));
        assert!(!app.tray_playing(), "{playback:?} should show the resting line");
        state(&driver, PlayState::Playing, Duration::from_secs(10));
        assert!(app.tray_playing(), "resuming should restore the waveform");
    }

    app.play_track(track(1), false);
    assert!(!app.tray_playing(), "a new resolve must not inherit the old playing snapshot");
    resolve(&mut app, 1);
    assert!(app.tray_playing());

    app.stop();
    assert_eq!(
        app.snapshot().state, PlayState::Playing, "the worker is still catching up"
    );
    assert!(!app.tray_playing(), "Stop should immediately select the resting icon");
}

#[test]
fn discord_artwork_follows_only_the_current_tracks_watch_response() {
    let (mut app, driver, _requests) = fixture();
    app.play_track(track(0), false);
    state(&driver, PlayState::Playing, Duration::from_secs(10));
    let watch = || Watch { artwork_url: Some("https://example.com/sleeve.jpg".into()), ..Default::default() };
    app.apply_watch("song-1", Ok(watch()));
    assert!(app.activity().unwrap().artwork_url.is_none());
    app.apply_watch("song-0", Ok(watch()));
    assert_eq!(app.activity().unwrap().artwork_url.as_deref(), Some("https://example.com/sleeve.jpg"));

    app.play_track(track(1), false);
    app.apply_watch("song-0", Ok(watch()));
    assert!(app.activity().unwrap().artwork_url.is_none(), "the next song must not inherit artwork");
    app.stop();
    app.apply_watch("song-1", Ok(watch()));
    assert!(app.activity().is_none());
}

#[test]
fn cover_requests_wait_for_music_metadata_and_use_video_only_as_fallback() {
    let sleeve = "https://lh3.googleusercontent.com/sleeve=w544-h544-l90-rj";
    for (watch, expected) in [
        (Ok(Watch { artwork_url: Some(sleeve.into()), ..Default::default() }), Some(sleeve)),
        (Ok(Watch::default()), None),
        (Err("Music metadata unavailable".into()), None),
    ] {
        let (mut app, _driver, requests) = fixture();
        app.cover = Some(Cover::solid(10, 10));
        app.play_track(track(0), false);
        assert!(app.cover.is_none());
        assert!(requests.try_iter().all(|request| !matches!(request, Request::Cover { .. })),
            "a video thumbnail must not be requested before Music metadata arrives");
        app.apply_watch("song-0", watch);
        let covers: Vec<_> = requests.try_iter().filter(|request| matches!(request, Request::Cover { .. })).collect();
        assert_eq!(covers.len(), 1);
        assert!(matches!(&covers[0], Request::Cover { id, artwork_url, edge: crate::source::cover::CELL_EDGE }
            if id == "song-0" && artwork_url.as_deref() == expected));
    }
}

#[test]
fn loaded_cover_source_updates_discord_and_late_images_cannot_replace_it() {
    let (mut app, driver, requests) = fixture();
    app.image_renderer = ImageRenderer::Kitty;
    let sleeve = "https://example.com/sleeve.jpg";
    app.play_track(track(0), false);
    state(&driver, PlayState::Playing, Duration::from_secs(10));
    app.apply_watch("song-0", Ok(Watch { artwork_url: Some(sleeve.into()), ..Default::default() }));
    app.apply(Response::Cover { id: "song-0".into(), art: Some(Cover::solid(544, 544)), artwork_url: Some(sleeve.into()), edge: crate::source::cover::MAX_EDGE });
    assert_eq!(app.cover.as_ref().unwrap().width, 544);
    assert_eq!(app.activity().unwrap().artwork_url.as_deref(), Some(sleeve));
    app.apply(Response::Cover { id: "old-song".into(), art: Some(Cover::solid(10, 10)), artwork_url: None, edge: crate::source::cover::MAX_EDGE });
    assert_eq!(app.activity().unwrap().artwork_url.as_deref(), Some(sleeve));
    assert_eq!(app.cover.as_ref().unwrap().width, 544);

    // A failed Music fetch can still return a decoded video thumbnail.
    app.apply(Response::Cover { id: "song-0".into(), art: Some(Cover::solid(720, 720)), artwork_url: None, edge: crate::source::cover::MAX_EDGE });
    assert_eq!(app.activity().unwrap().artwork_url.as_deref(), Some(sleeve), "video fallback pixels must not replace Discord's native Music artwork");
    assert_eq!(app.cover.as_ref().unwrap().width, 720);
    app.stop();
    requests.try_iter().for_each(drop);
    app.apply_watch("song-0", Ok(Watch::default()));
    app.apply(Response::Cover { id: "song-0".into(), art: Some(Cover::solid(544, 544)), artwork_url: Some(sleeve.into()), edge: crate::source::cover::MAX_EDGE });
    assert!(app.cover.is_none() && app.activity().is_none());
    assert!(requests.try_iter().all(|request| !matches!(request, Request::Cover { .. })));
}

#[test]
fn volume_buttons_and_pointer_use_five_percent_steps_at_the_bounds_and_after_dragging() {
    let (mut app, driver, _requests) = fixture();
    driver.commands.try_iter().for_each(drop);
    for (initial, action, expected) in [
        (0.10, MouseAction::AdjustVolume(true), 0.15),
        (0.10, MouseAction::AdjustVolume(false), 0.05),
        (0.0, MouseAction::AdjustVolume(false), 0.0),
        (0.95, MouseAction::AdjustVolume(true), 1.0),
        (1.0, MouseAction::AdjustVolume(true), 1.0),
        (1.63, MouseAction::AdjustVolume(true), 1.0),
        (0.72, MouseAction::AdjustVolume(true), 0.75),
        (0.5, MouseAction::SetVolume(1234), 0.10),
        (0.5, MouseAction::SetVolume(6789), 0.70),
    ] {
        driver.snapshot.lock().unwrap().volume = initial;
        app.volume_save = None;
        app.handle_mouse_action(action).unwrap();
        let Command::SetVolume(volume) = driver.commands.try_recv().unwrap() else { panic!("wrong playback command") };
        assert!((volume - expected).abs() < 0.00001, "{initial}, {action:?}: {volume}");
    }
    driver.snapshot.lock().unwrap().volume = 0.10;
    app.volume_save = None;
    for expected in [0.15, 0.20, 0.25] {
        app.handle_mouse_action(MouseAction::AdjustVolume(true)).unwrap();
        let Command::SetVolume(volume) = driver.commands.try_recv().unwrap() else { panic!("wrong playback command") };
        assert!((volume - expected).abs() < 0.00001, "rapid clicks must accumulate before the audio thread responds");
    }
}

#[test]
fn precision_cover_volume_uses_one_percent_steps_and_accumulates_rapid_wheel_input() {
    let (mut app, driver, _requests) = fixture();
    driver.commands.try_iter().for_each(drop);
    app.handle_mouse_action(MouseAction::AdjustPrecisionVolume(true)).unwrap();
    assert!(driver.commands.try_iter().next().is_none(), "disabled precision is inert");
    app.precision_volume = true;
    for (initial, up, expected) in [
        (0.50, true, 0.51), (0.51, false, 0.50),
        (0.0, false, 0.0), (0.0, true, 0.01),
        (0.01, false, 0.0), (0.99, true, 1.0), (1.0, true, 1.0),
        (1.63, true, 1.0),
    ] {
        app.volume_save = None;
        driver.snapshot.lock().unwrap().volume = initial;
        app.handle_mouse_action(MouseAction::AdjustPrecisionVolume(up)).unwrap();
        let Command::SetVolume(volume) = driver.commands.try_recv().unwrap() else { panic!("wrong command") };
        assert!((volume - expected).abs() < 0.00001, "{initial}, {up}: {volume}");
    }
    app.volume_save = None;
    driver.snapshot.lock().unwrap().volume = 0.50;
    for (up, expected) in [(true, 0.51), (true, 0.52), (false, 0.51), (false, 0.50)] {
        app.handle_mouse_action(MouseAction::AdjustPrecisionVolume(up)).unwrap();
        let Command::SetVolume(volume) = driver.commands.try_recv().unwrap() else { panic!("wrong command") };
        assert!((volume - expected).abs() < 0.00001, "wheel ticks must accumulate before the audio snapshot updates");
    }
    assert_eq!(mute_transition(true, 0.0, 0.03).0, 0.03, "mute restores precision levels below 5%");
}

#[test]
fn artwork_renderer_choices_reach_the_encoder_and_respect_modal_and_text_fallbacks() {
    use ratatui::{Terminal, backend::TestBackend};
    use ImageRenderer::{Automatic, Kitty, PixelArt, Sixel};

    for (sixel, kitty, renderer, expected) in [
        (false, false, Automatic, None),
        (true, false, Automatic, Some(Sixel)),
        (false, true, Automatic, Some(Kitty)),
        (false, false, Sixel, Some(Sixel)),
        (false, true, Sixel, Some(Sixel)),
        (true, false, Kitty, Some(Kitty)),
        (true, false, PixelArt, None),
        (false, true, PixelArt, None),
    ] {
        let (mut app, _driver, _requests) = fixture();
        queue(&mut app, RepeatMode::Off, 0);
        app.cover = Some(Cover::from_rgb(2, 2, [180, 90, 60].repeat(4)));
        app.cover_size = CoverSize::Full;
        app.graphics = Graphics { sixel, kitty, cell: (8, 16) };
        app.image_renderer = renderer;
        let mut terminal = Terminal::new(TestBackend::new(48, 18)).unwrap();
        let mut mouse = crate::ui::MouseMap::default();
        terminal.draw(|frame| crate::ui::render(frame, &mut app, &mut mouse)).unwrap();

        let mut output = Vec::new();
        crate::paint_cover(&mut app, &mut output).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert_eq!(output.contains("\x1bPq"), expected == Some(Sixel), "{renderer:?}");
        assert_eq!(output.contains("\x1b_Ga=T"), expected == Some(Kitty), "{renderer:?}");
        assert_eq!(app.images.is_empty(), expected.is_none());
        let mut unchanged = Vec::new();
        crate::paint_cover(&mut app, &mut unchanged).unwrap();
        assert!(unchanged.is_empty(), "unchanged covers must not be re-encoded");

        terminal.backend_mut().resize(32, 12);
        terminal.draw(|frame| crate::ui::render(frame, &mut app, &mut mouse)).unwrap();
        assert_eq!(app.image_needs_clearing(), expected.is_some(), "resizing must replace old placements");
        app.invalidate_image();
        let mut resized = Vec::new();
        crate::paint_cover(&mut app, &mut resized).unwrap();
        assert_eq!(resized.is_empty(), expected.is_none());

        app.overlay = Overlay::Settings;
        terminal.draw(|frame| crate::ui::render(frame, &mut app, &mut mouse)).unwrap();
        assert!(app.images.is_empty(), "a cover cannot show through Settings");
        assert_eq!(app.image_needs_clearing(), expected.is_some());
        app.invalidate_image();

        app.overlay = Overlay::None;
        app.cover_style = CoverStyle::ColoredAscii;
        terminal.draw(|frame| crate::ui::render(frame, &mut app, &mut mouse)).unwrap();
        assert!(app.images.is_empty(), "colored text must override bitmap modes");
        let mut text = Vec::new();
        crate::paint_cover(&mut app, &mut text).unwrap();
        assert!(text.is_empty());
    }
}

#[test]
fn home_and_search_use_sixel_thumbnails_and_keep_the_terminal_cell_fallback() {
    use crate::source::catalog::search::{Filter, Item};
    use ratatui::{Terminal, backend::TestBackend};

    for view in [View::Home, View::Tracks] {
        for renderer in [ImageRenderer::Automatic, ImageRenderer::Sixel, ImageRenderer::Kitty, ImageRenderer::PixelArt] {
            let (mut app, _driver, _requests) = fixture();
            let card = Card {
                title: "Cover fixture".into(), subtitle: "Artist".into(),
                art: Some("https://example.invalid/cover.jpg".into()),
                duration: None, artist_ref: None,
                target: Target::Play { video_id: "song-0".into() },
            };
            let key = card.art_key().to_string();
            app.apply_home(vec![Shelf { title: "Albums for you".into(), cards: vec![card.clone()] }]);
            app.search_items = vec![Item { card, kind: Filter::Songs, track: Some(track(0)) }];
            app.art.want(&key);
            app.art.store(key.clone(), Some(Cover::from_rgb(2, 2, [180, 90, 60].repeat(4))));
            app.view = view;
            app.graphics = Graphics { sixel: true, kitty: false, cell: (8, 16) };
            app.image_renderer = renderer;
            let mut terminal = Terminal::new(TestBackend::new(100, 36)).unwrap();
            let mut mouse = crate::ui::MouseMap::default();
            terminal.draw(|frame| crate::ui::render(frame, &mut app, &mut mouse)).unwrap();
            let bitmap = renderer != ImageRenderer::PixelArt;
            assert_eq!(app.images.iter().any(|image| image.source == ImageSource::Card(key.clone())), bitmap,
                "missing bitmap thumbnail in {view:?} with {renderer:?}");
            let mut output = Vec::new();
            crate::paint_cover(&mut app, &mut output).unwrap();
            let output = String::from_utf8(output).unwrap();
            assert_eq!(output.contains("\x1bPq"), bitmap && renderer != ImageRenderer::Kitty);
            assert_eq!(output.contains("\x1b_Ga=T"), renderer == ImageRenderer::Kitty);
            terminal.draw(|frame| crate::ui::render(frame, &mut app, &mut mouse)).unwrap();
            assert!(app.images_to_paint().is_empty(), "unchanged thumbnail should stay on screen");
            app.overlay = Overlay::Settings;
            terminal.draw(|frame| crate::ui::render(frame, &mut app, &mut mouse)).unwrap();
            assert!(app.images.is_empty());
            assert_eq!(app.image_needs_clearing(), bitmap);
        }
    }
}

#[test]
fn cover_clicks_toggle_playback_and_precision_wheels_only_target_visible_current_artwork() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    use crate::ui::MouseMap;
    use ratatui::{Terminal, backend::TestBackend};
    for (width, height, style, full, sixel, kitty) in [
        (132, 42, CoverStyle::Pixel, false, false, false),
        (132, 42, CoverStyle::ColoredAscii, false, false, false),
        (60, 24, CoverStyle::Pixel, false, false, false),
        (60, 24, CoverStyle::Pixel, true, false, false),
        (132, 42, CoverStyle::Pixel, false, true, false),
        (132, 42, CoverStyle::Pixel, false, false, true),
        (60, 24, CoverStyle::Pixel, false, false, true),
        (60, 24, CoverStyle::Pixel, true, true, false),
    ] {
        let (mut app, driver, requests) = fixture();
        queue(&mut app, RepeatMode::Off, 0);
        app.cover = Some(Cover::solid(32, 32));
        app.cover_style = style;
        app.graphics = Graphics { sixel, kitty, cell: (9, 20) };
        if full { app.cover_size = CoverSize::Full; }
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut mouse = MouseMap::default();
        terminal.draw(|frame| crate::ui::render(frame, &mut app, &mut mouse)).unwrap();
        let points: Vec<_> = (0..height).flat_map(|y| (0..width).map(move |x| (x, y)))
            .filter(|&(x, y)| mouse.over_cover(x, y)).collect();
        assert!(!points.is_empty(), "missing cover target at {width}x{height}");
        assert!(points.iter().all(|&(x, y)| mouse.action_at(x, y) == Some(MouseAction::TogglePlayback)));
        assert!(!mouse.over_cover(0, 0), "header is not artwork");
        assert!(!mouse.over_cover(width - 1, height - 1), "footer is not artwork");
        let (column, row) = points[points.len() / 2];
        let event = |kind| MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE };
        let mut pointer = crate::terminal::pointer::Pointer::default();
        for playback in [PlayState::Playing, PlayState::Paused] {
            state(&driver, playback, Duration::from_secs(35));
            pointer.handle(&mut app, &mouse, event(MouseEventKind::Down(MouseButton::Left))).unwrap();
            pointer.handle(&mut app, &mouse, event(MouseEventKind::Up(MouseButton::Left))).unwrap();
            assert!(matches!(driver.commands.try_recv().unwrap(), Command::TogglePause));
            assert!(driver.commands.try_recv().is_err(), "one click toggles once");
            assert_eq!(app.now.as_ref().unwrap().video_id, "song-0");
            assert_eq!(app.now.as_ref().unwrap().queue.len(), 4);
        }
        pointer.handle(&mut app, &mouse, event(MouseEventKind::ScrollDown)).unwrap();
        assert!(driver.commands.try_recv().is_err(), "wheel leaves volume alone when precision is off");
        app.precision_volume = true;
        driver.snapshot.lock().unwrap().volume = 0.50;
        pointer.handle(&mut app, &mouse, event(MouseEventKind::ScrollUp)).unwrap();
        assert!(matches!(driver.commands.try_recv().unwrap(), Command::SetVolume(volume) if (volume - 0.51).abs() < 0.00001));
        assert!(driver.commands.try_recv().is_err(), "wheeling the cover must not toggle playback");
        assert!(!requests.try_iter().any(|request| matches!(request, Request::Resolve { .. })));
        app.volume_save = None;
        app.overlay = Overlay::Settings;
        terminal.draw(|frame| crate::ui::render(frame, &mut app, &mut mouse)).unwrap();
        assert!(points.iter().all(|&(x, y)| !mouse.over_cover(x, y)), "settings block artwork behind them");
        app.overlay = Overlay::None;
        app.cover = None;
        terminal.draw(|frame| crate::ui::render(frame, &mut app, &mut mouse)).unwrap();
        assert!(points.iter().all(|&(x, y)| !mouse.over_cover(x, y)), "missing art has no cover controls");
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
fn home_responses_keep_unrelated_page_requests_and_menus_open() {
    let (mut app, _driver, _requests) = fixture();
    app.request_home();
    let generation = app.home_generation;
    app.menu = Some(Menu::new(MenuPage::PageActions, Vec::new()));
    app.begin_page_request(PageRequestKind::Browse);

    app.apply(Response::HomeMore { generation, shelves: Vec::new() });
    assert!(app.page_loading());

    let shelves = || vec![Shelf { title: "Home feed".into(), cards: Vec::new() }];
    app.apply(Response::Home {
        generation: generation.wrapping_sub(1),
        shelves: shelves(),
    });
    assert!(app.home_pending);
    assert!(app.home.is_empty());
    assert!(app.menu.is_some());

    app.apply(Response::Home { generation, shelves: shelves() });
    assert!(!app.home_pending);
    assert_eq!(app.home.len(), 1);
    assert!(app.menu.is_some());
    assert!(app.page_loading());
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
        timed: vec![crate::source::youtube::watch::TimedLine { text: "An invented test line".into(), start: Duration::ZERO }],
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

fn resolved_stream(index: usize) -> StreamUrl {
    StreamUrl {
        url: format!("https://fixture.invalid/song-{index}.m4a"),
        expires_at: None, content_length: Some(1024),
        format: mtui_resolver::AudioFormat { itag: Some(140) },
        source: mtui_resolver::ResolveSource::Cache,
    }
}

fn resolve(app: &mut App, index: usize) {
    app.apply(Response::Resolved {
        id: track(index).id, title: track(index).title,
        stream: Ok(resolved_stream(index)),
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
        assert!(matches!(commands.as_slice(), [Command::Load { .. }, Command::Play { id, .. }, Command::SeekToEnd(target)]
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
    assert!(matches!(commands.as_slice(), [Command::Load { .. }, Command::Play { .. }, Command::SeekToEnd(target)]
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

#[test]
fn a_stale_resolve_cannot_consume_the_new_tracks_deferred_seek() {
    for stale_succeeds in [false, true] {
        let (mut app, driver, _requests) = fixture();
        app.play_track(track(0), false);
        app.handle_mouse_action(MouseAction::SeekTo(POINTER_SCALE)).unwrap();
        app.play_track(track(1), false);
        app.handle_mouse_action(MouseAction::SeekTo(POINTER_SCALE / 2)).unwrap();
        app.apply(Response::Resolved {
            id: track(0).id, title: track(0).title,
            stream: if stale_succeeds { Ok(resolved_stream(0)) } else { Err("old failure".into()) },
        });
        assert_eq!(app.playback.pending_id(), Some("song-1"));
        assert!(app.playback.failure().is_none());
        resolve(&mut app, 1);
        let commands: Vec<_> = driver.commands.try_iter().collect();
        assert!(matches!(commands.as_slice(), [Command::Load { .. }, Command::Load { .. },
            Command::Play { id, .. }, Command::Seek(position)]
            if id == "song-1" && *position == Duration::from_secs(120)), "{commands:?}");
    }
}

#[test]
fn recovery_answers_match_the_current_play_and_cannot_restart_after_stop() {
    for replacement in [false, true] {
        let (mut app, driver, _requests) = fixture();
        app.play_track(track(1), false);
        resolve(&mut app, 1);
        driver.commands.try_iter().for_each(drop);
        let answer = |index| if replacement {
            Response::Replacement { id: track(index).id, stream: Ok(resolved_stream(index)) }
        } else {
            Response::Resolved { id: track(index).id, title: track(index).title, stream: Ok(resolved_stream(index)) }
        };
        let from = Duration::from_secs(58);
        driver.events.send(PlayerEvent::NeedsUrl { id: track(1).id, from }).unwrap();
        app.poll_player();
        app.apply(answer(0));
        assert!(app.playback.is_recovering());
        assert!(driver.commands.try_recv().is_err());
        app.apply(answer(1));
        let commands: Vec<_> = driver.commands.try_iter().collect();
        assert!(matches!(commands.as_slice(), [Command::Resume { from: position, .. }] if *position == from), "{commands:?}");
        assert!(!app.playback.is_recovering());
        driver.events.send(PlayerEvent::NeedsUrl { id: track(1).id, from }).unwrap();
        app.poll_player();
        app.stop();
        app.apply(answer(1));
        assert!(matches!(driver.commands.try_iter().collect::<Vec<_>>().as_slice(), [Command::Stop]));
        assert!(app.playback.listening_track().is_none());
        assert!(!app.playback.is_recovering());
    }
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
    assert!(app.now.is_none() && !app.playback.is_loading());
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
        assert!(driver.commands.try_iter().all(|cmd| !matches!(cmd, Command::Seek(_) | Command::SeekToEnd(_))));
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
                assert!(matches!(commands.as_slice(), [Command::Seek(target) | Command::SeekToEnd(target)] if *target <= total), "{commands:?}");
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
    assert_eq!(app.playback.pending_id(), Some("song-1"));
    assert_eq!(app.playback.listening_track().map(|track| track.id.as_str()), Some("song-1"),
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
    assert_eq!(app.playback.pending_id(), Some("song-1"));
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
    assert!(app.playback.failure().is_some());
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
    assert!(app.playback.failure().is_some());
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
    assert!(!app.playback.is_recovering());
    assert_eq!(app.playback.pending_id(), Some("song-1"));
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
    app.apply(Response::Cover { id: "song-0".into(), art: Some(Cover::from_rgb(1, 1, vec![255, 0, 0])), artwork_url: None, edge: crate::source::cover::CELL_EDGE });
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
    assert!(matches!(driver.commands.try_recv().unwrap(), Command::SeekToEnd(target) if target == Duration::from_secs(240)));
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
