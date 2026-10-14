use super::{
    config, Block, BorderType, Borders, Cursor, Frame, List, ListItem, ListState, Rect, Span,
    Style, Table, TableState,
};

/// Construct and render a block.
///
/// This function should only be used to render a window's borders and its title.
/// It returns the rectangle to render the inner widgets inside the block.
pub fn construct_and_render_block(
    title: &str,
    theme: &crate::theme::Theme,
    borders: Borders,
    frame: &mut Frame,
    rect: Rect,
) -> Rect {
    let mut title = title.to_string();

    let configs = config::get_config();

    let (borders, border_type) = match configs.app_config.border_type {
        config::BorderType::Hidden | config::BorderType::Plain => (borders, BorderType::Plain),
        config::BorderType::Rounded => (borders, BorderType::Rounded),
        config::BorderType::Double => (borders, BorderType::Double),
        config::BorderType::Thick => (borders, BorderType::Thick),
    };

    let mut block = Block::default()
        .borders(borders)
        .border_style(theme.border())
        .border_type(border_type);

    let inner_rect = block.inner(rect);

    // Handle `BorderType::Hidden` after determining the inner rectangle
    // `Hidden` border can be done by setting the borders to be `NONE`.
    // NOTE: we want to handle the border after the inner rectangle computation,
    // so that paddings between windows are properly determined.
    if configs.app_config.border_type == config::BorderType::Hidden {
        block = block.borders(Borders::NONE);
        // add padding to the title to ensure the inner text is aligned with the title
        title = format!(" {title}");
    }

    // Set `title` for the block
    block = block.title(Span::styled(title, theme.block_title()));

    frame.render_widget(block, rect);
    inner_rect
}

/// Construct a generic list widget
pub fn construct_list_widget<'a>(
    theme: &crate::theme::Theme,
    items: Vec<(String, bool)>,
    is_active: bool,
    selected_index: Option<usize>,
) -> (List<'a>, usize) {
    let configs = config::get_config();
    let n_items = items.len();

    (
        List::new(
            items
                .into_iter()
                .enumerate()
                .map(|(i, (s, is_playing))| {
                    let text = if is_active && configs.app_config.enable_relative_line_number {
                        if let Some(selected_index) = selected_index {
                            let diff = (i as isize - selected_index as isize).abs();
                            let width = std::cmp::min(n_items.to_string().len(), 2);
                            format!("{diff:>width$}  {s}")
                        } else {
                            s
                        }
                    } else {
                        s
                    };
                    ListItem::new(text).style(if is_playing {
                        theme.current_playing()
                    } else {
                        Style::default()
                    })
                })
                .collect::<Vec<_>>(),
        )
        .highlight_style(theme.selection(is_active)),
        n_items,
    )
}

/// Adjust the `selected` position of a cursor if that position is invalid
fn adjust_cursor(cursor: &mut Cursor, len: usize) {
    if let Some(p) = cursor.selected() {
        if p >= len {
            cursor.select(if len > 0 { Some(len - 1) } else { Some(0) });
        }
    } else if len > 0 {
        cursor.select(Some(0));
    }
}

pub fn render_list_window(
    frame: &mut Frame,
    widget: List,
    rect: Rect,
    len: usize,
    cursor: &mut Cursor,
) {
    adjust_cursor(cursor, len);
    // ratatui keeps the scroll offset inside its own widget state; bridge the cursor in and
    // back out so the view model stays free of rendering types.
    let mut state = ListState::default();
    state.select(cursor.selected());
    *state.offset_mut() = cursor.offset;
    frame.render_stateful_widget(widget, rect, &mut state);
    cursor.selected = state.selected();
    cursor.offset = state.offset();
}

pub fn render_table_window(
    frame: &mut Frame,
    widget: Table,
    rect: Rect,
    len: usize,
    cursor: &mut Cursor,
) {
    adjust_cursor(cursor, len);
    let mut state = TableState::default();
    state.select(cursor.selected());
    *state.offset_mut() = cursor.offset;
    frame.render_stateful_widget(widget, rect, &mut state);
    cursor.selected = state.selected();
    cursor.offset = state.offset();
}
