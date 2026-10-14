/// A list/table cursor: the selected row plus the scroll offset.
///
/// Mirrors the stateful part of a widget state so a frontend can bridge it to its own
/// widget state without the view model depending on a rendering framework.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cursor {
    pub selected: Option<usize>,
    pub offset: usize,
}

impl Cursor {
    /// Selects an item.
    pub fn select(&mut self, selected: Option<usize>) {
        self.selected = selected;
    }

    /// The selected item.
    #[must_use]
    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    /// A mutable reference to the scroll offset.
    pub fn offset_mut(&mut self) -> &mut usize {
        &mut self.offset
    }
}
