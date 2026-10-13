use crate::{
    client::{ClientRequest, PlayerRequest},
    command::{ActionTarget, Command, CommandOrAction},
    config, controller,
    key::{Key, KeySequence},
    state::{
        ActionListItem, AlbumId, ArtistId, ArtistPopupAction, ContextId, ContextPageType, Cursor,
        Id, PageState, PageType, PlaylistCreateCurrentField, PlaylistFolderItem, PlaylistId,
        PlaylistPopupAction, PopupState, SearchFocusState, SharedState,
    },
    ui::{single_line_input, Ctx, Orientation, SharedUiState},
};
use anyhow::Result;
use crossterm::event::KeyCode;

mod page;
mod popup;

/// Start a terminal event handler (key pressed, mouse clicked, etc)
pub fn start_event_handler(
    state: &SharedState,
    ui_state: &SharedUiState,
    client_pub: &flume::Sender<ClientRequest>,
) {
    while let Ok(event) = crossterm::event::read() {
        let _enter = tracing::info_span!("terminal_event", event = ?event).entered();
        if let Err(err) = match event {
            crossterm::event::Event::Mouse(event) => {
                handle_mouse_event(event, client_pub, state, ui_state)
            }
            crossterm::event::Event::Resize(columns, rows) => {
                ui_state.lock().orientation = Orientation::from_size(columns, rows);
                Ok(())
            }
            crossterm::event::Event::Key(event) => {
                if event.kind == crossterm::event::KeyEventKind::Press {
                    // only handle key press event to avoid handling a key event multiple times
                    // context:
                    // - https://github.com/crossterm-rs/crossterm/issues/752
                    // - https://github.com/aome510/spotify-player/issues/136
                    handle_key_event(event, client_pub, state, ui_state)
                } else {
                    Ok(())
                }
            }
            _ => Ok(()),
        } {
            tracing::error!("Failed to handle terminal event: {err:#}");
        }
    }
}

// Handle a terminal mouse event
fn handle_mouse_event(
    event: crossterm::event::MouseEvent,
    client_pub: &flume::Sender<ClientRequest>,
    state: &SharedState,
    ui_state: &SharedUiState,
) -> Result<()> {
    tracing::debug!("Handling mouse event: {event:?}");

    let enable_scroll = config::get_config().app_config.enable_mouse_scroll_volume;

    match event.kind {
        crossterm::event::MouseEventKind::ScrollUp if enable_scroll => {
            let step = config::get_config().app_config.volume_scroll_step;
            let mut volume_target = state.volume_target.lock();
            let base = volume_target
                .or_else(|| {
                    state
                        .player
                        .read()
                        .buffered_playback
                        .as_ref()
                        .and_then(|p| p.volume.map(|v| v as u8))
                })
                .unwrap_or(0);
            let new_volume = std::cmp::min(base + step, 100);
            *volume_target = Some(new_volume);
            client_pub.send(ClientRequest::Player(PlayerRequest::Volume(new_volume)))?;
        }
        crossterm::event::MouseEventKind::ScrollDown if enable_scroll => {
            let step = config::get_config().app_config.volume_scroll_step;
            let mut volume_target = state.volume_target.lock();
            let base = volume_target
                .or_else(|| {
                    state
                        .player
                        .read()
                        .buffered_playback
                        .as_ref()
                        .and_then(|p| p.volume.map(|v| v as u8))
                })
                .unwrap_or(0);
            let new_volume = base.saturating_sub(step);
            *volume_target = Some(new_volume);
            client_pub.send(ClientRequest::Player(PlayerRequest::Volume(new_volume)))?;
        }
        // a left click event
        crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
            let rect = ui_state.lock().playback_progress_bar_rect;
            if rect.contains(event.row, event.column) {
                // calculate the seek position (in ms) based on the mouse click position,
                // the progress bar's width and the track's duration (in ms)
                let player = state.player.read();
                let duration = match player.currently_playing() {
                    Some(rspotify::model::PlayableItem::Track(track)) => Some(track.duration),
                    Some(rspotify::model::PlayableItem::Episode(episode)) => Some(episode.duration),
                    Some(rspotify::model::PlayableItem::Unknown(_)) | None => None,
                };
                if let Some(duration) = duration {
                    let position_ms = duration.num_milliseconds()
                        * i64::from(event.column - rect.x)
                        / i64::from(rect.width);
                    client_pub.send(ClientRequest::Player(PlayerRequest::SeekTrack(
                        chrono::Duration::try_milliseconds(position_ms).unwrap(),
                    )))?;
                }
            }
        }
        _ => {}
    }
    Ok(())
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

// Handle a terminal key pressed event
fn handle_key_event(
    event: crossterm::event::KeyEvent,
    client_pub: &flume::Sender<ClientRequest>,
    state: &SharedState,
    ui_state: &SharedUiState,
) -> Result<()> {
    let key: Key = event.into();
    let mut tui = ui_state.lock();
    let mut ui = Ctx::new(state.view.lock(), &mut tui);

    let mut key_sequence = ui.tui.input_key_sequence.clone();
    key_sequence.keys.push(key);

    // check if the current key sequence matches any keymap's prefix
    // if not, reset the key sequence
    let keymap_config = &crate::frontend_config::get().keymap_config;
    if !keymap_config.has_matched_prefix(&key_sequence) {
        key_sequence = KeySequence { keys: vec![key] };
    }

    tracing::debug!(
        "Handling key event: {event:?}, current key sequence: {key_sequence:?}, count prefix: {:?}",
        ui.tui.count_prefix
    );
    let handled = {
        if ui.popup.is_none() {
            page::handle_key_sequence_for_page(&key_sequence, client_pub, state, &mut ui)?
        } else {
            popup::handle_key_sequence_for_popup(&key_sequence, client_pub, state, &mut ui)?
        }
    };

    // if the key sequence is not handled, let the global handler handle it
    let handled = if handled {
        true
    } else {
        match keymap_config.find_command_or_action_from_key_sequence(&key_sequence) {
            Some(CommandOrAction::Action(action, target)) => {
                controller::handle_global_action(action, target, client_pub, state, ui.view_mut())?
            }
            Some(CommandOrAction::Command(Command::SwitchTheme)) => {
                // the TUI owns the theme list, so this one command is handled here
                handle_switch_theme(&mut ui)
            }
            Some(CommandOrAction::Command(command)) => {
                controller::handle_global_command(command, client_pub, state, ui.view_mut())?
            }
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
