//! Tern keys → core keys, and the routing every frontend shares.
//!
//! The mapping mirrors what the TUI's crossterm adapter produces for the same keystroke, so a
//! binding behaves the same in either frontend (see [`key_from_tern`]).

use anyhow::Result;
use spotify_player_core::{
    client::ClientRequest,
    key::{Key as CoreKey, KeyCode},
    state::SharedState,
};
use tern_sdk::keys::Key as TernKey;

use crate::{event::dispatch, ui::Ctx};

/// Routes one key press, or a paste, through the shared pipeline.
pub fn handle(
    key: &TernKey,
    state: &SharedState,
    ui_state: &mut crate::ui::UIState,
    client_tx: &flume::Sender<ClientRequest>,
) -> Result<()> {
    // A paste is text, not a key. The TUI has bracketed paste off, so it receives pasted
    // characters as the key presses for them; expanding a paste the same way keeps the two
    // frontends identical (and the text fields, which take one character at a time, work).
    if key.name == "paste" {
        let mut ui = Ctx::new(state.view.lock(), ui_state);
        for c in key.text.as_deref().unwrap_or_default().chars() {
            dispatch::handle_key(char_key(c), client_tx, state, &mut ui)?;
        }
        return Ok(());
    }

    let Some(key) = key_from_tern(key) else {
        return Ok(());
    };

    let mut ui = Ctx::new(state.view.lock(), ui_state);
    dispatch::handle_key(key, client_tx, state, &mut ui)
}

/// The key press a pasted character stands for.
fn char_key(c: char) -> CoreKey {
    match c {
        '\n' | '\r' => CoreKey::None(KeyCode::Enter),
        '\t' => CoreKey::None(KeyCode::Tab),
        _ => CoreKey::None(KeyCode::Char(c)),
    }
}

/// Converts a Tern key into the core's [`CoreKey`].
///
/// `None` for a key the application cannot bind: media and functional keys, lone modifiers,
/// ⌘/meta chords, and combinations of more than one modifier — the same set that yields
/// [`CoreKey::Unknown`] from a crossterm event.
pub fn key_from_tern(key: &TernKey) -> Option<CoreKey> {
    if key.meta || (key.ctrl && key.alt) {
        return None;
    }

    // A key with Control, Alt or Meta never types text (the decoder clears it for those), so `text`
    // is the typed character: the decoder keeps the shifted character there, which is what
    // crossterm reports as the key code, so `Shift+G` is `Char('G')` in both frontends.
    if !key.ctrl && !key.alt {
        if let Some(text) = key.text.as_deref() {
            let mut chars = text.chars();
            let c = chars.next()?;
            return if chars.next().is_some() {
                None
            } else {
                Some(CoreKey::None(KeyCode::Char(c)))
            };
        }
    }

    // ⇧Tab is `BackTab` in the core's vocabulary: crossterm reports it as its own key code rather
    // than as Tab with Shift.
    let code = if key.name == "tab" && key.shift {
        KeyCode::BackTab
    } else {
        key_code_from_tern(&key.name)?
    };

    match (key.ctrl, key.alt) {
        (false, false) => Some(CoreKey::None(code)),
        (true, false) => Some(CoreKey::Ctrl(code)),
        (false, true) => Some(CoreKey::Alt(code)),
        (true, true) => None,
    }
}

/// The core key code behind a Tern key name: a one-character name is that character, anything else
/// is a named key.
fn key_code_from_tern(name: &str) -> Option<KeyCode> {
    let code = match name {
        "enter" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "backspace" => KeyCode::Backspace,
        // the decoder spells it out (`esc` in `keymap.toml`)
        "escape" => KeyCode::Esc,
        "space" => KeyCode::Char(' '),

        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,

        "insert" => KeyCode::Insert,
        "delete" => KeyCode::Delete,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "page_up" => KeyCode::PageUp,
        "page_down" => KeyCode::PageDown,

        "f1" => KeyCode::F(1),
        "f2" => KeyCode::F(2),
        "f3" => KeyCode::F(3),
        "f4" => KeyCode::F(4),
        "f5" => KeyCode::F(5),
        "f6" => KeyCode::F(6),
        "f7" => KeyCode::F(7),
        "f8" => KeyCode::F(8),
        "f9" => KeyCode::F(9),
        "f10" => KeyCode::F(10),
        "f11" => KeyCode::F(11),
        "f12" => KeyCode::F(12),

        // `f13`+ and the keypad, media and lock keys the application does not bind
        _ => {
            let mut chars = name.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            KeyCode::Char(c)
        }
    };

    Some(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tern_sdk::keys::Key;

    fn named(name: &str) -> Key {
        Key::named(name)
    }

    #[test]
    fn typed_characters_keep_their_case() {
        // `Shift+G` reaches the decoder as name "g" with the typed character in `text`
        let shifted = Key {
            text: Some("G".to_owned()),
            shift: true,
            ..named("g")
        };
        assert_eq!(
            key_from_tern(&shifted),
            Some(CoreKey::None(KeyCode::Char('G')))
        );
        assert_eq!(
            key_from_tern(&Key {
                text: Some("g".to_owned()),
                ..named("g")
            }),
            Some(CoreKey::None(KeyCode::Char('g')))
        );
        // a space is `Char(' ')`, the core's spelling of the keymap's `space`
        assert_eq!(
            key_from_tern(&Key {
                text: Some(" ".to_owned()),
                ..named("space")
            }),
            Some(CoreKey::None(KeyCode::Char(' ')))
        );
    }

    #[test]
    fn control_and_alt_use_the_key_name() {
        let ctrl_n = Key {
            ctrl: true,
            ..named("n")
        };
        assert_eq!(
            key_from_tern(&ctrl_n),
            Some(CoreKey::Ctrl(KeyCode::Char('n')))
        );

        let alt_x = Key {
            alt: true,
            ..named("x")
        };
        assert_eq!(
            key_from_tern(&alt_x),
            Some(CoreKey::Alt(KeyCode::Char('x')))
        );

        // the kitty protocol spells ctrl+space as `space` with Control
        let ctrl_space = Key {
            ctrl: true,
            ..named("space")
        };
        assert_eq!(
            key_from_tern(&ctrl_space),
            Some(CoreKey::Ctrl(KeyCode::Char(' ')))
        );
    }

    #[test]
    fn named_keys_map_to_their_core_code() {
        assert_eq!(
            key_from_tern(&named("enter")),
            Some(CoreKey::None(KeyCode::Enter))
        );
        assert_eq!(
            key_from_tern(&named("escape")),
            Some(CoreKey::None(KeyCode::Esc))
        );
        assert_eq!(
            key_from_tern(&named("page_up")),
            Some(CoreKey::None(KeyCode::PageUp))
        );
        assert_eq!(
            key_from_tern(&named("f5")),
            Some(CoreKey::None(KeyCode::F(5)))
        );
        assert_eq!(
            key_from_tern(&named("backspace")),
            Some(CoreKey::None(KeyCode::Backspace))
        );
    }

    #[test]
    fn shifted_tab_is_backtab() {
        let backtab = Key {
            shift: true,
            ..named("tab")
        };
        assert_eq!(
            key_from_tern(&backtab),
            Some(CoreKey::None(KeyCode::BackTab))
        );
        assert_eq!(
            key_from_tern(&named("tab")),
            Some(CoreKey::None(KeyCode::Tab))
        );
    }

    #[test]
    fn unbound_keys_do_not_map() {
        for key in [
            Key {
                meta: true,
                ..named("c")
            },
            Key {
                ctrl: true,
                alt: true,
                ..named("c")
            },
            named("f13"),
            named("menu"),
            named("begin"),
            named("caps_lock"),
        ] {
            assert_eq!(key_from_tern(&key), None, "{key:?} should not map");
        }
    }
}
