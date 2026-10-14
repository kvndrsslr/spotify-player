//! The key → command → handler pipeline, shared by every frontend.
//!
//! The TUI feeds it keys decoded from crossterm, the TSP frontend keys decoded from the terminal's
//! pty; both then take the same path: the current page or popup handles the command, and the global
//! handler gets what neither took.

use super::{
    config, controller, page, popup, Command, CommandOrAction, Ctx, Cursor, Key, KeyCode,
    KeySequence, PopupState, Result, SharedState,
};
use spotify_player_core::client::ClientRequest;

/// Runs one key through the whole pipeline: key sequence bookkeeping, the page/popup router and the
/// global fallback.
pub fn handle_key(
    key: Key,
    client_pub: &flume::Sender<ClientRequest>,
    state: &SharedState,
    ui: &mut Ctx<'_>,
) -> Result<()> {
    let mut key_sequence = ui.tui.input_key_sequence.clone();
    key_sequence.keys.push(key);

    // check if the current key sequence matches any keymap's prefix
    // if not, reset the key sequence
    let keymap_config = &config::get_config().keymap_config;
    if !keymap_config.has_matched_prefix(&key_sequence) {
        key_sequence = KeySequence { keys: vec![key] };
    }

    tracing::debug!(
        "Handling key {key:?}, current key sequence: {key_sequence:?}, count prefix: {:?}",
        ui.tui.count_prefix
    );

    let handled = {
        if ui.popup.is_none() {
            page::handle_key_sequence_for_page(&key_sequence, client_pub, state, ui)?
        } else {
            popup::handle_key_sequence_for_popup(&key_sequence, client_pub, state, ui)?
        }
    };

    // if the key sequence is not handled, let the global handler handle it
    let handled = if handled {
        true
    } else {
        match keymap_config.find_command_or_action_from_key_sequence(&key_sequence) {
            Some(found) => handle_command_or_action(&found, client_pub, state, ui)?,
            None => false,
        }
    };

    // if handled, clear the key sequence and count prefix
    // otherwise, the current key sequence can be a prefix of a command's shortcut
    if handled {
        ui.tui.input_key_sequence.keys = vec![];
        ui.tui.count_prefix = None;
    } else {
        // update the count prefix if the key is a digit
        match key {
            Key::None(KeyCode::Char(c)) if c.is_ascii_digit() => {
                let digit = c.to_digit(10).unwrap() as usize;
                ui.tui.input_key_sequence.keys = vec![];
                ui.tui.count_prefix = match ui.tui.count_prefix {
                    Some(count) => Some(count * 10 + digit),
                    None => {
                        if digit > 0 {
                            Some(digit)
                        } else {
                            None
                        }
                    }
                };
            }
            _ => {
                ui.tui.input_key_sequence = key_sequence;
                ui.tui.count_prefix = None;
            }
        }
    }

    Ok(())
}

/// Runs a command the current page or popup does not know about through the global handler.
///
/// This is the fallback a matched key sequence takes.
pub fn handle_command_or_action(
    found: &CommandOrAction,
    client_pub: &flume::Sender<ClientRequest>,
    state: &SharedState,
    ui: &mut Ctx<'_>,
) -> Result<bool> {
    match found {
        CommandOrAction::Action(action, target) => {
            controller::handle_global_action(*action, *target, client_pub, state, ui.view_mut())
        }
        CommandOrAction::Command(Command::SwitchTheme) => {
            // the TUI owns the theme list, so this one command is handled here
            Ok(handle_switch_theme(ui))
        }
        CommandOrAction::Command(command) => {
            controller::handle_global_command(*command, client_pub, state, ui.view_mut())
        }
    }
}

/// Runs `command` exactly as its key sequence would: the current page or popup first, then the
/// global handler.
///
/// Frontends that turn a semantic action into a command (Tern's `activate`, a click) come through
/// here, so they behave like the key press. The TSP frontend is the one that does today.
#[cfg(feature = "tsp")]
pub fn handle_command(
    command: Command,
    client_pub: &flume::Sender<ClientRequest>,
    state: &SharedState,
    ui: &mut Ctx<'_>,
) -> Result<bool> {
    let handled = if ui.popup.is_none() {
        page::handle_command_for_page(
            command,
            ui.current_page().page_type(),
            client_pub,
            state,
            ui,
        )?
    } else {
        // a popup handled before the command path (search, action list, create-playlist,
        // confirm) chooses here: without a key sequence there is nowhere else to do it
        popup::choose_in_current_popup(client_pub, state, ui)?
            || popup::handle_command_for_popup(command, client_pub, state, ui)?
    };

    if handled {
        return Ok(true);
    }

    handle_command_or_action(&CommandOrAction::Command(command), client_pub, state, ui)
}

/// Open the theme list popup. Handled by the TUI because it owns the resolved themes.
fn handle_switch_theme(ui: &mut Ctx<'_>) -> bool {
    // get the available theme names with the current theme moved to the first position
    let mut themes: Vec<String> = crate::frontend_config::get()
        .theme_config
        .themes
        .iter()
        .map(|t| t.name.clone())
        .collect();
    if let Some(id) = themes.iter().position(|name| *name == ui.tui.theme.name) {
        let theme = themes.remove(id);
        themes.insert(0, theme);
    }

    ui.popup = Some(PopupState::ThemeList(themes, Cursor::default()));
    true
}
