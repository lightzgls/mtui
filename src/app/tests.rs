use super::*;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config;
use crate::graphics::Graphics;
use crate::player::{OutputDevice, Player};
use crate::source::artist::{ArtistPage, ArtistSong};
use crate::source::cover::Cover;
use crate::source::home::{Card, Shelf, Target};
use crate::source::watch::{Lyrics, QueuePage};
use crate::source::worker::SourceWorker;
use crate::source::{ArtistRef, BrowseEndpoint, Track};

use super::artist::ArtistView;
use super::input::{MouseAction, POINTER_SCALE, is_bare_character, is_ctrl_key};
use super::menu::{Menu, MenuAction, MenuPage, account_menu_items, app_menu_items};
use super::navigation::{
    HistoryEntry, PAGE_HISTORY, PageRequestKind, TrackPage, View, accept_pending_page,
    page_behind_search, page_snapshot_blocked, player_return_target, push_history,
};
use super::now_playing::{
    NowPlaying, Panel, QUEUE_AHEAD, QUEUE_BEHIND, QUEUE_MEMORY, RepeatMode, Tab, shuffle_tracks,
};
use super::playback::{
    QUEUE_LOW, duration_at_pointer, mute_transition, playback_failure_status,
    playback_failure_summary, pointer_fraction,
};
use super::preferences::{VOLUME_STEP, next_output_device};

#[test]
fn preview_search_palette_and_player_links() {
    use crate::source::search::{Filter, Item};
    let (mut app, _driver, _requests) = super::scenarios::fixture();
    let artist = ArtistRef {
        name: "Evening Artist".into(),
        endpoint: BrowseEndpoint::new("UCfixture"),
    };
    let track = Track {
        id: "fixtureSong".into(),
        title: "A quieter evening".into(),
        uploader: artist.name.clone(),
        album: Some("Evening Album".into()),
        artist_ref: Some(artist.clone()),
        duration: Some(Duration::from_secs(180)),
    };
    app.search_items = [
        (
            Filter::Artists,
            "Evening Artist",
            Target::Artist {
                artist: artist.clone(),
            },
        ),
        (
            Filter::Albums,
            "Evening Album",
            Target::Open {
                endpoint: BrowseEndpoint::new("MPREfixture"),
            },
        ),
        (
            Filter::Playlists,
            "After hours",
            Target::Open {
                endpoint: BrowseEndpoint::new("VLfixture"),
            },
        ),
        (
            Filter::Songs,
            "A quieter evening",
            Target::Play {
                video_id: track.id.clone(),
            },
        ),
    ]
    .into_iter()
    .map(|(kind, title, target)| Item {
        card: Card {
            title: title.into(),
            subtitle: format!("{} • Evening Artist", kind.label().trim_end_matches('s')),
            art: None,
            duration: None,
            artist_ref: Some(artist.clone()),
            target,
        },
        kind,
        track: (kind == Filter::Songs).then(|| track.clone()),
    })
    .collect();
    let keys: Vec<_> = app
        .search_items
        .iter()
        .map(|item| item.card.art_key().to_owned())
        .collect();
    app.art.want_visible(keys.iter().map(String::as_str));
    for (index, key) in keys.into_iter().enumerate() {
        app.art.store(
            key,
            Some(Cover::from_rgb(
                4,
                4,
                [70 + index as u8 * 35, 90, 110].repeat(16),
            )),
        );
    }
    app.results = vec![track.clone()];
    app.query = "Evening".into();
    app.view = View::Tracks;
    app.cover = Some(Cover::from_rgb(4, 4, [72, 104, 156].repeat(16)));
    app.now = Some(NowPlaying::new(&track));
    app.now.as_mut().unwrap().album_route =
        Some(("Evening Album".into(), BrowseEndpoint::new("MPREfixture")));
    let mut mouse = crate::ui::MouseMap::default();
    for (width, height) in [(48, 18), (100, 36), (160, 42)] {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| crate::ui::render(frame, &mut app, &mut mouse))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
        assert!(text.contains("Artists") && text.contains("Playlists"));
        assert!(
            (0..height)
                .any(|y| (0..width)
                    .any(|x| mouse.action_at(x, y) == Some(MouseAction::PlayTrack(1))))
        );
        crate::ui::preview_buffer(&format!("mixed-search-{width}x{height}"), buffer);
    }
    app.handle_mouse_action(MouseAction::PlayTrack(0)).unwrap();
    assert_eq!(app.view, View::Artist);
    assert!(app.playback.listening_track().is_none());
    app.go_back();
    assert_eq!(app.search_items.len(), 4);
    app.handle_mouse_action(MouseAction::PlayTrack(1)).unwrap();
    assert_eq!(
        app.browsing_endpoint.as_ref().unwrap().browse_id,
        "MPREfixture"
    );
    assert!(app.playback.listening_track().is_none());
    app.go_back();
    app.view = View::Playing;
    app.now.as_mut().unwrap().tab = Tab::Lyrics;
    app.now.as_mut().unwrap().lyrics = Panel::Ready(Lyrics {
        text:
            "The evening settles quietly\nA little light across the room\nThe music stays with us"
                .into(),
        source: Some("Synced lyrics".into()),
        timed: (0..8)
            .map(|index| crate::source::watch::TimedLine {
                start: Duration::from_secs(index * 10),
                text: format!("A quiet lyric line {index}"),
            })
            .collect(),
    });
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 36)).unwrap();
    terminal
        .draw(|frame| crate::ui::render(frame, &mut app, &mut mouse))
        .unwrap();
    assert!((0..36).any(|y| (0..120).any(|x| mouse.action_at(x, y) == Some(MouseAction::OpenPlayingAlbum))));
    crate::ui::preview_buffer("palette-lyrics-120x36", terminal.backend().buffer());
    app.now.as_mut().unwrap().tab = Tab::UpNext;
    app.now.as_mut().unwrap().queue_title = "Evening Mix".into();
    app.now.as_mut().unwrap().playing = Some(0);
    app.now.as_mut().unwrap().queue = (0..40)
        .map(|index| Track {
            id: format!("fixture{index}"),
            title: format!("Evening song {index:02} — a longer title"),
            ..track.clone()
        })
        .collect();
    for (width, height) in [(48, 18), (64, 20), (100, 36), (160, 42), (216, 50)] {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        for selected in [0, 20, 39] {
            app.now.as_mut().unwrap().cursor[Tab::UpNext.index()] = selected;
            terminal
                .draw(|frame| crate::ui::render(frame, &mut app, &mut mouse))
                .unwrap();
            assert!(
                (0..height).any(|y| (0..width)
                    .any(|x| mouse.action_at(x, y) == Some(MouseAction::OpenPageRow(selected)))),
                "selected song cannot be clicked at {width}x{height}"
            );
            assert!(
                (0..height).any(|y| (0..width)
                    .any(|x| mouse.action_at(x, y) == Some(MouseAction::OpenPlayingAlbum))),
                "album target missing at {width}x{height}"
            );
            if width >= 100 {
                assert!(
                    (0..height)
                        .any(|y| (0..width).any(|x| mouse.action_at(x, y)
                            == Some(MouseAction::OpenQueueArtist(selected)))),
                    "queue artist hidden at {width}x{height}"
                );
            }
            crate::ui::preview_buffer(
                &format!("spaced-queue-{width}x{height}-{selected}"),
                terminal.backend().buffer(),
            );
        }
    }
    app.handle_mouse_action(MouseAction::OpenPlayingArtist)
        .unwrap();
    assert_eq!(app.view, View::Artist);
    assert!(app.playback.listening_track().is_none());
}

#[test]
#[ignore = "reads saved playlists through clicks and workers; no playback or history writes"]
fn saved_playlist_clicks_load_tracks_in_the_real_app() {
    let cookies = config::Cookies::available()
        .unwrap()
        .expect("sign in before this check");
    let http = crate::source::http::Http::new().unwrap();
    let cards: Vec<Card> = crate::source::collection::saved_cards(&http, &cookies)
        .unwrap()
        .into_iter()
        .take(4)
        .collect();
    assert!(!cards.is_empty());
    let player = Player::spawn(0.0, None).unwrap();
    let source = SourceWorker::spawn(crate::source::youtube::YouTube::default()).unwrap();
    let mut app = App::new(
        player,
        source,
        Graphics::blocks(),
        config::Settings::default(),
    );
    // Startup Home is independent; invalidate it before providing this
    // fixture shelf so its later response cannot replace the click target.
    app.home_generation += 1;
    app.home_pending = false;
    app.home = vec![Shelf {
        title: "Saved playlists".into(),
        cards,
    }];
    for card in 0..app.home[0].cards.len() {
        app.view = View::Home;
        app.handle_mouse_action(MouseAction::OpenHomeCard { shelf: 0, card })
            .unwrap();
        assert_eq!(app.view, View::Tracks);
        assert!(app.collection.is_some());
        let deadline = Instant::now() + Duration::from_secs(25);
        while app.pending_page_request.is_some() && Instant::now() < deadline {
            app.poll_source();
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(
            app.pending_page_request.is_none(),
            "playlist request timed out"
        );
        assert!(
            app.collection_error.is_none(),
            "playlist click failed: {:?}",
            app.collection_error
        );
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 36)).unwrap();
        let mut mouse = crate::ui::MouseMap::default();
        terminal
            .draw(|frame| crate::ui::render(frame, &mut app, &mut mouse))
            .unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("Play") || app.results.is_empty());
        if let Some(track) = app.results.first() {
            assert!(rendered.contains(&track.title.chars().take(12).collect::<String>()));
        }
        println!(
            "Saved playlist click: {} tracks rendered, error=false",
            app.results.len()
        );
    }
    assert!(app.now.is_none());
    assert!(app.playback.listening_track().is_none());
}

#[test]
fn displayed_menu_shortcuts_select_the_matching_enabled_command() {
    let menu = Menu::new(MenuPage::Root, app_menu_items(false));
    for (code, modifiers, expected) in [
        (KeyCode::Char('H'), KeyModifiers::SHIFT, MenuAction::GoHome),
        (
            KeyCode::Char('/'),
            KeyModifiers::NONE,
            MenuAction::BeginSearch,
        ),
        (
            KeyCode::Char('?'),
            KeyModifiers::SHIFT,
            MenuAction::OpenHelp,
        ),
        (
            KeyCode::Char('s'),
            KeyModifiers::CONTROL,
            MenuAction::OpenSettings,
        ),
    ] {
        let index = menu.shortcut_index(KeyEvent::new(code, modifiers)).unwrap();
        assert_eq!(menu.items[index].action, Some(expected));
    }
    assert_eq!(
        menu.shortcut_index(KeyEvent::new(KeyCode::Char('P'), KeyModifiers::SHIFT)),
        None
    );
    assert_eq!(
        menu.shortcut_index(KeyEvent::new(KeyCode::Char('H'), KeyModifiers::ALT)),
        None
    );
    let account = Menu::new(MenuPage::Account, account_menu_items(false, false));
    assert_eq!(
        account.shortcut_index(KeyEvent::new(KeyCode::Char('M'), KeyModifiers::SHIFT)),
        Some(0)
    );
    let pending = Menu::new(MenuPage::Account, account_menu_items(false, true));
    assert_eq!(
        pending.shortcut_index(KeyEvent::new(KeyCode::Char('M'), KeyModifiers::SHIFT)),
        None
    );
}

#[test]
fn menu_navigation_skips_unavailable_player_commands() {
    let mut menu = Menu::new(MenuPage::Root, app_menu_items(false));
    menu.selected = menu
        .items
        .iter()
        .position(|item| item.action == Some(MenuAction::BeginSearch))
        .unwrap();
    menu.move_by(1, menu.items.len());
    assert_eq!(
        menu.items[menu.selected].action,
        Some(MenuAction::OpenAccount)
    );
    menu.move_by(-1, menu.items.len());
    assert_eq!(
        menu.items[menu.selected].action,
        Some(MenuAction::BeginSearch)
    );
}

#[test]
fn logout_is_offered_only_for_a_connected_session() {
    let disconnected = account_menu_items(false, false);
    assert_eq!(disconnected.len(), 1);
    assert_eq!(disconnected[0].label, "Connect YouTube Music");

    let connected = account_menu_items(true, false);
    assert_eq!(connected.len(), 2);
    assert_eq!(connected[0].label, "Refresh YouTube Music session");
    assert_eq!(connected[1].label, "Log out of YouTube Music");
    assert!(connected[1].enabled);

    let pending = account_menu_items(true, true);
    assert!(!pending[0].enabled);
    assert!(
        pending[1].enabled,
        "logout must remain available during renewal"
    );
}

#[test]
fn menu_cursor_movement_clamps_at_both_ends() {
    let mut menu = Menu::new(MenuPage::Root, Vec::new());

    menu.move_by(-1, 4);
    assert_eq!(menu.selected, 0);
    menu.move_by(2, 4);
    assert_eq!(menu.selected, 2);
    menu.move_by(20, 4);
    assert_eq!(menu.selected, 3);
    menu.move_by(-20, 4);
    assert_eq!(menu.selected, 0);

    menu.selected = usize::MAX;
    menu.clamp(2);
    assert_eq!(menu.selected, 1);
    menu.last(0);
    assert_eq!(menu.selected, 0);
}

#[test]
fn stale_page_responses_do_not_consume_the_active_request() {
    let mut pending = Some((42, PageRequestKind::Artist));

    assert!(!accept_pending_page(&mut pending, 41));
    assert_eq!(pending, Some((42, PageRequestKind::Artist)));
    assert!(accept_pending_page(&mut pending, 42));
    assert_eq!(pending, None);
}

#[test]
fn only_search_requests_allow_the_current_page_to_be_snapshotted() {
    assert!(!page_snapshot_blocked(None, false, false));
    assert!(page_snapshot_blocked(None, true, false));
    assert!(page_snapshot_blocked(None, false, true));
    assert!(!page_snapshot_blocked(
        Some((1, PageRequestKind::Search)),
        true,
        false
    ));
    assert!(page_snapshot_blocked(
        Some((2, PageRequestKind::Browse)),
        true,
        false
    ));
    assert!(page_snapshot_blocked(
        Some((4, PageRequestKind::Artist)),
        true,
        false
    ));
}

#[test]
fn player_return_targets_are_flattened_to_a_real_page() {
    let nested = HistoryEntry::Playing {
        back_to: View::Artist,
        player_back: Some(Box::new(HistoryEntry::Playing {
            back_to: View::Home,
            player_back: Some(Box::new(HistoryEntry::Home {
                status: "home".to_string(),
            })),
        })),
    };

    let (page, view) = player_return_target(Some(nested), View::Tracks);
    assert_eq!(view, View::Home);
    assert!(matches!(page, Some(HistoryEntry::Home { .. })));
}

#[test]
fn page_history_drops_the_oldest_route_at_its_bound() {
    let mut history = Vec::new();
    for selected in 0..PAGE_HISTORY + 3 {
        push_history(
            &mut history,
            HistoryEntry::Tracks(Box::new(TrackPage {
                results: Vec::new(),
                query: String::new(),
                search_items: Vec::new(),
                search_filter: crate::source::search::Filter::All,
                selected,
                offset: selected,
                browsing: None,
                browsing_endpoint: None,
                collection: None,
                collection_error: None,
                status: format!("page {selected}"),
            })),
        );
    }

    assert_eq!(history.len(), PAGE_HISTORY);
    let HistoryEntry::Tracks(oldest) = &history[0] else {
        panic!("the test only inserted track pages");
    };
    assert_eq!(oldest.selected, 3);
}

#[test]
fn artist_selection_belongs_to_exactly_one_section() {
    let artist = ArtistRef {
        name: "Tame Impala".to_string(),
        endpoint: BrowseEndpoint::new("UCGz-artist"),
    };
    let track = Track {
        id: "letithappen".to_string(),
        title: "Let It Happen".to_string(),
        uploader: artist.name.clone(),
        duration: Some(Duration::from_secs(468)),
        album: Some("Currents".to_string()),
        artist_ref: Some(artist.clone()),
    };
    let mut view = ArtistView {
        requested: artist.clone(),
        page: Panel::Ready(ArtistPage {
            artist: artist.clone(),
            audience: None,
            description: None,
            art: None,
            top_songs: vec![ArtistSong { track, plays: None }],
            songs_endpoint: None,
            songs_truncated: false,
            shelves: vec![Shelf {
                title: "Fans might also like".to_string(),
                cards: vec![Card {
                    title: "Metronomy".to_string(),
                    subtitle: "Artist".to_string(),
                    art: None,
                    duration: None,
                    artist_ref: None,
                    target: Target::Artist { artist },
                }],
            }],
        }),
        section: 0,
        song: 0,
        song_offset: 0,
        card: 0,
        top: 0,
        scroll: vec![0],
        songs_expanded: false,
        songs_request: None,
        songs_error: None,
    };

    assert!(view.selected_song().is_some());
    assert!(view.selected_card().is_none());
    view.section = 1;
    assert!(view.selected_song().is_none());
    assert_eq!(
        view.selected_card().map(|card| card.title.as_str()),
        Some("Metronomy")
    );
}

#[test]
fn player_tabs_use_the_number_key_order_and_title_case_labels() {
    assert_eq!(
        Tab::ALL,
        [Tab::UpNext, Tab::Lyrics, Tab::Related, Tab::Comments]
    );
    assert_eq!(
        Tab::ALL.map(Tab::label),
        ["Queue", "Lyrics", "Related", "Comments"]
    );
    assert_eq!(Tab::UpNext.shifted(-1), Tab::Comments);
    assert_eq!(Tab::Comments.shifted(1), Tab::UpNext);
}

#[test]
fn pointer_positions_cover_the_whole_track() {
    let total = Duration::from_secs(240);

    assert_eq!(pointer_fraction(0), 0.0);
    assert_eq!(pointer_fraction(POINTER_SCALE / 2), 0.5);
    assert_eq!(pointer_fraction(POINTER_SCALE), 1.0);
    assert_eq!(duration_at_pointer(total, 0), Duration::ZERO);
    assert_eq!(
        duration_at_pointer(total, POINTER_SCALE / 2),
        Duration::from_secs(120)
    );
    assert_eq!(
        duration_at_pointer(total, POINTER_SCALE),
        Duration::from_secs(240)
    );
    assert_eq!(
        duration_at_pointer(total, u16::MAX),
        Duration::from_secs(240),
        "out-of-range input is clamped"
    );
}

#[test]
fn mute_restores_the_last_audible_volume() {
    let (muted, remembered) = mute_transition(false, 0.65, 1.0);
    assert_eq!(muted, 0.0);
    assert_eq!(remembered, 0.65);

    let (restored, remembered) = mute_transition(true, 0.0, remembered);
    assert_eq!(restored, 0.65);
    assert_eq!(remembered, 0.65);

    let (restored_default, _) = mute_transition(true, 0.0, 0.0);
    assert_eq!(restored_default, VOLUME_STEP);
    assert_eq!(mute_transition(true, 0.0, 1.63).0, 1.0);
}

#[test]
fn playback_failures_are_reduced_to_actionable_categories() {
    assert_eq!(
        playback_failure_summary("HTTP 403 while reading the signed URL"),
        "YouTube session needs attention"
    );
    assert_eq!(
        playback_failure_summary("Video unavailable: private video"),
        "track is unavailable"
    );
    assert_eq!(
        playback_failure_summary("chunk request timed out"),
        "network interrupted playback"
    );
    assert_eq!(
        playback_failure_summary("decoder rejected the audio codec"),
        "audio format could not be played"
    );
    assert_eq!(
        playback_failure_summary("resolver unavailable"),
        "playback service is unavailable"
    );
    assert_eq!(
        playback_failure_summary("unexpected response"),
        "track could not be played"
    );
    assert_eq!(
        playback_failure_status("track is unavailable"),
        "track is unavailable — r retry · n skip"
    );
}

#[test]
fn a_pasted_url_adopts_the_real_queue_metadata() {
    let artist = ArtistRef {
        name: "Daft Punk".to_string(),
        endpoint: BrowseEndpoint::new("UCdaft"),
    };
    let mut now = NowPlaying::new(&Track {
        id: "JhulBGMA7G4".to_string(),
        title: "https://music.youtube.com/watch?v=JhulBGMA7G4".to_string(),
        uploader: String::new(),
        duration: None,
        album: None,
        artist_ref: None,
    });
    now.queue = vec![Track {
        id: "JhulBGMA7G4".to_string(),
        title: "Harder, Better, Faster, Stronger".to_string(),
        uploader: "Daft Punk".to_string(),
        duration: Some(Duration::from_secs(224)),
        album: Some("Discovery".to_string()),
        artist_ref: Some(artist.clone()),
    }];
    now.playing = Some(0);

    let _ = now.hydrate_from_queue();

    assert_eq!(now.title, "Harder, Better, Faster, Stronger");
    assert_eq!(now.artist, "Daft Punk");
    assert_eq!(now.album.as_deref(), Some("Discovery"));
    assert_eq!(now.duration, Some(Duration::from_secs(224)));
    assert_eq!(now.artist_ref, Some(artist));
    assert!(now.lyrics_query().is_some());
}

#[test]
fn bare_menu_keys_allow_shift_but_not_control_or_alt() {
    assert!(is_bare_character(
        KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT),
        '?'
    ));
    assert!(!is_bare_character(
        KeyEvent::new(KeyCode::Char('.'), KeyModifiers::CONTROL),
        '.'
    ));
    assert!(!is_bare_character(
        KeyEvent::new(KeyCode::Char('?'), KeyModifiers::ALT),
        '?'
    ));
    assert!(is_ctrl_key(
        KeyEvent::new(
            KeyCode::Char('K'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        ),
        'k'
    ));
}

#[test]
fn a_search_started_on_player_returns_to_the_page_behind_player() {
    assert_eq!(page_behind_search(View::Home, View::Tracks), View::Home);
    assert_eq!(
        page_behind_search(View::Playing, View::Artist),
        View::Artist
    );
}

fn playing() -> NowPlaying {
    NowPlaying::new(&Track {
        id: "letithappen".to_string(),
        title: "Let It Happen".to_string(),
        uploader: "Tame Impala".to_string(),
        duration: Some(Duration::from_secs(468)),
        album: Some("Currents".to_string()),
        artist_ref: None,
    })
}

#[test]
fn a_new_page_follows_the_singer_until_told_otherwise() {
    assert!(
        playing().follow_lyrics,
        "a track just started is one nobody has scrolled yet"
    );
}

#[test]
fn scrolling_the_lyrics_hands_them_back_to_the_user() {
    let mut now = playing();
    now.open(Tab::Lyrics);

    now.scroll(3);
    assert_eq!(now.cursor(), 3);
    assert!(
        !now.follow_lyrics,
        "a panel that yanks itself back to the singer cannot be read ahead in"
    );

    // Bounded below only: the far end needs the wrapped length, which is
    // the renderer's to know.
    now.scroll(-10);
    assert_eq!(now.cursor(), 0);
}

#[test]
fn jumping_to_an_end_hands_them_back_too() {
    let mut now = playing();
    now.open(Tab::Lyrics);

    // What `G` asks for: a line no panel has, for the renderer to clamp.
    now.jump(usize::MAX);
    assert_eq!(now.cursor(), usize::MAX);
    assert!(!now.follow_lyrics);

    now.follow_lyrics = true;
    now.jump(0);
    assert!(!now.follow_lyrics, "`g` is a scroll like any other");
}

#[test]
fn opening_the_lyrics_tab_again_is_how_you_get_back_to_the_song() {
    let mut now = playing();
    assert!(now.open(Tab::Lyrics), "that was a change of tab");
    now.scroll(20);
    assert!(!now.follow_lyrics);

    // Pressing `2` while already on Lyrics: not a change of tab, so no
    // fetch is started, but it is the gesture that means "show me where we
    // are" -- and it needs no key of its own.
    assert!(!now.open(Tab::Lyrics), "the tab did not change");
    assert!(now.follow_lyrics, "the panel should be following again");
    assert_eq!(now.cursor(), 20, "the cursor is the renderer's to move");
}

#[test]
fn the_other_tabs_leave_the_lyrics_alone() {
    let mut now = playing();
    now.open(Tab::Lyrics);
    now.scroll(20);

    // Walking away and back through another tab does re-arm it, because
    // arriving on Lyrics is the gesture; walking *past* it does not.
    assert!(now.open(Tab::Comments));
    assert!(!now.follow_lyrics);
    now.scroll(5);
    assert_eq!(now.cursor(), 5, "each tab keeps its own cursor");
    assert_eq!(
        now.cursor[Tab::Lyrics.index()],
        20,
        "the lyrics cursor is where it was left"
    );
}

/// One numbered track, so a queue and the pages appended to it can be
/// built out of ids that never accidentally collide.
fn numbered(i: usize) -> Track {
    Track {
        id: i.to_string(),
        title: format!("track {i}"),
        uploader: "Tame Impala".to_string(),
        duration: Some(Duration::from_secs(180)),
        album: None,
        artist_ref: None,
    }
}

/// A queue of `count` tracks, ids "0", "1", ...
fn queued(count: usize) -> Vec<Track> {
    (0..count).map(numbered).collect()
}

#[test]
fn a_page_of_tracks_lands_on_the_end_of_the_queue() {
    let mut now = playing();
    now.queue = queued(3);
    now.playing = Some(0);

    assert_eq!(now.remaining(), 2);
    assert_eq!(now.absorb(queued(6).split_off(3)), 3);
    assert_eq!(now.queue.len(), 6);
    assert_eq!(
        now.remaining(),
        5,
        "the new tracks are ahead of the playing one"
    );
    assert_eq!(now.playing, Some(0), "appending must not move the cursor");
}

/// A radio repeats over hours. An endless queue that re-appends what it just
/// played is a loop, which is worse than a queue that ends honestly.
#[test]
fn a_page_of_tracks_already_held_is_not_appended_twice() {
    let mut now = playing();
    now.queue = queued(3);
    now.playing = Some(0);

    assert_eq!(now.absorb(queued(3)), 0, "every track was already held");
    assert_eq!(now.queue.len(), 3);

    // Half repeats, half new: only the new half lands.
    let mut page = queued(5);
    page.drain(..2);
    assert_eq!(now.absorb(page), 2);
    assert_eq!(now.queue.len(), 5);
}

#[test]
fn explicit_queue_actions_share_the_radio_memory_bound() {
    let mut now = playing();
    now.queue = queued(QUEUE_BEHIND + 1 + QUEUE_AHEAD);
    now.playing = Some(QUEUE_BEHIND);
    let old_tail = now.queue.last().unwrap().id.clone();

    assert!(now.insert_user_track(numbered(999), true));

    assert_eq!(now.queue.len(), QUEUE_BEHIND + 1 + QUEUE_AHEAD);
    assert_eq!(now.queue[QUEUE_BEHIND + 1].id, "999");
    assert!(
        now.queue.iter().all(|track| track.id != old_tail),
        "the farthest recommendation should yield to the user choice"
    );
}

#[test]
fn adding_to_queue_uses_the_tail_and_rejects_duplicates() {
    let mut now = playing();
    now.queue = queued(4);
    now.playing = Some(0);

    assert!(now.insert_user_track(numbered(9), false));
    assert_eq!(now.queue.last().unwrap().id, "9");
    assert!(!now.insert_user_track(numbered(9), true));
    assert_eq!(now.queue.iter().filter(|track| track.id == "9").count(), 1);
}

#[test]
fn queue_edits_apply_only_after_the_playing_track() {
    let mut now = playing();
    now.queue = queued(6);
    now.playing = Some(2);

    now.cursor[Tab::UpNext.index()] = 2;
    assert!(now.remove_selected_upcoming().is_none());
    assert!(!now.move_selected_upcoming(1));

    now.cursor[Tab::UpNext.index()] = 4;
    assert_eq!(now.remove_selected_upcoming().unwrap().id, "4");
    assert_eq!(
        now.queue
            .iter()
            .map(|track| track.id.as_str())
            .collect::<Vec<_>>(),
        vec!["0", "1", "2", "3", "5"]
    );

    assert!(now.move_selected_upcoming(-1));
    assert_eq!(now.cursor(), 3);
    assert_eq!(now.queue[3].id, "5");
    assert!(
        !now.move_selected_upcoming(-1),
        "an upcoming row cannot cross the current track"
    );
}

#[test]
fn clearing_upcoming_keeps_history_and_the_current_track() {
    let mut now = playing();
    now.queue = queued(8);
    now.playing = Some(3);
    now.cursor[Tab::UpNext.index()] = 7;

    assert_eq!(now.clear_upcoming(), 4);
    assert_eq!(
        now.queue
            .iter()
            .map(|track| track.id.as_str())
            .collect::<Vec<_>>(),
        vec!["0", "1", "2", "3"]
    );
    assert_eq!(now.playing, Some(3));
    assert_eq!(now.cursor(), 3);
}

#[test]
fn shuffling_upcoming_keeps_current_and_selected_tracks() {
    let mut now = playing();
    now.queue = queued(12);
    now.playing = Some(3);
    now.cursor[Tab::UpNext.index()] = 8;

    let before = now
        .queue
        .iter()
        .map(|track| track.id.clone())
        .collect::<Vec<_>>();
    assert_eq!(now.shuffle_upcoming(), 8);
    assert_eq!(
        now.queue[..=3]
            .iter()
            .map(|track| track.id.as_str())
            .collect::<Vec<_>>(),
        vec!["0", "1", "2", "3"]
    );
    assert_eq!(now.queue[now.cursor()].id, "8");

    let mut after = now
        .queue
        .iter()
        .map(|track| track.id.clone())
        .collect::<Vec<_>>();
    let mut before = before;
    before.sort();
    after.sort();
    assert_eq!(after, before);
}

#[test]
fn repeat_modes_cycle_in_the_order_shown_to_the_user() {
    let mut repeat = RepeatMode::Off;
    repeat = repeat.next();
    assert_eq!((repeat, repeat.label()), (RepeatMode::All, "all"));
    repeat = repeat.next();
    assert_eq!((repeat, repeat.label()), (RepeatMode::One, "one"));
    assert_eq!(repeat.next(), RepeatMode::Off);
}

#[test]
fn repeat_modes_change_only_automatic_queue_edges() {
    let mut now = playing();
    now.queue = queued(4);
    now.playing = Some(3);

    now.repeat = RepeatMode::One;
    assert_eq!(now.advanced_index(1, true), Some(3));
    assert_eq!(now.advanced_index(-1, false), Some(2));
    assert_eq!(now.advanced_index(1, false), None);

    now.repeat = RepeatMode::All;
    assert_eq!(now.advanced_index(1, true), Some(0));
    now.playing = Some(0);
    assert_eq!(now.advanced_index(-1, false), Some(3));

    now.topping_up = true;
    now.playing = Some(3);
    assert_eq!(
        now.advanced_index(1, true),
        None,
        "a late page should land before repeat-all wraps"
    );
}

#[test]
fn a_queue_still_loading_does_not_accept_an_orphaned_track() {
    let mut now = playing();
    now.queue = queued(2);

    assert!(!now.insert_user_track(numbered(9), true));
    assert_eq!(
        now.queue
            .iter()
            .map(|track| track.id.as_str())
            .collect::<Vec<_>>(),
        vec!["0", "1"]
    );
}

#[test]
fn shelf_shuffle_keeps_exactly_the_same_bounded_tracks() {
    let mut tracks = queued(QUEUE_AHEAD + 1);
    let mut ids: Vec<String> = tracks.iter().map(|track| track.id.clone()).collect();

    shuffle_tracks(&mut tracks);

    let mut shuffled: Vec<String> = tracks.into_iter().map(|track| track.id).collect();
    ids.sort();
    shuffled.sort();
    assert_eq!(shuffled, ids);
}

/// The window is what keeps an endless queue from being an endless `Vec`.
/// Both indices have to move with it, or the queue silently jumps.
#[test]
fn the_queue_trims_behind_the_playing_track() {
    let mut now = playing();
    now.queue = queued(QUEUE_BEHIND + 10);
    now.playing = Some(QUEUE_BEHIND + 5);
    now.cursor[Tab::UpNext.index()] = QUEUE_BEHIND + 5;

    now.trim();

    assert_eq!(now.queue.len(), QUEUE_BEHIND + 5);
    assert_eq!(now.playing, Some(QUEUE_BEHIND));
    assert_eq!(now.cursor[Tab::UpNext.index()], QUEUE_BEHIND);
    assert_eq!(
        now.queue[QUEUE_BEHIND].id, "25",
        "the playing track is still the one under the index"
    );
    assert_eq!(now.remaining(), 4, "and what was ahead is untouched");
}

/// Trimming is what makes a track eligible to be offered again, so what
/// leaves the window has to be remembered or the radio simply re-queues it.
#[test]
fn a_track_trimmed_away_is_not_offered_back() {
    let mut now = playing();
    now.queue = queued(QUEUE_BEHIND + 2);
    now.playing = Some(QUEUE_BEHIND + 1);
    now.trim();

    assert!(now.dropped.contains(&"0".to_string()));
    assert_eq!(now.absorb(queued(1)), 0, "track 0 has already been played");
    assert_eq!(now.queue.len(), QUEUE_BEHIND + 1);
}

#[test]
fn a_short_queue_is_left_alone_by_the_trim() {
    let mut now = playing();
    now.queue = queued(5);
    now.playing = Some(4);
    now.cursor[Tab::UpNext.index()] = 4;

    now.trim();

    assert_eq!(
        now.queue.len(),
        5,
        "nothing has fallen out of the window yet"
    );
    assert_eq!(now.playing, Some(4));
    assert!(now.dropped.is_empty());
}

/// The whole claim of an endless queue: it can be played through for as
/// long as anyone likes without growing. Walked here the way a session
/// walks it -- top up when the tail runs low, advance one, trim behind --
/// for far more tracks than either bound holds.
#[test]
fn playing_through_an_endless_queue_does_not_grow_it() {
    let mut now = playing();
    now.queue = queued(1);
    now.playing = Some(0);

    let mut next = 1;
    for _ in 0..(QUEUE_MEMORY * 4) {
        if now.remaining() < QUEUE_LOW {
            // A page, as a continuation delivers one.
            let page: Vec<Track> = (next..next + 25).map(numbered).collect();
            next += 25;
            assert!(now.absorb(page) > 0, "a page of new tracks must land");
        }
        now.playing = Some(now.playing.unwrap() + 1);
        now.trim();
    }

    assert!(
        now.dropped.len() <= QUEUE_MEMORY,
        "remembered {} ids, over the {QUEUE_MEMORY} bound",
        now.dropped.len()
    );
    assert!(
        now.queue.len() <= QUEUE_BEHIND + 1 + QUEUE_AHEAD,
        "held {} tracks, wider than the window",
        now.queue.len()
    );
    assert_eq!(
        now.playing,
        Some(QUEUE_BEHIND),
        "the window settles with the playing track a fixed way into it"
    );
}

/// Nothing playing inside the queue is not the same as a queue with plenty
/// left: reporting its length would top up a queue standing still.
#[test]
fn a_queue_nothing_is_playing_in_has_nothing_remaining() {
    let mut now = playing();
    now.queue = queued(30);
    now.playing = None;

    assert_eq!(now.remaining(), 0);
}

#[test]
fn audio_outputs_cycle_through_the_system_default() {
    let outputs = vec![
        OutputDevice {
            id: "speakers".to_string(),
            name: "Speakers".to_string(),
        },
        OutputDevice {
            id: "headphones".to_string(),
            name: "Headphones".to_string(),
        },
    ];

    assert_eq!(
        next_output_device(None, &outputs, true).as_deref(),
        Some("speakers")
    );
    assert_eq!(
        next_output_device(Some("speakers"), &outputs, true).as_deref(),
        Some("headphones")
    );
    assert_eq!(next_output_device(Some("headphones"), &outputs, true), None);
    assert_eq!(
        next_output_device(None, &outputs, false).as_deref(),
        Some("headphones")
    );
    // A disconnected saved device starts cycling from the usable default.
    assert_eq!(
        next_output_device(Some("missing"), &outputs, true).as_deref(),
        Some("speakers")
    );
}

/// A continuation that carried the queue forward is trusted about where the
/// next page lives, and leaves the station's name alone.
#[test]
fn a_page_that_carried_the_queue_forward_is_paged_again() {
    let mut now = playing();
    now.queue = queued(3);
    now.playing = Some(0);
    now.queue_title = "Let It Happen Mix".to_string();
    now.continuation = Some("FIRST".to_string());
    now.topup_failures = 2;

    let added = now.take_page(QueuePage {
        tracks: queued(8).split_off(3),
        continuation: Some("SECOND".to_string()),
        title: None,
    });

    assert_eq!(added, 5);
    assert_eq!(now.continuation.as_deref(), Some("SECOND"));
    assert_eq!(
        now.topup_failures, 0,
        "a page that worked clears what earlier failures had counted up"
    );
    assert_eq!(
        now.queue_title, "Let It Happen Mix",
        "more of the same station must not rename it"
    );
}

/// A page whose every track is already held teaches the queue nothing, and
/// the token that bought it will buy the same page again. Giving it up is
/// what lets the next top-up fall through to the journal instead of
/// spending a round trip per track re-learning the same thing.
#[test]
fn a_page_of_nothing_new_gives_up_the_token() {
    let mut now = playing();
    now.queue = queued(3);
    now.playing = Some(0);
    now.continuation = Some("TOKEN".to_string());

    let added = now.take_page(QueuePage {
        tracks: queued(3),
        continuation: Some("WOULD_BUY_THE_SAME_AGAIN".to_string()),
        title: None,
    });

    assert_eq!(added, 0);
    assert_eq!(now.queue.len(), 3, "nothing was appended");
    assert!(
        now.continuation.is_none(),
        "the offered token would buy this same page again"
    );
    assert_eq!(
        now.topup_failures, 1,
        "a station repeating itself has to count against the budget too"
    );
}

/// The handover to the journal: a station built locally is a different
/// station, and the panel has to say so rather than keep the old name over
/// entirely different music.
#[test]
fn a_seeded_station_renames_the_queue_and_is_paged_from_there() {
    let mut now = playing();
    now.queue = queued(2);
    now.playing = Some(1);
    now.queue_title = "Let It Happen Mix".to_string();
    // Tier one has already given up: this is the fallback landing.
    now.continuation = None;
    now.topup_failures = 1;

    let added = now.take_page(QueuePage {
        tracks: queued(20).split_off(2),
        continuation: Some("STATION_PAGE_TWO".to_string()),
        title: Some("Station from Currents".to_string()),
    });

    assert_eq!(added, 18);
    assert_eq!(now.queue_title, "Station from Currents");
    assert_eq!(
        now.continuation.as_deref(),
        Some("STATION_PAGE_TWO"),
        "the new station is YouTube's to page from here on"
    );
    assert_eq!(now.topup_failures, 0);
}

/// The window has a ceiling as well as a floor.
#[test]
fn the_queue_does_not_grow_past_the_window() {
    let mut now = playing();
    now.queue = queued(2);
    now.playing = Some(0);

    let page: Vec<Track> = queued(QUEUE_AHEAD + 20).split_off(2);
    now.absorb(page);

    assert_eq!(now.queue.len(), QUEUE_AHEAD + 1);
    assert_eq!(now.remaining(), QUEUE_AHEAD);
}

/// A top-up arrives below the low-water mark and may contain sixty rows.
/// The continuation it carries starts after all sixty, so all must fit or
/// the omitted tail can never be requested again.
#[test]
fn a_complete_continuation_page_fits_at_the_low_water_mark() {
    let mut now = playing();
    now.queue = queued(QUEUE_LOW);
    now.playing = Some(0);

    let page: Vec<Track> = (QUEUE_LOW..QUEUE_LOW + 60).map(numbered).collect();
    assert_eq!(now.absorb(page), 60);
    assert_eq!(now.remaining(), QUEUE_LOW - 1 + 60);
}
