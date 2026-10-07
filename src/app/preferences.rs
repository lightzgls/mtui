//! Settings state and semantic choices, independent of terminal geometry.

use crossterm::event::{KeyCode, KeyEvent};

use super::{App, MenuPage, Overlay, moved_cursor};
use crate::config::{CoverStyle, IconTheme, ImageRenderer};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    Output,
    Cover,
    Renderer,
    Icon,
    Discord,
    Tray,
}

impl Setting {
    pub const ALL: [Self; 6] = [
        Self::Output,
        Self::Cover,
        Self::Renderer,
        Self::Icon,
        Self::Discord,
        Self::Tray,
    ];

    pub fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|setting| *setting == self)
            .unwrap_or(0)
    }

    pub fn section(self) -> &'static str {
        match self {
            Self::Output => "Playback",
            Self::Cover | Self::Renderer | Self::Icon => "Appearance",
            Self::Discord | Self::Tray => "Integrations",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Output => "Audio output",
            Self::Cover => "Cover style",
            Self::Renderer => "Artwork display",
            Self::Icon => "Tray icon",
            Self::Discord => "Discord presence",
            Self::Tray => "Keep tray icon visible",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Output => "System default follows the output selected in Windows.",
            Self::Cover => "Choose detailed artwork or a cover drawn with text.",
            Self::Renderer => "Automatic detects support; Kitty needs a compatible terminal.",
            Self::Icon => "The icon shown in the Windows notification area.",
            Self::Discord => "Share your current song and playback state on Discord.",
            Self::Tray => "Keep tray controls available while the terminal is open.",
        }
    }

    pub fn is_toggle(self) -> bool {
        matches!(self, Self::Discord | Self::Tray)
    }

    pub fn enabled(self) -> bool {
        cfg!(windows) || !matches!(self, Self::Tray | Self::Icon)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChoiceValue {
    Output(Option<String>),
    Cover(CoverStyle),
    Renderer(ImageRenderer),
    Icon(IconTheme),
}

#[derive(Debug, Clone)]
pub struct Choice {
    pub label: String,
    pub value: ChoiceValue,
    pub current: bool,
}

pub struct SettingRow {
    pub setting: Setting,
    pub value: String,
    pub enabled: bool,
}

#[derive(Debug, Clone)]
pub struct Picker {
    pub setting: Setting,
    pub choices: Vec<Choice>,
    pub selected: usize,
}

#[derive(Default)]
pub struct State {
    pub selected: usize,
    pub picker: Option<Picker>,
    pub return_to_menu: bool,
    pub notice: Option<(String, bool)>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Intent {
    Activate(Setting),
    Step(Setting, bool),
    Apply(ChoiceValue),
    Close,
}

impl State {
    pub fn selected_setting(&self) -> Setting {
        Setting::ALL[self.selected.min(Setting::ALL.len() - 1)]
    }

    pub fn open(&mut self, return_to_menu: bool) {
        self.picker = None;
        self.notice = None;
        self.return_to_menu = return_to_menu;
    }

    pub fn key(&mut self, key: KeyEvent) -> Option<Intent> {
        if let Some(picker) = self.picker.as_mut() {
            match key.code {
                KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                    picker.selected = moved_cursor(picker.selected, 1, picker.choices.len())
                }
                KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => {
                    picker.selected = moved_cursor(picker.selected, -1, picker.choices.len())
                }
                KeyCode::Home | KeyCode::Char('g') => picker.selected = 0,
                KeyCode::End | KeyCode::Char('G') => {
                    picker.selected = picker.choices.len().saturating_sub(1)
                }
                KeyCode::Esc | KeyCode::Char('q') => self.picker = None,
                KeyCode::Enter | KeyCode::Char(' ') => {
                    let intent = picker
                        .choices
                        .get(picker.selected)
                        .map(|choice| Intent::Apply(choice.value.clone()));
                    self.picker = None;
                    return intent;
                }
                _ => {}
            }
            return None;
        }
        let previous = self.selected;
        match key.code {
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                self.selected = moved_cursor(self.selected, 1, Setting::ALL.len())
            }
            KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => {
                self.selected = moved_cursor(self.selected, -1, Setting::ALL.len())
            }
            KeyCode::Home | KeyCode::Char('g') => self.selected = 0,
            KeyCode::End | KeyCode::Char('G') => self.selected = Setting::ALL.len() - 1,
            KeyCode::Enter | KeyCode::Char(' ') => {
                return Some(Intent::Activate(self.selected_setting()));
            }
            KeyCode::Left | KeyCode::Char('h') => {
                return Some(Intent::Step(self.selected_setting(), false));
            }
            KeyCode::Right | KeyCode::Char('l') => {
                return Some(Intent::Step(self.selected_setting(), true));
            }
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('S') => return Some(Intent::Close),
            _ => {}
        }
        if previous != self.selected {
            self.notice = None;
        }
        None
    }

    pub fn choose(&mut self, index: usize) -> Option<Intent> {
        let choice = self.picker.as_ref()?.choices.get(index)?.value.clone();
        self.picker = None;
        Some(Intent::Apply(choice))
    }
}

impl App {
    pub fn preferences(&self) -> &State {
        &self.preferences
    }

    pub fn preference_rows(&self) -> Vec<SettingRow> {
        Setting::ALL
            .into_iter()
            .map(|setting| SettingRow {
                setting,
                enabled: setting.enabled(),
                value: match setting {
                    Setting::Output => self.output_device_label().to_string(),
                    Setting::Cover => cover_label(self.cover_style).to_string(),
                    Setting::Renderer => renderer_label(self.image_renderer).to_string(),
                    Setting::Icon => self.icon_theme.label().to_string(),
                    Setting::Discord => {
                        if self.presence_enabled() { "On" } else { "Off" }.to_string()
                    }
                    Setting::Tray => if self.start_in_tray { "On" } else { "Off" }.to_string(),
                },
            })
            .collect()
    }

    fn setting_choices(&self, setting: Setting) -> Vec<Choice> {
        let values: Vec<(String, ChoiceValue)> = match setting {
            Setting::Output => {
                std::iter::once(("System default".to_string(), ChoiceValue::Output(None)))
                    .chain(self.output_devices.iter().map(|device| {
                        (
                            device.name.clone(),
                            ChoiceValue::Output(Some(device.id.clone())),
                        )
                    }))
                    .collect()
            }
            Setting::Cover => [CoverStyle::Pixel, CoverStyle::ColoredAscii]
                .into_iter()
                .map(|value| (cover_label(value).to_string(), ChoiceValue::Cover(value)))
                .collect(),
            Setting::Renderer => [
                ImageRenderer::Automatic,
                ImageRenderer::PixelArt,
                ImageRenderer::Kitty,
            ]
            .into_iter()
            .map(|value| {
                (
                    renderer_label(value).to_string(),
                    ChoiceValue::Renderer(value),
                )
            })
            .collect(),
            Setting::Icon => [
                IconTheme::Signal,
                IconTheme::Wave,
                IconTheme::Orbit,
                IconTheme::Mono,
            ]
            .into_iter()
            .map(|value| (value.label().to_string(), ChoiceValue::Icon(value)))
            .collect(),
            Setting::Discord | Setting::Tray => Vec::new(),
        };
        values
            .into_iter()
            .map(|(label, value)| {
                let current = match &value {
                    ChoiceValue::Output(output) => *output == self.output_device,
                    ChoiceValue::Cover(style) => *style == self.cover_style,
                    ChoiceValue::Renderer(renderer) => *renderer == self.image_renderer,
                    ChoiceValue::Icon(icon) => *icon == self.icon_theme,
                };
                Choice {
                    label,
                    value,
                    current,
                }
            })
            .collect()
    }

    pub(super) fn handle_preferences_intent(&mut self, intent: Intent) {
        match intent {
            Intent::Close => {
                self.overlay = Overlay::None;
                if self.preferences.return_to_menu {
                    self.open_menu(MenuPage::Root);
                    if let Some(menu) = self.menu.as_mut() {
                        menu.selected = menu
                            .items
                            .iter()
                            .position(|item| item.action == Some(super::MenuAction::OpenSettings))
                            .unwrap_or(0);
                    }
                }
            }
            Intent::Activate(setting) if setting.enabled() => {
                self.preferences.selected = setting.index();
                self.preferences.notice = None;
                if setting.is_toggle() {
                    self.change_setting(setting, true);
                } else {
                    if setting == Setting::Output {
                        self.refresh_output_devices();
                    }
                    let choices = self.setting_choices(setting);
                    let selected = choices
                        .iter()
                        .position(|choice| choice.current)
                        .unwrap_or(0);
                    self.preferences.picker = Some(Picker {
                        setting,
                        choices,
                        selected,
                    });
                }
            }
            Intent::Step(setting, forward) if setting.enabled() => {
                self.change_setting(setting, forward)
            }
            Intent::Apply(choice) => {
                match choice {
                    ChoiceValue::Output(output) => self.set_audio_output(output),
                    ChoiceValue::Cover(style) => self.set_cover_style(style),
                    ChoiceValue::Renderer(renderer) => self.set_image_renderer(renderer),
                    ChoiceValue::Icon(icon) => self.set_icon_theme(icon),
                }
                self.preferences.notice =
                    Some((self.status.clone(), self.status.starts_with("could not")));
            }
            _ => {}
        }
    }

    fn change_setting(&mut self, setting: Setting, forward: bool) {
        match setting {
            Setting::Output => self.cycle_audio_output(forward),
            Setting::Cover => self.cycle_cover_style(forward),
            Setting::Renderer => self.cycle_image_renderer(forward),
            Setting::Icon => self.cycle_icon_theme(forward),
            Setting::Discord => self.toggle_presence(),
            Setting::Tray => self.toggle_start_in_tray(),
        }
        self.preferences.notice = Some((self.status.clone(), self.status.starts_with("could not")));
    }
}

pub fn cover_label(style: CoverStyle) -> &'static str {
    match style {
        CoverStyle::Pixel => "Artwork",
        CoverStyle::ColoredAscii => "Colored text",
    }
}

pub fn renderer_label(renderer: ImageRenderer) -> &'static str {
    match renderer {
        ImageRenderer::Automatic => "Automatic",
        ImageRenderer::Kitty => "Kitty images",
        ImageRenderer::PixelArt => "Terminal cells",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn browsing_a_picker_does_not_apply_and_escape_keeps_the_setting() {
        let mut state = State {
            selected: Setting::Cover.index(),
            ..Default::default()
        };
        state.picker = Some(Picker {
            setting: Setting::Cover,
            selected: 0,
            choices: vec![
                Choice {
                    label: "Artwork".into(),
                    value: ChoiceValue::Cover(CoverStyle::Pixel),
                    current: true,
                },
                Choice {
                    label: "Colored text".into(),
                    value: ChoiceValue::Cover(CoverStyle::ColoredAscii),
                    current: false,
                },
            ],
        });
        assert_eq!(state.key(key(KeyCode::Down)), None);
        assert_eq!(state.picker.as_ref().unwrap().selected, 1);
        assert_eq!(state.key(key(KeyCode::Esc)), None);
        assert!(state.picker.is_none());
        assert_eq!(state.selected_setting(), Setting::Cover);
        assert_eq!(state.key(key(KeyCode::Esc)), Some(Intent::Close));
    }

    #[test]
    fn confirming_a_choice_applies_only_that_value_and_invalid_clicks_do_nothing() {
        let mut state = State {
            picker: Some(Picker {
                setting: Setting::Output,
                selected: 0,
                choices: vec![Choice {
                    label: "Headphones".into(),
                    value: ChoiceValue::Output(Some("headphones".into())),
                    current: false,
                }],
            }),
            ..Default::default()
        };
        assert_eq!(state.choose(9), None);
        assert!(state.picker.is_some());
        assert_eq!(
            state.key(key(KeyCode::Enter)),
            Some(Intent::Apply(ChoiceValue::Output(Some(
                "headphones".into()
            ))))
        );
        assert!(state.picker.is_none());
    }
}
