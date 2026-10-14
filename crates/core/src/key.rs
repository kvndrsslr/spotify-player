//! The application's key vocabulary, with no dependency on a terminal library.
//!
//! A frontend converts its terminal's key events into [`Key`] and looks the resulting
//! [`KeySequence`] up in [`crate::keymap::KeymapConfig`], so the user's `keymap.toml` binds every
//! frontend the same.

/// A key code, with the variants the application binds.
///
/// Frontends convert their own terminal key representations into this type, so the keymap below
/// stays independent of any terminal library.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum KeyCode {
    Char(char),
    Enter,
    Tab,
    BackTab,
    Backspace,
    Esc,
    Left,
    Right,
    Up,
    Down,
    Insert,
    Delete,
    Home,
    End,
    PageUp,
    PageDown,
    F(u8),
}

/// Key represents a key received from user's input
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Key {
    Unknown,
    None(KeyCode),
    Ctrl(KeyCode),
    Alt(KeyCode),
}

/// `KeySequence` represents a combination of pressed keys
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeySequence {
    pub keys: Vec<Key>,
}

impl Key {
    fn parse_key_code(s: &str) -> Option<KeyCode> {
        Some(match s {
            "enter" => KeyCode::Enter,
            "space" => KeyCode::Char(' '),
            "tab" => KeyCode::Tab,
            "backtab" => KeyCode::BackTab,
            "backspace" => KeyCode::Backspace,
            "esc" => KeyCode::Esc,

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

            _ => {
                let chars = s.chars().collect::<Vec<_>>();
                if chars.len() == 1 && chars[0] != ' ' {
                    KeyCode::Char(chars[0])
                } else {
                    return None;
                }
            }
        })
    }

    /// creates a `Key` from its string representation
    ///
    /// The application parses its key strings (`"C-x"`, `"g a"`, `"page_up"`) with an
    /// `Option`-returning method, so this is intentionally not `std::str::FromStr`.
    #[allow(clippy::should_implement_trait)]
    #[must_use]
    pub fn from_str(s: &str) -> Option<Self> {
        let chars = s.chars().collect::<Vec<_>>();
        if chars.len() > 2 && chars[1] == '-' && chars[2] != ' ' {
            // M-<c> for alt-<c> and C-<c> for ctrl-<c>
            let key = Self::parse_key_code(&chars[2..].iter().collect::<String>());
            match chars[0] {
                'C' => key.map(Key::Ctrl),
                'M' => key.map(Key::Alt),
                _ => None,
            }
        } else {
            Self::parse_key_code(s).map(Key::None)
        }
    }
}

fn key_code_to_string(k: KeyCode) -> String {
    match k {
        KeyCode::Char(c) => {
            if c == ' ' {
                "space".to_string()
            } else {
                c.to_string()
            }
        }
        KeyCode::Enter => "enter".to_string(),
        KeyCode::Tab => "tab".to_string(),
        KeyCode::BackTab => "backtab".to_string(),
        KeyCode::Backspace => "backspace".to_string(),
        KeyCode::Esc => "esc".to_string(),

        KeyCode::Left => "left".to_string(),
        KeyCode::Right => "right".to_string(),
        KeyCode::Up => "up".to_string(),
        KeyCode::Down => "down".to_string(),

        KeyCode::Insert => "insert".to_string(),
        KeyCode::Delete => "delete".to_string(),
        KeyCode::Home => "home".to_string(),
        KeyCode::End => "end".to_string(),
        KeyCode::PageUp => "page_up".to_string(),
        KeyCode::PageDown => "page_down".to_string(),

        KeyCode::F(n) => format!("f{n}"),
    }
}

impl std::fmt::Display for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Key::Ctrl(k) => write!(f, "C-{}", key_code_to_string(k)),
            Key::Alt(k) => write!(f, "M-{}", key_code_to_string(k)),
            Key::None(k) => write!(f, "{}", key_code_to_string(k)),
            Key::Unknown => write!(f, "unknown key"),
        }
    }
}

impl<'de> serde::de::Deserialize<'de> for Key {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match Self::from_str(&s) {
            Some(key) => Ok(key),
            None => Err(serde::de::Error::custom(format!(
                "failed to parse key: unknown key {s}"
            ))),
        }
    }
}

impl KeySequence {
    /// creates a `KeySequence` from its string representation
    ///
    /// As with [`Key::from_str`], this is intentionally not `std::str::FromStr`.
    #[allow(clippy::should_implement_trait)]
    #[must_use]
    pub fn from_str(s: &str) -> Option<Self> {
        let keys = s.split(' ').collect::<Vec<_>>();
        if keys.is_empty() {
            return None;
        }
        keys.into_iter()
            .map(Key::from_str)
            .collect::<Option<Vec<_>>>()
            .map(|keys| Self { keys })
    }

    /// checks if a key sequence is a prefix of `other` key sequence
    #[must_use]
    pub fn is_prefix(&self, other: &Self) -> bool {
        if self.keys.len() > other.keys.len() {
            return false;
        }
        (0..self.keys.len()).all(|i| self.keys[i] == other.keys[i])
    }
}

impl std::fmt::Display for KeySequence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            self.keys
                .iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>()
                .join(" ")
        )
    }
}

impl<'de> serde::de::Deserialize<'de> for KeySequence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match KeySequence::from_str(&s) {
            Some(key_sequence) => Ok(key_sequence),
            None => Err(serde::de::Error::custom(format!(
                "failed to parse key sequence: invalid key sequence {s}"
            ))),
        }
    }
}

impl From<&str> for Key {
    /// converts a string into a `Key`.
    /// # Panics
    /// This function will panic if the given string is not a valid
    /// representation of a `Key`.
    fn from(s: &str) -> Self {
        Self::from_str(s).unwrap_or_else(|| panic!("invalid key {s}"))
    }
}

impl From<&str> for KeySequence {
    /// converts a string into a `KeySequence`.
    /// # Panics
    /// This function will panic if the given string is not a valid
    /// representation of a `KeySequence`.
    fn from(s: &str) -> Self {
        Self::from_str(s).unwrap_or_else(|| panic!("invalid key sequence {s}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_strings_round_trip() {
        // every key string the application and its `keymap.toml` files use
        let codes = [
            "enter",
            "space",
            "tab",
            "backtab",
            "backspace",
            "esc",
            "left",
            "right",
            "up",
            "down",
            "insert",
            "delete",
            "home",
            "end",
            "page_up",
            "page_down",
            "f1",
            "f5",
            "f10",
            "f12",
            "n",
            "N",
            "g",
            "G",
            "0",
            "+",
            "-",
            "_",
            "^",
            ">",
            "<",
            "/",
            "?",
            ".",
            "é",
        ];

        let mut strings = Vec::new();
        for code in codes {
            strings.push(code.to_string());
            strings.push(format!("C-{code}"));
            strings.push(format!("M-{code}"));
        }

        for s in &strings {
            let key = Key::from_str(s).unwrap_or_else(|| panic!("{s} should parse"));
            assert_eq!(&key.to_string(), s, "{s} should round-trip through Display");
        }
    }

    #[test]
    fn modifiers_parse_and_display() {
        assert_eq!(Key::from_str("C-n"), Some(Key::Ctrl(KeyCode::Char('n'))));
        assert_eq!(Key::from_str("M-x"), Some(Key::Alt(KeyCode::Char('x'))));
        assert_eq!(
            Key::from_str("C-page_down"),
            Some(Key::Ctrl(KeyCode::PageDown))
        );
        assert_eq!(
            Key::from_str("C-space"),
            Some(Key::Ctrl(KeyCode::Char(' ')))
        );
    }

    #[test]
    fn invalid_key_strings_do_not_parse() {
        for s in ["", " ", "ab", "F1", "ctrl-n", "C- ", "C-ab", "space bar"] {
            assert_eq!(Key::from_str(s), None, "{s:?} should not parse");
        }
    }

    #[test]
    fn sequences_split_on_space_and_compare_by_prefix() {
        let sequence = KeySequence::from_str("g a").expect("parses");
        assert_eq!(
            sequence.keys,
            vec![Key::None(KeyCode::Char('g')), Key::None(KeyCode::Char('a'))]
        );
        assert_eq!(sequence.to_string(), "g a");

        let prefix = KeySequence::from_str("s").expect("parses");
        assert!(prefix.is_prefix(&KeySequence::from_str("s l").expect("parses")));
        assert!(!prefix.is_prefix(&KeySequence::from_str("l").expect("parses")));
        assert_eq!(KeySequence::from_str(""), None);
    }
}
