//! Romaji user actions with controlled provider completion and no network.
use super::*;
use super::scenarios::fixture;
use crate::source::watch::TimedLine;

fn original() -> Lyrics {
    Lyrics { text: "ねこ\nそら".into(), source: Some("Original provider".into()), timed: vec![
        TimedLine { text: "ねこ".into(), start: Duration::from_secs(1) },
        TimedLine { text: "そら".into(), start: Duration::from_secs(4) },
    ] }
}

#[test]
fn romaji_fetch_is_opt_in_and_cached_toggles_preserve_original_lyrics() {
    let (mut app, _driver, requests) = fixture();
    app.play_track(app.results[0].clone(), false);
    requests.try_iter().for_each(drop);
    let now = app.now.as_mut().unwrap();
    now.watched = true;
    now.lyrics = Panel::Ready(original());
    now.open(Tab::Lyrics);
    app.tick_page();
    assert!(requests.try_iter().next().is_none());

    app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE)).unwrap();
    assert!(matches!(requests.try_recv().unwrap(), Request::Romaji { video_id, original: Some(held), .. }
        if video_id == "song-0" && held.text == "ねこ\nそら"));
    assert!(matches!(app.now.as_ref().unwrap().shown_lyrics(), Panel::Loading));
    app.tick_page();
    assert!(requests.try_iter().next().is_none());

    app.apply(Response::Romaji { video_id: "song-0".into(), lyrics: Ok(Lyrics {
        text: "neko\nsora".into(), source: Some("Romaji provider (unsynced)".into()), timed: Vec::new(),
    }) });
    let now = app.now.as_mut().unwrap();
    now.follow_lyrics = false;
    assert!(matches!(&now.lyrics, Panel::Ready(held) if held.timed.len() == 2));
    app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE)).unwrap();
    assert!(!app.now.as_ref().unwrap().follow_lyrics, "unsynced romaji has no follow action");
    app.handle_mouse_action(MouseAction::ToggleLyricsRomaji).unwrap();
    assert!(matches!(app.now.as_ref().unwrap().shown_lyrics(), Panel::Ready(held) if held.text == "ねこ\nそら"));
    app.handle_mouse_action(MouseAction::ToggleLyricsRomaji).unwrap();
    assert!(matches!(app.now.as_ref().unwrap().shown_lyrics(), Panel::Ready(held) if held.text == "neko\nsora"));
    assert!(requests.try_iter().next().is_none(), "a cached alternate must not be fetched again");
}

#[test]
fn original_completion_precedes_romaji_matching_and_new_tracks_discard_old_answers() {
    let (mut app, _driver, requests) = fixture();
    app.play_track(app.results[0].clone(), false);
    requests.try_iter().for_each(drop);
    let now = app.now.as_mut().unwrap();
    now.watched = true;
    now.lyrics = Panel::Loading;
    now.open(Tab::Lyrics);
    app.handle_mouse_action(MouseAction::ToggleLyricsRomaji).unwrap();
    assert!(requests.try_iter().next().is_none(), "wait for original lyrics to verify provider timing");
    app.apply(Response::Lyrics { video_id: "song-0".into(), lyrics: Ok(original()) });
    app.tick_page();
    assert!(matches!(requests.try_recv().unwrap(), Request::Romaji { original: Some(_), .. }));

    app.play_track(app.results[1].clone(), false);
    app.apply(Response::Romaji { video_id: "song-0".into(), lyrics: Ok(original()) });
    let now = app.now.as_ref().unwrap();
    assert!(now.lyrics_romaji, "the representation preference travels to the next song");
    assert!(now.romaji.is_idle(), "a delayed answer cannot fill another song's panel");
    assert_eq!(now.video_id, "song-1");
}

#[test]
fn empty_romaji_can_be_retried_explicitly_and_hidden_controls_are_ignored() {
    let (mut app, _driver, requests) = fixture();
    app.play_track(app.results[0].clone(), false);
    requests.try_iter().for_each(drop);
    let now = app.now.as_mut().unwrap();
    now.watched = true;
    now.lyrics = Panel::Ready(original());
    now.romaji = Panel::Empty("provider temporarily unavailable".into());
    now.open(Tab::Lyrics);
    app.handle_mouse_action(MouseAction::ToggleLyricsRomaji).unwrap();
    assert!(matches!(requests.try_recv().unwrap(), Request::Romaji { .. }));
    app.now.as_mut().unwrap().open(Tab::UpNext);
    app.handle_mouse_action(MouseAction::ToggleLyricsRomaji).unwrap();
    assert!(app.now.as_ref().unwrap().lyrics_romaji);
    assert!(requests.try_iter().next().is_none());
}
