/// The effect of an editing operation on a [`TextInput`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEffect {
    TextChanged,
    CursorMoved,
    /// The input was consumed without changing or moving anything. For instance, pressing
    /// backspace when there is no text.
    Ack,
}

/// A single-line text buffer with a cursor, and the framework-agnostic edit operations.
///
/// Frontends translate their own input events into these operations; the buffer and the
/// cursor stay here so the view model owns the edited text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextInput {
    // This is less space-efficient than String, but it's easier to work with text manipulation at the
    // cursor. Otherwise, you have to shuffle back and forth between String and String::chars().
    line: Vec<char>,
    cursor: usize,
}

impl TextInput {
    /// Inserts `c` at the cursor and moves the cursor past it.
    pub fn insert_char(&mut self, c: char) -> InputEffect {
        if self.cursor == self.line.len() {
            self.line.push(c);
        } else {
            self.line.insert(self.cursor, c);
        }
        self.cursor += 1;
        InputEffect::TextChanged
    }

    /// Removes the character before the cursor.
    pub fn backspace(&mut self) -> InputEffect {
        if self.line.is_empty() || self.cursor == 0 {
            return InputEffect::Ack;
        }
        // Perform the decrement first.
        self.cursor -= 1;
        self.line.remove(self.cursor);
        InputEffect::TextChanged
    }

    pub fn move_left(&mut self) -> InputEffect {
        if self.cursor == 0 {
            return InputEffect::Ack;
        }
        self.cursor -= 1;
        InputEffect::CursorMoved
    }

    pub fn move_right(&mut self) -> InputEffect {
        if self.cursor == self.line.len() {
            return InputEffect::Ack;
        }
        self.cursor += 1;
        InputEffect::CursorMoved
    }

    /// Splits the buffer into the text before the cursor, the character under the cursor
    /// (a space when the cursor sits past the end), and the text after it.
    #[must_use]
    pub fn split_at_cursor(&self) -> (String, String, String) {
        let before_cursor: String = self.line[0..self.cursor].iter().collect();
        if self.cursor == self.line.len() {
            return (before_cursor, " ".to_string(), String::new());
        }
        let after_cursor: String = self.line[self.cursor + 1..].iter().collect();
        (
            before_cursor,
            self.line[self.cursor].to_string(),
            after_cursor,
        )
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.line.is_empty()
    }

    #[must_use]
    pub fn text(&self) -> String {
        self.line.iter().collect()
    }
}
