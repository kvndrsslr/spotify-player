//! The frontend's own configuration: themes and keybindings.
//!
//! Core owns the application configuration (`app.toml`, see [`crate::config`]); themes and
//! keymaps are rendering/input concerns, so they are loaded and owned here.

use std::sync::OnceLock;

use crate::{keymap::KeymapConfig, theme::ThemeConfig};

static CONFIGS: OnceLock<FrontendConfigs> = OnceLock::new();

#[derive(Debug)]
pub struct FrontendConfigs {
    pub theme_config: ThemeConfig,
    pub keymap_config: KeymapConfig,
}

impl FrontendConfigs {
    pub fn new(config_folder: &std::path::Path) -> anyhow::Result<Self> {
        Ok(Self {
            theme_config: ThemeConfig::new(config_folder)?,
            keymap_config: KeymapConfig::new(config_folder)?,
        })
    }
}

/// Returns the frontend configurations, which must have been set by [`set`] first.
pub fn get() -> &'static FrontendConfigs {
    CONFIGS
        .get()
        .expect("frontend configs are already initialized")
}

pub fn set(configs: FrontendConfigs) {
    CONFIGS
        .set(configs)
        .expect("frontend configs should be initialized only once");
}
