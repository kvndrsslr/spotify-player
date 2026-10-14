use crate::{
    client::{ClientRequest, PlayerRequest},
    command::{ActionTarget, Command, CommandOrAction},
    config, controller,
    key::{Key, KeyCode, KeySequence},
    state::{
        ActionListItem, AlbumId, ArtistId, ArtistPopupAction, ContextId, ContextPageType, Cursor,
        Id, PageState, PageType, PlaylistCreateCurrentField, PlaylistFolderItem, PlaylistId,
        PlaylistPopupAction, PopupState, SearchFocusState, SharedState,
    },
    ui::{single_line_input, Ctx, Orientation, SharedUiState},
};
use anyhow::Result;

pub(crate) mod dispatch;
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

/// Converts a crossterm key code into the core's [`KeyCode`].
///
/// `None` for a key the application does not bind (media keys, lock keys, modifiers alone).
fn key_code_from_crossterm(code: crossterm::event::KeyCode) -> Option<KeyCode> {
    use crossterm::event::KeyCode as C;
    Some(match code {
        C::Char(c) => KeyCode::Char(c),
        C::Enter => KeyCode::Enter,
        C::Tab => KeyCode::Tab,
        C::BackTab => KeyCode::BackTab,
        C::Backspace => KeyCode::Backspace,
        C::Esc => KeyCode::Esc,

        C::Left => KeyCode::Left,
        C::Right => KeyCode::Right,
        C::Up => KeyCode::Up,
        C::Down => KeyCode::Down,

        C::Insert => KeyCode::Insert,
        C::Delete => KeyCode::Delete,
        C::Home => KeyCode::Home,
        C::End => KeyCode::End,
        C::PageUp => KeyCode::PageUp,
        C::PageDown => KeyCode::PageDown,

        C::F(n) => KeyCode::F(n),
        _ => return None,
    })
}

/// Converts a crossterm key event into the core's [`Key`].
///
/// Not a `From` impl: [`Key`] is defined in `spotify_player_core`, which cannot depend on
/// crossterm.
fn key_from_event(event: crossterm::event::KeyEvent) -> Key {
    let mut modifiers = event.modifiers;
    // if the key combination contains `SHIFT`, remove it
    // because the `event.code` already represents the with-SHIFT key code
    if modifiers & crossterm::event::KeyModifiers::SHIFT == crossterm::event::KeyModifiers::SHIFT {
        modifiers ^= crossterm::event::KeyModifiers::SHIFT;
    }

    let Some(code) = key_code_from_crossterm(event.code) else {
        return Key::Unknown;
    };

    match modifiers {
        crossterm::event::KeyModifiers::NONE => Key::None(code),
        crossterm::event::KeyModifiers::ALT => Key::Alt(code),
        crossterm::event::KeyModifiers::CONTROL => Key::Ctrl(code),
        _ => Key::Unknown,
    }
}

// Handle a terminal key pressed event
fn handle_key_event(
    event: crossterm::event::KeyEvent,
    client_pub: &flume::Sender<ClientRequest>,
    state: &SharedState,
    ui_state: &SharedUiState,
) -> Result<()> {
    let key = key_from_event(event);
    let mut tui = ui_state.lock();
    let mut ui = Ctx::new(state.view.lock(), &mut tui);

    dispatch::handle_key(key, client_pub, state, &mut ui)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode as CrosstermKeyCode, KeyEvent, KeyModifiers};

    fn key(code: CrosstermKeyCode, modifiers: KeyModifiers) -> Key {
        key_from_event(KeyEvent::new(code, modifiers))
    }

    #[test]
    fn maps_every_bound_key_code() {
        for (from, to) in [
            (CrosstermKeyCode::Char('n'), KeyCode::Char('n')),
            (CrosstermKeyCode::Char(' '), KeyCode::Char(' ')),
            (CrosstermKeyCode::Enter, KeyCode::Enter),
            (CrosstermKeyCode::Tab, KeyCode::Tab),
            (CrosstermKeyCode::BackTab, KeyCode::BackTab),
            (CrosstermKeyCode::Backspace, KeyCode::Backspace),
            (CrosstermKeyCode::Esc, KeyCode::Esc),
            (CrosstermKeyCode::Left, KeyCode::Left),
            (CrosstermKeyCode::Right, KeyCode::Right),
            (CrosstermKeyCode::Up, KeyCode::Up),
            (CrosstermKeyCode::Down, KeyCode::Down),
            (CrosstermKeyCode::Insert, KeyCode::Insert),
            (CrosstermKeyCode::Delete, KeyCode::Delete),
            (CrosstermKeyCode::Home, KeyCode::Home),
            (CrosstermKeyCode::End, KeyCode::End),
            (CrosstermKeyCode::PageUp, KeyCode::PageUp),
            (CrosstermKeyCode::PageDown, KeyCode::PageDown),
            (CrosstermKeyCode::F(5), KeyCode::F(5)),
        ] {
            assert_eq!(key(from, KeyModifiers::NONE), Key::None(to), "{from:?}");
        }
    }

    #[test]
    fn maps_modifiers_like_the_keymap_strings() {
        assert_eq!(
            key(CrosstermKeyCode::Char('n'), KeyModifiers::CONTROL),
            Key::Ctrl(KeyCode::Char('n'))
        );
        assert_eq!(
            key(CrosstermKeyCode::Char('x'), KeyModifiers::ALT),
            Key::Alt(KeyCode::Char('x'))
        );
        // the code already carries SHIFT, so the modifier itself is dropped
        assert_eq!(
            key(CrosstermKeyCode::Char('G'), KeyModifiers::SHIFT),
            Key::None(KeyCode::Char('G'))
        );
        assert_eq!(
            key(CrosstermKeyCode::BackTab, KeyModifiers::SHIFT),
            Key::None(KeyCode::BackTab)
        );
    }

    #[test]
    fn unbound_codes_and_modifier_combinations_are_unknown() {
        assert_eq!(
            key(
                CrosstermKeyCode::Char('c'),
                KeyModifiers::CONTROL | KeyModifiers::ALT
            ),
            Key::Unknown
        );
        assert_eq!(
            key(CrosstermKeyCode::Null, KeyModifiers::NONE),
            Key::Unknown
        );
        assert_eq!(
            key(CrosstermKeyCode::CapsLock, KeyModifiers::NONE),
            Key::Unknown
        );
    }
}
