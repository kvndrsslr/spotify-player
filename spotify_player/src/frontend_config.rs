//! The frontend's own configuration: the theme.
//!
//! Core owns the application configuration (`app.toml`) and the keybindings (`keymap.toml`, see
//! [`crate::config::Configs::keymap_config`]); the theme is a rendering concern, so it is loaded
//! and owned here.

use std::sync::OnceLock;

use crate::theme::ThemeConfig;

static CONFIGS: OnceLock<FrontendConfigs> = OnceLock::new();

#[derive(Debug)]
pub struct FrontendConfigs {
    pub theme_config: ThemeConfig,
}

impl FrontendConfigs {
    pub fn new(config_folder: &std::path::Path) -> anyhow::Result<Self> {
        Ok(Self {
            theme_config: ThemeConfig::new(config_folder)?,
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
