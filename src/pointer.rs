//! Song clicks select first; a completed second click plays. Drags keep ownership.
use std::time::{Duration, Instant};
use anyhow::Result;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use crate::{app::{App, MouseAction, SongClick}, ui::MouseMap};

const DOUBLE_CLICK_TIME: Duration = Duration::from_millis(500);

#[derive(Default)]
pub struct Pointer {
    capture: Option<Capture>,
    press: Option<SongPress>,
    last_click: Option<SongPress>,
}

struct SongPress {
    target: SongClick,
    position: (u16, u16),
    at: Instant,
    double: bool,
}

fn near(a: (u16, u16), b: (u16, u16)) -> bool {
    a.0.abs_diff(b.0) <= 1 && a.1.abs_diff(b.1) <= 1
}

enum Capture {
    Queue { id: String, epoch: u64 },
    Seek,
    Volume,
}

impl Pointer {
    pub fn cancel(&mut self) {
        self.capture = None;
        self.press = None;
        self.last_click = None;
    }

    pub fn handle(&mut self, app: &mut App, mouse: &MouseMap, event: MouseEvent) -> Result<()> {
        self.handle_at(app, mouse, event, Instant::now())
    }

    pub(crate) fn handle_at(&mut self, app: &mut App, mouse: &MouseMap, event: MouseEvent, at: Instant) -> Result<()> {
        let (x, y) = (event.column, event.row);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.capture = None;
                self.press = None;
                if let Some(action) = mouse.action_at(x, y) {
                    let target = app.song_click(action);
                    let selected = target.as_ref().map_or(action, |target| target.select);
                    if let Some(target) = target {
                        let double = self.last_click.take().is_some_and(|first|
                            at.saturating_duration_since(first.at) <= DOUBLE_CLICK_TIME
                                && near(first.position, (x, y)) && first.target == target);
                        self.press = Some(SongPress { target, position: (x, y), at, double });
                    } else { self.last_click = None; }
                    self.capture = match selected {
                        MouseAction::SelectPageRow(index) => app.queue_drag_identity(index)
                            .map(|(id, epoch)| Capture::Queue { id, epoch }),
                        MouseAction::SeekTo(_) => Some(Capture::Seek),
                        MouseAction::SetVolume(_) => Some(Capture::Volume),
                        _ => None,
                    };
                    app.handle_mouse_action(selected)?;
                } else { self.last_click = None; }
            }
            MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left) => {
                if matches!(event.kind, MouseEventKind::Drag(_))
                    && self.press.as_ref().is_some_and(|press| press.position != (x, y))
                {
                    self.press = None;
                    self.last_click = None;
                }
                match &self.capture {
                    Some(Capture::Queue { id, epoch }) => {
                        if let Some(target) = mouse.queue_row_at(x, y) { app.drag_queue_song(id, *epoch, target); }
                    }
                    Some(Capture::Seek) => {
                        if matches!(event.kind, MouseEventKind::Drag(_))
                            && let Some(action @ MouseAction::SeekTo(_)) = mouse.drag_action_at(x, y) { app.handle_mouse_action(action)?; }
                    }
                    Some(Capture::Volume) => {
                        if matches!(event.kind, MouseEventKind::Drag(_))
                            && let Some(action @ MouseAction::SetVolume(_)) = mouse.drag_action_at(x, y) { app.handle_mouse_action(action)?; }
                    }
                    None => {}
                }
                if matches!(event.kind, MouseEventKind::Up(_)) {
                    self.capture = None;
                    self.last_click = None;
                    if let Some(press) = self.press.take()
                        && near(press.position, (x, y))
                        && at.saturating_duration_since(press.at) <= DOUBLE_CLICK_TIME
                        && mouse.action_at(x, y).and_then(|action| app.song_click(action)).as_ref() == Some(&press.target)
                    {
                        if press.double { app.handle_mouse_action(press.target.activate)?; }
                        else { self.last_click = Some(press); }
                    }
                }
            }
            MouseEventKind::Down(MouseButton::Right) => {
                self.cancel();
                if let Some(action) = mouse.context_action_at(x, y) { app.handle_mouse_action(action)?; }
                app.handle_mouse_action(MouseAction::OpenPageActions)?;
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                self.press = None;
                self.last_click = None;
                let up = event.kind == MouseEventKind::ScrollUp;
                if app.precision_volume && mouse.over_cover(x, y) {
                    app.handle_mouse_action(MouseAction::AdjustPrecisionVolume(up))?;
                } else if mouse.over_volume(x, y) {
                    app.handle_mouse_action(MouseAction::AdjustVolume(up))?;
                } else { app.handle_mouse_scroll(!up)?; }
            }
            MouseEventKind::Down(_) | MouseEventKind::Up(_) => self.cancel(),
            _ => {}
        }
        Ok(())
    }
}
