use crate::utils::filtered_items_from_query;

mod cursor;
mod page;
mod popup;
mod text_input;

pub use cursor::*;
pub use page::*;
pub use popup::*;
pub use text_input::*;

/// Guard over the application's [`ViewState`].
pub type ViewStateGuard<'a> = parking_lot::MutexGuard<'a, ViewState>;

/// The application's view state: the framework-agnostic page/popup model and its cursors.
///
/// Frontends own their rendering state; everything the command executor needs in order to
/// decide what a command does to the current view lives here.
#[derive(Debug)]
pub struct ViewState {
    pub is_running: bool,

    pub history: Vec<PageState>,
    pub popup: Option<PopupState>,
}

impl ViewState {
    /// # Panics
    ///
    /// Panics if the page history is empty, which cannot happen: [`ViewState::default`]
    /// always pushes one page.
    #[must_use]
    pub fn current_page(&self) -> &PageState {
        self.history.last().expect("non-empty history")
    }

    /// # Panics
    ///
    /// Panics if the page history is empty (see [`ViewState::current_page`]).
    pub fn current_page_mut(&mut self) -> &mut PageState {
        self.history.last_mut().expect("non-empty history")
    }

    pub fn new_search_popup(&mut self) {
        self.current_page_mut().select(0);
        self.popup = Some(PopupState::Search {
            query: String::new(),
        });
    }

    pub fn new_page(&mut self, page: PageState) {
        self.popup = None;
        if let Some(current_page) = self.history.last() {
            if &page == current_page {
                return;
            }
        }
        self.history.push(page);
    }

    /// Return whether there exists a focused popup.
    ///
    /// Currently, only search popup is not focused when it's opened.
    #[must_use]
    pub fn has_focused_popup(&self) -> bool {
        match self.popup.as_ref() {
            None => false,
            Some(popup) => !matches!(popup, PopupState::Search { .. }),
        }
    }

    /// Get a list of items possibly filtered by a search query if exists a search popup
    pub fn search_filtered_items<'a, T: std::fmt::Display>(&self, items: &'a [T]) -> Vec<&'a T> {
        match self.popup {
            Some(PopupState::Search { ref query }) => filtered_items_from_query(query, items),
            _ => items.iter().collect::<Vec<_>>(),
        }
    }
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            is_running: true,
            history: vec![PageState::Library {
                state: LibraryPageUIState::new(),
            }],
            popup: None,
        }
    }
}
