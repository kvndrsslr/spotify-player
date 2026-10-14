//! The ratatui TUI frontend: the application's original renderer.

use anyhow::{Context, Result};
use parking_lot::Mutex;
use std::sync::Arc;

use crate::{
    event,
    frontend::{Frontend, FrontendContext},
    ui,
};

/// The TUI, on the terminal's alternate screen.
pub static FRONTEND: Frontend = Frontend {
    name: "tui",
    probe: || Ok(true),
    run,
};

/// Starts the terminal handling and the TUI's two threads, and returns once they are running.
///
/// The threads live until the process exits: `ui::run` exits the process on quit.
fn run(ctx: &FrontendContext) -> Result<()> {
    // the TUI owns its own state: the theme and the terminal size are resolved here,
    // not while constructing the shared application state.
    let ui_state: ui::SharedUiState = Arc::new(Mutex::new(ui::UIState::new()));

    // The image picker must run before the terminal is set up: it queries the terminal on
    // stdin and has to clean up after a query that is never answered before `init_terminal`
    // reads a cursor position. See `ui::init_image_picker`.
    #[cfg(feature = "image")]
    ui::init_image_picker(&ui_state).context("initialize image picker")?;
    let terminal = ui::init_terminal().context("initialize terminal")?;

    // terminal event handler task
    std::thread::Builder::new()
        .name("terminal-event-handler".to_string())
        .spawn({
            let client_tx = ctx.client_tx.clone();
            let state = ctx.state.clone();
            let ui_state = ui_state.clone();
            move || {
                event::start_event_handler(&state, &ui_state, &client_tx);
            }
        })?;

    // application UI task
    std::thread::Builder::new().name("ui".to_string()).spawn({
        let state = ctx.state.clone();
        move || ui::run(&state, &ui_state, terminal)
    })?;

    Ok(())
}
