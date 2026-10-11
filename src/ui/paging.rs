//! Shelf paging shares its geometry with the visible buttons and click targets.
use super::*;

pub(super) struct Controls {
    pub heading: Rect,
    buttons: [(Rect, Option<usize>); 2],
}

impl Controls {
    pub fn new(
        area: Rect,
        count: usize,
        offset: usize,
        columns: usize,
        rows: usize,
    ) -> Option<Self> {
        if rows == 0 {
            return None;
        }
        let capacity = columns * rows;
        if count <= capacity || capacity == 0 || area.width < 24 || area.height == 0 {
            return None;
        }
        let maximum = count.div_ceil(rows).saturating_sub(columns) * rows;
        let first = (offset / rows * rows).min(maximum);
        Some(Self {
            heading: Rect::new(area.x, area.y, area.width - 9, 1),
            buttons: [
                (
                    Rect::new(area.right() - 7, area.y, 3, 1),
                    (first > 0).then_some(first.saturating_sub(capacity)),
                ),
                (
                    Rect::new(area.right() - 3, area.y, 3, 1),
                    (first < maximum).then_some(first.saturating_add(capacity).min(maximum)),
                ),
            ],
        })
    }

    pub fn render(&self, frame: &mut Frame) {
        for ((area, offset), label) in self.buttons.iter().zip([" < ", " > "]) {
            let style = if offset.is_some() {
                Style::default()
                    .fg(Color::White)
                    .bg(shell::SURFACE)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            frame.render_widget(Paragraph::new(Span::styled(label, style)), *area);
        }
    }

    pub fn register(&self, mouse: &mut MouseMap, action: impl Fn(usize) -> MouseAction) {
        for (area, offset) in self.buttons {
            if let Some(offset) = offset {
                mouse.targets.push(MouseTarget::Area(area, action(offset)));
            }
        }
    }
}
