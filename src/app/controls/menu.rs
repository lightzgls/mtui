//! Menu state, available commands, and command execution.

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::runtime::config;
use crate::player::PlayState;
use crate::source::catalog::home::{Card, Target};

use crate::app::App;
use crate::app::browse::artist::card_artist;
use crate::app::artwork::CoverSize;
use crate::app::controls::input::{is_bare_character, is_ctrl_key};
use crate::app::browse::navigation::{View, moved_cursor};
use crate::app::playback::now_playing::{Panel, RelatedRow, Tab};

/// Which page the shared modal menu is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuPage {
    Root,
    Account,
    Help,
    PageActions,
    PlayerActions,
}

impl MenuPage {
    pub fn title(self) -> &'static str {
        match self {
            Self::Root => "Menu",
            Self::Account => "Account",
            Self::Help => "Keyboard shortcuts",
            Self::PageActions => "Page Actions",
            Self::PlayerActions => "Player actions",
        }
    }
}

/// Cursor state for the one menu overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Menu {
    pub page: MenuPage,
    pub selected: usize,
    pub(in crate::app) items: Vec<MenuItem>,
}

impl Menu {
    pub(in crate::app) fn new(page: MenuPage, items: Vec<MenuItem>) -> Self {
        Self {
            page,
            selected: 0,
            items,
        }
    }

    pub(in crate::app) fn move_by(&mut self, delta: isize, len: usize) {
        let mut next = moved_cursor(self.selected, delta, len);
        while self.page != MenuPage::Help && self.items.get(next).is_some_and(|item| !item.enabled) {
            let following = moved_cursor(next, delta.signum(), len);
            if following == next { return; }
            next = following;
        }
        self.selected = next;
    }

    pub(in crate::app) fn shortcut_index(&self, key: KeyEvent) -> Option<usize> {
        self.items.iter().position(|item| {
            item.enabled && item.action.is_some() && (
                item.shortcut.is_some_and(|shortcut| menu_shortcut_matches(shortcut, key))
                || (item.action == Some(MenuAction::OpenSettings) && is_bare_character(key, 'S'))
            )
        })
    }

    pub(in crate::app) fn first(&mut self) {
        self.selected = 0;
    }

    pub(in crate::app) fn last(&mut self, len: usize) {
        self.selected = len.saturating_sub(1);
    }

    pub(in crate::app) fn clamp(&mut self, len: usize) {
        self.selected = self.selected.min(len.saturating_sub(1));
    }
}

/// A renderer-facing menu row. The semantic action stays private so drawing and
/// execution cannot grow separate command maps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuItem {
    pub label: String,
    pub shortcut: Option<&'static str>,
    pub enabled: bool,
    /// A heading to draw immediately before this row.
    pub section: Option<&'static str>,
    pub(in crate::app) action: Option<MenuAction>,
}

impl MenuItem {
    pub fn opens_panel(&self) -> bool {
        matches!(self.action, Some(MenuAction::OpenAccount | MenuAction::OpenSettings | MenuAction::OpenHelp))
    }

    pub(in crate::app) fn action(
        label: impl Into<String>,
        shortcut: Option<&'static str>,
        enabled: bool,
        section: Option<&'static str>,
        action: MenuAction,
    ) -> Self {
        Self {
            label: label.into(),
            shortcut,
            enabled,
            section,
            action: Some(action),
        }
    }

    pub(in crate::app) fn help(label: &'static str, shortcut: &'static str, section: Option<&'static str>) -> Self {
        Self {
            label: label.to_string(),
            shortcut: Some(shortcut),
            enabled: false,
            section,
            action: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum MenuAction {
    GoHome,
    OpenLibrary,
    OpenLibrarySelection,
    OpenLikedMusic,
    SortLibrary,
    MoreLibrary,
    RefreshLibrary,
    BeginSearch,
    OpenPlayer,
    OpenAccount,
    OpenSettings,
    OpenHelp,
    #[cfg(windows)]
    Background,
    Quit,
    ConnectMusic,
    LogOutMusic,
    OpenHomeSelection,
    StartHomeRadio,
    PlaySelectionNext,
    QueueSelection,
    PlayHomeShelf,
    ShuffleHomeShelf,
    RefreshHome,
    PlaySelected,
    OpenArtist,
    OpenArtistSelection,
    ReloadArtist,
    MoreArtistSongs,
    PlayArtistSongs,
    OpenPageSelection,
    RemoveQueueSelection,
    MoveQueueSelectionUp,
    MoveQueueSelectionDown,
    ClearUpcomingQueue,
    ShuffleUpcomingQueue,
    CycleRepeat,
    FollowLyrics,
    TogglePause,
    ToggleMute,
    RetryTrack,
    Next,
    Previous,
    Stop,
    ToggleCoverSize,
    LikePlaying,
    SharePlaying,
    SavePlaying,
    ChooseOutput,
}

pub(in crate::app) fn menu_shortcut_matches(shortcut: &str, key: KeyEvent) -> bool {
    match shortcut {
        "Space" => is_bare_character(key, ' '),
        "Ctrl+S" => is_ctrl_key(key, 's'),
        "Ctrl+C" => is_ctrl_key(key, 'c'),
        "Alt+Up" => key.code == KeyCode::Up && key.modifiers.contains(KeyModifiers::ALT),
        "Alt+Down" => key.code == KeyCode::Down && key.modifiers.contains(KeyModifiers::ALT),
        _ => {
            let mut characters = shortcut.chars();
            characters.next().is_some_and(|character| {
                characters.next().is_none() && is_bare_character(key, character)
            })
        }
    }
}

pub(crate) fn app_menu_items(has_player: bool) -> Vec<MenuItem> {
    let mut items = vec![
        MenuItem::action("Home", Some("H"), true, Some("Go to"), MenuAction::GoHome),
        MenuItem::action("Library", Some("L"), true, None, MenuAction::OpenLibrary),
        MenuItem::action("Search", Some("/"), true, None, MenuAction::BeginSearch),
    ];
    items.extend([
        MenuItem::action(
            "Now Playing",
            Some("P"),
            has_player,
            None,
            MenuAction::OpenPlayer,
        ),
        MenuItem::action(
            "Account",
            None,
            true,
            Some("App"),
            MenuAction::OpenAccount,
        ),
        MenuItem::action("Settings", Some("Ctrl+S"), true, None, MenuAction::OpenSettings),
        MenuItem::action("Keyboard shortcuts", Some("?"), true, None, MenuAction::OpenHelp),
    ]);
    #[cfg(windows)]
    items.push(MenuItem::action(
        "Minimize to tray",
        Some("B"),
        has_player,
        Some("Window"),
        MenuAction::Background,
    ));
    items.push(MenuItem::action(
        "Quit",
        Some("Ctrl+C"),
        true,
        None,
        MenuAction::Quit,
    ));
    items
}

pub(crate) fn keyboard_help_items() -> Vec<MenuItem> {
    vec![
        MenuItem::help("Move selection", "j/k, Up/Down", Some("Navigation")),
        MenuItem::help("First or last item", "g/G, Home/End", None),
        MenuItem::help("Open or play", "Enter", None),
        MenuItem::help("Select song / play song", "Click / double-click", None),
        MenuItem::help("Song actions", "Right-click", None),
        MenuItem::help("Go back", "Esc", None),
        MenuItem::help("Search", "/ or i", Some("Pages")),
        MenuItem::help("Home", "H", None),
        MenuItem::help("Library", "L", None),
        MenuItem::help("Library categories", "1–6 / Tab", None),
        MenuItem::help("Library: liked / sort", "f / o", None),
        MenuItem::help("Now Playing", "P", None),
        MenuItem::help("Artist: play all top songs", "a", None),
        MenuItem::help("Artist: show more / less", "e", None),
        MenuItem::help("Album/playlist: play all / shuffle", "a / z", None),
        MenuItem::help("Search: move cursor", "Left/Right, Home/End", None),
        MenuItem::help("Search: delete word / clear", "Ctrl+W / Ctrl+U", None),
        MenuItem::help("Pause or resume", "Space", Some("Playback")),
        MenuItem::help("Next or previous", "n / p", None),
        MenuItem::help("Repeat off / all / one", "R", None),
        MenuItem::help("Player actions", "Ctrl+P", None),
        MenuItem::help("Like / share current song", "Ctrl+L / Ctrl+Y", None),
        MenuItem::help("Save song / audio output", "Ctrl+A / Ctrl+O", None),
        MenuItem::help("Lyrics: expand / restore", "e", None),
        MenuItem::help("Lyrics: pause / resume follow", "f", None),
            MenuItem::help("Queue: remove / shuffle", "d / z", None),
            MenuItem::help("Queue: move row", "Alt+Up/Down, K/J", None),
            MenuItem::help("Queue: clear upcoming", "C", None),
            MenuItem::help("Seek", "Left/Right", None),
        MenuItem::help("Change volume", "+ / -", None),
        MenuItem::help("Mute or restore volume", "m", None),
        MenuItem::help("Stop", "s", None),
        MenuItem::help("App Menu", "Ctrl+K", Some("Global")),
        MenuItem::help("Page Actions (outside search)", ".", None),
        MenuItem::help("Keyboard Help", "?", None),
        MenuItem::help("Settings", "Ctrl+S", None),
        MenuItem::help("Quit immediately", "Ctrl+C", None),
    ]
}

pub(crate) fn account_menu_items(connected: bool, signing_in: bool) -> Vec<MenuItem> {
    let mut items = vec![MenuItem::action(
        if connected {
            "Refresh YouTube Music session"
        } else {
            "Connect YouTube Music"
        },
        Some("M"),
        !signing_in,
        Some("Account"),
        MenuAction::ConnectMusic,
    )];
    if connected {
        items.push(MenuItem::action(
            "Log out of YouTube Music",
            None,
            true,
            None,
            MenuAction::LogOutMusic,
        ));
    }
    items
}

impl App {

    /// State for the active menu overlay, if the keyboard belongs to one.
    pub fn menu(&self) -> Option<&Menu> {
        self.menu.as_ref()
    }


    /// Rows for the active menu. Enter resolves its action from this same list,
    /// so a renderer can never show a command different from the one invoked.
    pub fn menu_items(&self) -> Vec<MenuItem> {
        self.menu()
            .map(|menu| menu.items.clone())
            .unwrap_or_default()
    }


    pub(in crate::app) fn root_menu_items(&self) -> Vec<MenuItem> {
        app_menu_items(self.now.is_some())
    }


    pub(in crate::app) fn account_menu_items(&self) -> Vec<MenuItem> {
        let connected = config::Cookies::available().ok().flatten().is_some();
        account_menu_items(connected, self.session_pending())
    }


    pub(in crate::app) fn help_menu_items(&self) -> Vec<MenuItem> {
        keyboard_help_items()
    }


    pub(in crate::app) fn page_action_items(&self) -> Vec<MenuItem> {
        let mut items = match self.view {
            View::Library => self.library_action_items(),
            View::Home => {
                let selected = self.home_card();
                let (label, enabled) = match selected {
                    Some(card) if matches!(&card.target, Target::Play { .. }) => {
                        ("Play selected item", true)
                    }
                    Some(_) => ("Open selected item", true),
                    None => ("Open selected item", false),
                };
                let playable = selected.is_some_and(Card::is_playable);
                let queue_ready =
                    playable && self.now.as_ref().is_some_and(|now| now.playing.is_some());
                let artist = selected.and_then(card_artist).is_some();
                let playable_in_shelf = self.home.get(self.home_shelf).map_or(0, |shelf| {
                    shelf.cards.iter().filter(|card| card.is_playable()).count()
                });
                vec![
                    MenuItem::action(
                        label,
                        Some("Enter"),
                        enabled,
                        Some("Home"),
                        MenuAction::OpenHomeSelection,
                    ),
                    MenuItem::action(
                        "Start radio from selected song",
                        None,
                        playable,
                        None,
                        MenuAction::StartHomeRadio,
                    ),
                    MenuItem::action(
                        "Play next",
                        None,
                        queue_ready,
                        None,
                        MenuAction::PlaySelectionNext,
                    ),
                    MenuItem::action(
                        "Add to queue",
                        None,
                        queue_ready,
                        None,
                        MenuAction::QueueSelection,
                    ),
                    MenuItem::action(
                        "Open selected artist",
                        None,
                        artist,
                        None,
                        MenuAction::OpenArtist,
                    ),
                    MenuItem::action(
                        "Play this shelf",
                        None,
                        playable_in_shelf > 0,
                        Some("Current shelf"),
                        MenuAction::PlayHomeShelf,
                    ),
                    MenuItem::action(
                        "Shuffle this shelf",
                        None,
                        playable_in_shelf > 1,
                        None,
                        MenuAction::ShuffleHomeShelf,
                    ),
                    MenuItem::action(
                        "Refresh Home",
                        Some("r"),
                        !self.home_pending,
                        Some("Home"),
                        MenuAction::RefreshHome,
                    ),
                ]
            }
            View::Tracks => {
                let selected = self.selected_result_track();
                let mut items = vec![MenuItem::action(
                    if selected.is_some() { "Play selected track" } else { "Open selected result" },
                    Some("Enter"),
                    self.result_count() > 0,
                    Some("Selected track"),
                    MenuAction::PlaySelected,
                )];
                if selected
                    .and_then(|track| track.artist_ref.as_ref())
                    .is_some()
                {
                    items.push(MenuItem::action(
                        "Open selected artist",
                        None,
                        true,
                        None,
                        MenuAction::OpenArtist,
                    ));
                }
                items
            }
            View::Artist => self.artist_action_items(),
            View::Playing => self.playing_action_items(),
        };
        if self.view != View::Home && self.selected_queue_track().is_some() {
            let ready = self.now.as_ref().is_some_and(|now| now.playing.is_some());
            let at = 1.min(items.len());
            items.splice(at..at, [
                MenuItem::action("Play next", None, ready, None, MenuAction::PlaySelectionNext),
                MenuItem::action("Add to queue", None, ready, None, MenuAction::QueueSelection),
            ]);
        }
        items
    }


    pub(in crate::app) fn artist_action_items(&self) -> Vec<MenuItem> {
        let Some(artist) = self.artist.as_ref() else {
            return Vec::new();
        };
        if matches!(artist.page, Panel::Empty(_)) {
            return vec![MenuItem::action(
                "Retry artist page",
                Some("r"),
                true,
                Some("Artist"),
                MenuAction::ReloadArtist,
            )];
        }

        let selected_song = artist.selected_song();
        let selected_card = artist.selected_card();
        let (label, enabled) = match (selected_song, selected_card) {
            (Some(_), _) => ("Play selected top song", true),
            (_, Some(card)) if card.is_playable() => ("Play selected item", true),
            (_, Some(_)) => ("Open selected item", true),
            _ => ("Open selected item", false),
        };
        let mut items = vec![MenuItem::action(
            label,
            Some("Enter"),
            enabled,
            Some("Artist"),
            MenuAction::OpenArtistSelection,
        )];
        items.push(MenuItem::action("Play all top songs", Some("a"), artist.has_songs(), None, MenuAction::PlayArtistSongs));
        items.push(MenuItem::action("Show more top songs", Some("e"), artist.has_songs(), None, MenuAction::MoreArtistSongs));
        items.push(MenuItem::action(
            "Refresh artist page",
            Some("r"),
            !matches!(artist.page, Panel::Loading),
            None,
            MenuAction::ReloadArtist,
        ));
        items
    }


    pub(in crate::app) fn playing_action_items(&self) -> Vec<MenuItem> {
        let mut items = Vec::new();
        let Some(now) = self.now.as_ref() else {
            return items;
        };

        match now.tab {
            Tab::UpNext => {
                let cursor = now.cursor();
                let playing = now.playing;
                let upcoming =
                    playing.is_some_and(|index| cursor > index && cursor < now.queue.len());
                let can_move_up = playing.is_some_and(|index| cursor > index + 1);
                let can_move_down = upcoming && cursor + 1 < now.queue.len();
                items.extend([
                    MenuItem::action(
                        "Play selected queue track",
                        Some("Enter"),
                        now.queue.get(cursor).is_some(),
                        Some("Selection"),
                        MenuAction::OpenPageSelection,
                    ),
                    MenuItem::action(
                        "Remove from queue",
                        Some("d"),
                        upcoming,
                        None,
                        MenuAction::RemoveQueueSelection,
                    ),
                    MenuItem::action(
                        "Move up",
                        Some("Alt+Up"),
                        can_move_up,
                        None,
                        MenuAction::MoveQueueSelectionUp,
                    ),
                    MenuItem::action(
                        "Move down",
                        Some("Alt+Down"),
                        can_move_down,
                        None,
                        MenuAction::MoveQueueSelectionDown,
                    ),
                    MenuItem::action(
                        "Shuffle upcoming",
                        Some("z"),
                        now.remaining() > 1,
                        Some("Queue"),
                        MenuAction::ShuffleUpcomingQueue,
                    ),
                    MenuItem::action(
                        "Clear upcoming",
                        Some("C"),
                        now.remaining() > 0,
                        None,
                        MenuAction::ClearUpcomingQueue,
                    ),
                ]);
            }
            Tab::Related => {
                let (label, enabled) = match now.related_rows().get(now.cursor()) {
                    Some(RelatedRow::Card(card)) if matches!(&card.target, Target::Play { .. }) => {
                        ("Play selected related track", true)
                    }
                    Some(RelatedRow::Card(_)) => ("Open selected related item", true),
                    _ => ("Open selected related item", false),
                };
                items.push(MenuItem::action(
                    label,
                    Some("Enter"),
                    enabled,
                    Some("Selection"),
                    MenuAction::OpenPageSelection,
                ));
            }
            Tab::Lyrics if matches!(&now.lyrics, Panel::Ready(lyrics) if !lyrics.timed.is_empty()) =>
            {
                items.push(MenuItem::action(
                    "Follow current lyric",
                    Some("2"),
                    true,
                    Some("Lyrics"),
                    MenuAction::FollowLyrics,
                ));
            }
            Tab::Lyrics | Tab::Comments => {}
        }

        let snapshot = self.snapshot();
        let failed = self.playback_failed(&snapshot);
        let current = now.playing;
        let can_previous =
            current.is_some_and(|index| index > 0 && now.queue.get(index - 1).is_some());
        let can_next = current.is_some_and(|index| now.queue.get(index + 1).is_some());
        items.extend([
            MenuItem::action(
                "Retry current track",
                Some("r"),
                failed,
                Some("Playback failure"),
                MenuAction::RetryTrack,
            ),
            MenuItem::action(
                "Open current artist",
                None,
                now.artist_ref.is_some(),
                Some("Current track"),
                MenuAction::OpenArtist,
            ),
            MenuItem::action(
                if snapshot.state == PlayState::Paused {
                    "Resume current track"
                } else {
                    "Pause current track"
                },
                Some("Space"),
                snapshot.state != PlayState::Idle,
                Some("Playback"),
                MenuAction::TogglePause,
            ),
            MenuItem::action(
                if self.muted {
                    "Restore volume"
                } else {
                    "Mute audio"
                },
                Some("m"),
                snapshot.state != PlayState::Idle,
                None,
                MenuAction::ToggleMute,
            ),
            MenuItem::action(
                if failed {
                    "Skip failed track"
                } else {
                    "Play next track"
                },
                Some("n"),
                can_next,
                None,
                MenuAction::Next,
            ),
            MenuItem::action(
                "Play previous track",
                Some("p"),
                can_previous,
                None,
                MenuAction::Previous,
            ),
            MenuItem::action(
                format!("Repeat: {}", now.repeat.label()),
                Some("R"),
                current.is_some(),
                None,
                MenuAction::CycleRepeat,
            ),
            MenuItem::action("Stop playback", Some("s"), true, None, MenuAction::Stop),
            MenuItem::action(
                if self.cover_size == CoverSize::Side {
                    "Show full-size cover"
                } else {
                    "Show side cover"
                },
                Some("c"),
                true,
                None,
                MenuAction::ToggleCoverSize,
            ),
        ]);
        items
    }


    pub(in crate::app) fn open_menu(&mut self, page: MenuPage) {
        if !self.overlay.is_open() {
            let items = self.items_for_menu(page);
            self.menu = Some(Menu::new(page, items));
        }
    }


    pub(in crate::app) fn items_for_menu(&self, page: MenuPage) -> Vec<MenuItem> {
        match page {
            MenuPage::Root => self.root_menu_items(),
            MenuPage::Account => self.account_menu_items(),
            MenuPage::Help => self.help_menu_items(),
            MenuPage::PageActions => self.page_action_items(),
            MenuPage::PlayerActions => self.player_action_items(),
        }
    }


    pub(in crate::app) fn close_page_actions(&mut self) {
        if matches!(
            self.menu.as_ref().map(|menu| menu.page),
            Some(MenuPage::PageActions)
        ) {
            self.menu = None;
        }
    }


    pub(in crate::app) fn toggle_root_menu(&mut self) {
        match self.menu.as_ref().map(|menu| menu.page) {
            Some(MenuPage::Root) => self.menu = None,
            Some(_) => self.open_menu(MenuPage::Root),
            None => self.open_menu(MenuPage::Root),
        }
    }


    pub(in crate::app) fn handle_menu_key(&mut self, key: KeyEvent) -> Result<()> {
        if key.modifiers.contains(KeyModifiers::ALT) && matches!(key.code, KeyCode::Up | KeyCode::Down) {
            if let Some(selected) = self.menu.as_ref().and_then(|menu| menu.shortcut_index(key)) {
                if let Some(menu) = self.menu.as_mut() { menu.selected = selected; }
                self.invoke_menu_item();
            }
            return Ok(());
        }
        let len = self.menu_items().len();
        match key.code {
            KeyCode::Char('j') | KeyCode::Down | KeyCode::Tab => {
                if let Some(menu) = self.menu.as_mut() {
                    menu.move_by(1, len);
                }
            }
            KeyCode::Char('k') | KeyCode::Up | KeyCode::BackTab => {
                if let Some(menu) = self.menu.as_mut() {
                    menu.move_by(-1, len);
                }
            }
            KeyCode::Char('g') | KeyCode::Home => {
                if let Some(menu) = self.menu.as_mut() {
                    menu.first();
                }
            }
            KeyCode::Char('G') | KeyCode::End => {
                if let Some(menu) = self.menu.as_mut() {
                    menu.last(len);
                }
            }
            KeyCode::Enter => self.invoke_menu_item(),
            KeyCode::Esc | KeyCode::Char('q') => {
                if matches!(
                    self.menu.as_ref().map(|menu| menu.page),
                    Some(MenuPage::Account | MenuPage::Help)
                ) {
                    self.open_menu(MenuPage::Root);
                } else {
                    self.menu = None;
                }
            }
            _ => {
                let selected = self.menu.as_ref().and_then(|menu| menu.shortcut_index(key));
                if let Some(selected) = selected {
                    if let Some(menu) = self.menu.as_mut() { menu.selected = selected; }
                    self.invoke_menu_item();
                }
            }
        }
        Ok(())
    }


    pub(in crate::app) fn invoke_menu_item(&mut self) {
        let items = self.menu_items();
        let Some(menu) = self.menu.as_mut() else {
            return;
        };
        menu.clamp(items.len());
        let Some(item) = items.get(menu.selected) else {
            return;
        };
        if !item.enabled {
            return;
        }
        let Some(action) = item.action else {
            return;
        };

        self.menu = None;
        match action {
            MenuAction::GoHome => {
                self.go_home();
            }
            MenuAction::OpenLibrary => self.open_library(),
            MenuAction::OpenLibrarySelection => self.open_library_item(self.library.collection().selected),
            MenuAction::OpenLikedMusic => self.open_liked_music(),
            MenuAction::SortLibrary => self.cycle_library_sort(),
            MenuAction::MoreLibrary => self.request_library(true),
            MenuAction::RefreshLibrary => self.refresh_library(),
            MenuAction::BeginSearch => self.begin_search(),
            MenuAction::OpenPlayer => self.open_player(),
            MenuAction::OpenAccount => self.open_menu(MenuPage::Account),
            MenuAction::OpenSettings => {
                self.open_settings();
                self.preferences.return_to_menu = true;
            }
            MenuAction::OpenHelp => self.open_menu(MenuPage::Help),
            #[cfg(windows)]
            MenuAction::Background => self.background(),
            MenuAction::Quit => self.should_quit = true,
            MenuAction::ConnectMusic => self.begin_music_sign_in(false),
            MenuAction::LogOutMusic => self.log_out_music(),
            MenuAction::OpenHomeSelection => self.open_card(),
            MenuAction::StartHomeRadio => self.start_home_radio(),
            MenuAction::PlaySelectionNext => self.queue_selected_track(true),
            MenuAction::QueueSelection => self.queue_selected_track(false),
            MenuAction::PlayHomeShelf => self.play_home_shelf(false),
            MenuAction::ShuffleHomeShelf => self.play_home_shelf(true),
            MenuAction::RefreshHome => self.refresh_home(),
            MenuAction::PlaySelected => self.play_selected(),
            MenuAction::OpenArtist => self.open_context_artist(),
            MenuAction::OpenArtistSelection => self.open_artist_selection(),
            MenuAction::MoreArtistSongs => self.more_artist_songs(false),
            MenuAction::PlayArtistSongs => self.more_artist_songs(true),
            MenuAction::ReloadArtist => self.reload_artist(),
            MenuAction::OpenPageSelection => self.open_page_row(),
            MenuAction::RemoveQueueSelection => self.remove_queue_selection(),
            MenuAction::MoveQueueSelectionUp => self.move_queue_selection(-1),
            MenuAction::MoveQueueSelectionDown => self.move_queue_selection(1),
            MenuAction::ClearUpcomingQueue => self.clear_upcoming_queue(),
            MenuAction::ShuffleUpcomingQueue => self.shuffle_upcoming_queue(),
            MenuAction::CycleRepeat => self.cycle_repeat(),
            MenuAction::FollowLyrics => {
                if let Some(now) = self.now.as_mut() {
                    now.open(Tab::Lyrics);
                }
            }
            MenuAction::TogglePause => self.toggle_pause(),
            MenuAction::ToggleMute => self.toggle_mute(),
            MenuAction::RetryTrack => self.retry_current_track(),
            MenuAction::Next => self.advance(1, false),
            MenuAction::Previous => self.advance(-1, false),
            MenuAction::Stop => self.stop(),
            MenuAction::ToggleCoverSize => self.toggle_cover_size(),
            MenuAction::LikePlaying => self.like_playing(),
            MenuAction::SharePlaying => self.share_playing(),
            MenuAction::SavePlaying => self.save_playing(),
            MenuAction::ChooseOutput => self.choose_output(),
        }
    }
}
