//! Byte cursors always sit between complete user-perceived characters.
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;

pub(in crate::app) fn boundary(text: &str, cursor: usize) -> usize {
    text.grapheme_indices(true).map(|(at, _)| at).chain(std::iter::once(text.len()))
        .take_while(|at| *at <= cursor).last().unwrap_or(0)
}

/// `Some` owns the key; its value says whether the text itself changed.
pub(in crate::app) fn edit(text: &mut String, cursor: &mut usize, key: KeyEvent) -> Option<bool> {
    *cursor = boundary(text, *cursor);
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let previous = || text[..*cursor].grapheme_indices(true).next_back().map_or(0, |(at, _)| at);
    let next = || text[*cursor..].graphemes(true).next().map_or(*cursor, |g| *cursor + g.len());
    let word_start = || {
        let before = &text[..*cursor];
        let trimmed = before.trim_end();
        trimmed.split_word_bound_indices().rev().find(|(_, part)| part.chars().any(char::is_alphanumeric))
            .map_or(0, |(at, _)| at)
    };
    match key.code {
        KeyCode::Left => { *cursor = if ctrl { word_start() } else { previous() }; Some(false) }
        KeyCode::Right => {
            *cursor = if ctrl {
                text[*cursor..].split_word_bound_indices().find(|(_, part)| part.chars().any(char::is_alphanumeric))
                    .map_or(text.len(), |(at, word)| *cursor + at + word.len())
            } else { next() }; Some(false)
        }
        KeyCode::Home | KeyCode::Char('a') if ctrl || key.code == KeyCode::Home => { *cursor = 0; Some(false) }
        KeyCode::End | KeyCode::Char('e') if ctrl || key.code == KeyCode::End => { *cursor = text.len(); Some(false) }
        KeyCode::Backspace | KeyCode::Char('w') if key.code == KeyCode::Backspace || ctrl => {
            let start = if ctrl { word_start() } else { previous() };
            let changed = start != *cursor;
            text.replace_range(start..*cursor, ""); *cursor = start; Some(changed)
        }
        KeyCode::Delete => {
            let end = next(); let changed = end != *cursor;
            text.replace_range(*cursor..end, ""); Some(changed)
        }
        KeyCode::Char('u') if ctrl => { let changed = !text.is_empty(); text.clear(); *cursor = 0; Some(changed) }
        KeyCode::Char(c) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) && !c.is_control() => {
            text.insert(*cursor, c); *cursor += c.len_utf8();
            // Inserting before a combining mark may join an existing grapheme.
            *cursor = text.grapheme_indices(true).map(|(at, _)| at).chain(std::iter::once(text.len()))
                .find(|at| *at >= *cursor).unwrap_or(text.len());
            Some(true)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn editing_never_splits_combining_text_or_emoji() {
        let mut query = "a e\u{301} 👩‍💻 日本語".to_owned();
        let mut cursor = query.len();
        for _ in 0..3 { edit(&mut query, &mut cursor, KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)); }
        edit(&mut query, &mut cursor, KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        edit(&mut query, &mut cursor, KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(query, "a e\u{301} 日本語");
        cursor = 2;
        edit(&mut query, &mut cursor, KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE));
        edit(&mut query, &mut cursor, KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE));
        assert_eq!(query, "a 日本語");
        edit(&mut query, &mut cursor, KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE));
        assert_eq!(query, "a z日本語");
    }
    #[test]
    fn word_deletion_and_clear_ignore_unrelated_control_characters() {
        let mut query = "hello world".to_owned(); let mut cursor = query.len();
        edit(&mut query, &mut cursor, KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(query, "hello ");
        assert_eq!(edit(&mut query, &mut cursor, KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL)), None);
        edit(&mut query, &mut cursor, KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!((query.as_str(), cursor), ("", 0));
    }
}
