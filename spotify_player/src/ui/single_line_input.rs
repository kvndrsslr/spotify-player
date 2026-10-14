use ratatui::widgets::Widget;

use super::{Line, Modifier, Paragraph, Span, Style};
use crate::{
    key::Key,
    state::{InputEffect, TextInput},
};

/// Translates a key press into an edit operation on `input`.
///
/// Returns `None` when the key has no editing meaning, so the caller can handle it instead.
pub fn handle_key(input: &mut TextInput, key: &Key) -> Option<InputEffect> {
    input.handle_key(key)
}

/// Renders a text input, highlighting the character under the cursor when it is active.
pub fn render(input: &TextInput, is_active: bool) -> impl Widget {
    if !is_active {
        return Paragraph::new(input.text());
    }

    let (before_cursor, cursor, after_cursor) = input.split_at_cursor();
    let text_style = Style::default();
    let cursor_style = Style::default().add_modifier(Modifier::REVERSED);
    let formatted_line = Line::from(vec![
        Span::styled(before_cursor, text_style),
        Span::styled(cursor, cursor_style),
        Span::styled(after_cursor, text_style),
    ]);

    Paragraph::new(formatted_line)
}
