//! Framework-agnostic core of `spotify-player`.
//!
//! This crate owns everything that is independent of how the application is drawn or how
//! input is read: the domain model, the shared application state and its view model, the
//! Spotify client, playback, configuration and the command executor. It deliberately does
//! **not** depend on `ratatui`, `crossterm` or `ratatui-image`, so a frontend can be
//! swapped without touching it.
//!
//! # Boundary
//!
//! Frontends keep: rendering, input mapping (key/mouse/pointer → [`command::Command`]),
//! their own theme and keymap configuration, terminal/window lifecycle and image
//! *encoding*. Core keeps the *what*; frontends own the *how*.
//!
//! # What a frontend consumes
//!
//! - [`state::SharedState`] — `Arc<state::State>`, the single shared application state:
//!   - `state::State::data` (`RwLock<state::AppData>`): user library, contexts, search
//!     results, lyrics, image caches (`caches.images`, `caches.episode_images` hold decoded
//!     `image::DynamicImage`s fetched and downscaled by core — frontends only encode them).
//!   - `state::State::player` (`RwLock<state::PlayerState>`): current playback, devices,
//!     volume, queue and progress estimation.
//!   - `state::State::view` (`Mutex<state::ViewState>`): the framework-agnostic view model —
//!     page history ([`state::PageState`]), popups ([`state::PopupState`]), cursors
//!     ([`state::Cursor`]) and text-input buffers ([`state::TextInput`]).
//!   - `state::State::vis_bands`: shared FFT bands for an audio visualizer
//!     ([`visualization::VisBands`], see also [`state::State::is_local_streaming_active`]).
//!   - `state::State::logs`: recent log lines for a log view.
//! - [`client::ClientRequest`] over a `flume` channel — how a frontend asks core to talk to
//!   Spotify or to control playback. Core's client task applies the results to [`state`].
//! - [`command::Command`] / [`command::Action`] plus [`controller`] — the executor entry
//!   points. [`controller::handle_global_command`],
//!   [`controller::handle_action_in_context`] (and the page-scoped helpers around them)
//!   take `&mut state::ViewStateGuard` and the request sender, mutate the view model and
//!   issue [`client::ClientRequest`]s. Input mapping and popup/page routing stay in the
//!   frontend.
//! - [`controller::handle_navigation_command`] — cursor movement over a page's focused
//!   window, so every frontend shares the same vim-style navigation semantics.
//! - [`config`] — [`config::get_config`] exposes [`config::AppConfig`] (theme *name*, icons,
//!   layout ratios, durations, device/session settings) and the config/cache folders.
//!   Frontends load their own theme/keymap files from [`config::Configs::config_folder`]
//!   (`theme.toml`, `keymap.toml`); core never interprets them.
//! - [`state::State::queue_image_fetch`] — core drains this queue on a timer and refetches
//!   cover art a frontend reported missing.
//!
//! # Polling
//!
//! There is no redraw notification: frontends poll [`state`] at their own cadence
//! (`config::AppConfig::app_refresh_duration_in_ms`) and re-render when they choose.
//! Shutdown is cooperative through [`state::ViewState::is_running`], which the executor
//! clears for [`command::Command::Quit`].

pub mod auth;
pub mod client;
pub mod clipboard;
pub mod command;
pub mod config;
pub mod controller;
pub mod log_layer;
#[cfg(feature = "media-control")]
pub mod media_control;
pub mod playlist_folders;
pub mod state;
#[cfg(feature = "streaming")]
pub mod streaming;
pub mod token;
pub mod utils;
#[cfg(feature = "streaming")]
pub mod visualization;
