use std::ops::{Deref, DerefMut};

use crate::{
    state::{ViewState, ViewStateGuard},
    ui::state::UIState,
};

/// The TUI's mutable context for a single event or render pass.
///
/// Derefs to the core [`ViewState`]; the TUI-only [`UIState`] is reachable through
/// [`Ctx::tui`]. Bundling both keeps the two locks under a single guard-like value so
/// frontend code cannot hold one without the other.
pub struct Ctx<'a> {
    view: ViewStateGuard<'a>,
    pub tui: &'a mut UIState,
}

impl<'a> Ctx<'a> {
    pub fn new(view: ViewStateGuard<'a>, tui: &'a mut UIState) -> Self {
        Self { view, tui }
    }

    /// The core view model, for the command executor.
    pub fn view_mut(&mut self) -> &mut ViewStateGuard<'a> {
        &mut self.view
    }
}

impl Deref for Ctx<'_> {
    type Target = ViewState;

    fn deref(&self) -> &ViewState {
        &self.view
    }
}

impl DerefMut for Ctx<'_> {
    fn deref_mut(&mut self) -> &mut ViewState {
        &mut self.view
    }
}
