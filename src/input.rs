use crossterm::event::KeyCode;
use unicode_width::UnicodeWidthStr;

/// A single-line text field with a cursor.
///
/// The search box and the playlist-name prompt need the same handful of
/// editing operations, and both have to count in *characters* rather than
/// bytes — a query or a playlist name with an accent in it otherwise panics
/// on the first insert past that character.
#[derive(Default)]
pub struct TextInput {
    pub value: String,
    /// Cursor position in characters, in `0..=value.chars().count()`.
    pub cursor: usize,
}

impl TextInput {
    /// A field pre-filled with `value`, cursor at the end — what renaming
    /// wants, so the existing name can be edited rather than retyped.
    pub fn with_value(value: impl Into<String>) -> Self {
        let value = value.into();
        let cursor = value.chars().count();
        Self { value, cursor }
    }

    pub fn is_empty(&self) -> bool {
        self.value.trim().is_empty()
    }

    pub fn len(&self) -> usize {
        self.value.chars().count()
    }

    /// Byte offset of character `idx`, or the end of the string.
    fn byte_pos(&self, idx: usize) -> usize {
        self.value
            .char_indices()
            .nth(idx)
            .map(|(i, _)| i)
            .unwrap_or(self.value.len())
    }

    /// Display width of the text before the cursor — where the terminal
    /// cursor has to be drawn, which is not the character count once
    /// double-width glyphs are involved.
    pub fn cursor_width(&self) -> u16 {
        let before: String = self.value.chars().take(self.cursor).collect();
        UnicodeWidthStr::width(before.as_str()) as u16
    }

    /// Apply an editing key. Returns false for keys this field has no meaning
    /// for (Enter, Esc, …), leaving them to the caller.
    pub fn handle_key(&mut self, key: KeyCode) -> bool {
        match key {
            KeyCode::Char(c) => {
                let at = self.byte_pos(self.cursor);
                self.value.insert(at, c);
                self.cursor += 1;
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    let at = self.byte_pos(self.cursor);
                    self.value.remove(at);
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.len() {
                    let at = self.byte_pos(self.cursor);
                    self.value.remove(at);
                }
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.len()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.len(),
            _ => return false,
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Editing has to be character-indexed: the byte offsets of "é" and the
    /// character after it differ, and using one for the other panics.
    #[test]
    fn editing_is_character_indexed() {
        let mut input = TextInput::default();
        for c in "café".chars() {
            input.handle_key(KeyCode::Char(c));
        }
        input.handle_key(KeyCode::Left);
        input.handle_key(KeyCode::Char('x'));
        assert_eq!(input.value, "cafxé");

        input.handle_key(KeyCode::Home);
        input.handle_key(KeyCode::Delete);
        assert_eq!(input.value, "afxé");

        input.handle_key(KeyCode::End);
        input.handle_key(KeyCode::Backspace);
        assert_eq!(input.value, "afx");
    }

    #[test]
    fn cursor_stays_within_bounds() {
        let mut input = TextInput::with_value("ab");
        assert_eq!(input.cursor, 2);
        input.handle_key(KeyCode::Right);
        assert_eq!(input.cursor, 2);
        input.handle_key(KeyCode::Home);
        input.handle_key(KeyCode::Left);
        assert_eq!(input.cursor, 0);
    }
}
