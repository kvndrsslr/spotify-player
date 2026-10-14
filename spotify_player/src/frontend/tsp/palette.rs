//! The program palette: how the active theme colours the Tern surface.

use tern_sdk::wire::{Palette, PaletteNames};

/// The palette for `theme`.
///
/// A theme has one variant, so it is sent for both appearances; Tern picks the one matching its own
/// and ignores the other.
pub fn palette(theme: &crate::theme::Theme) -> Palette {
    let tokens = theme.tsp_palette();

    Palette {
        sf: None,
        dark: Some(tokens.clone()),
        light: Some(tokens),
        name: Some(PaletteNames {
            dark: Some(theme.name.clone()),
            light: Some(theme.name.clone()),
        }),
    }
}
