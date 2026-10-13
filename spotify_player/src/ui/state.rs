use std::sync::Arc;

use crate::{
    config, key,
    ui::{self, Orientation},
};

#[cfg(feature = "image")]
use crate::ui::image_compositor::ImageCompositor;
#[cfg(feature = "image")]
use ratatui_image::picker::Picker;

/// The TUI state shared by the event and the render threads.
pub type SharedUiState = Arc<parking_lot::Mutex<UIState>>;

/// State owned by the ratatui frontend: rendering, input bookkeeping and terminal layout.
///
/// Everything that describes *what* is on screen (pages, popups, cursors, the quit flag)
/// lives in the framework-agnostic [`crate::state::ViewState`] instead.
#[derive(Debug)]
pub struct UIState {
    pub theme: crate::theme::Theme,
    pub input_key_sequence: key::KeySequence,
    pub orientation: ui::Orientation,

    /// The playback progress bar's area, used to handle mouse click events (for seeking).
    pub playback_progress_bar_rect: ProgressBarLayout,

    /// Count prefix for vim-style navigation (e.g., 5j, 10k)
    pub count_prefix: Option<usize>,

    #[cfg(feature = "image")]
    pub image_compositor: ImageCompositor,

    #[cfg(feature = "image")]
    pub picker: Picker,
}

/// The rectangle representing the playback progress bar.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProgressBarLayout {
    pub row: u16,
    pub x: u16,
    pub width: u16,
}

impl ProgressBarLayout {
    /// Whether a terminal cell at `(row, column)` falls inside the progress bar.
    pub fn contains(self, row: u16, column: u16) -> bool {
        row == self.row && column >= self.x && column < self.x + self.width
    }
}

impl UIState {
    /// Build the TUI state, resolving the configured theme and querying the terminal size.
    pub fn new() -> Self {
        let theme = crate::frontend_config::get()
            .theme_config
            .find_theme(&config::get_config().app_config.theme)
            .unwrap_or_default();

        Self {
            theme,
            input_key_sequence: key::KeySequence { keys: vec![] },
            orientation: match crossterm::terminal::size() {
                Ok((columns, rows)) => Orientation::from_size(columns, rows),
                Err(err) => {
                    tracing::warn!("Unable to get terminal size, error: {err:#}");
                    Orientation::default()
                }
            },
            playback_progress_bar_rect: ProgressBarLayout::default(),
            count_prefix: None,
            #[cfg(feature = "image")]
            image_compositor: ImageCompositor::default(),
            // Will be reinitialized later in `ui::init_image_picker`
            #[cfg(feature = "image")]
            picker: Picker::halfblocks(),
        }
    }
}

impl Default for UIState {
    fn default() -> Self {
        Self::new()
    }
}
