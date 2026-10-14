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
//! Frontends keep: rendering, terminal/window lifecycle, reading their terminal's events and
//! converting them into [`key::Key`], their own theme and image *encoding*. Core keeps the *what*:
//! the domain model, the key vocabulary and the user's keybindings ([`config::Configs::keymap_config`]),
//! and the command executor.
//!
//! # What a frontend consumes
//!
//! - [`runtime`] — [`runtime::init_logging`] and [`runtime::start`]: logging (log file, log
//!   buffer, panic backtrace) and the Spotify client plus the background tasks (client handler,
//!   session watcher, cover-image recovery, player-event watcher). A frontend spawns only what is
//!   its own (its loop, terminal lifecycle, os media control, the CLI socket server) and keeps the
//!   returned [`runtime::Runtime`].
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
//!   issue [`client::ClientRequest`]s. Page/popup routing of a matched command stays in the
//!   frontend.
//! - [`controller::handle_navigation_command`] — cursor movement over a page's focused
//!   window, so every frontend shares the same vim-style navigation semantics.
//! - [`key`] / [`keymap`] — the framework-agnostic [`key::Key`] / [`key::KeySequence`],
//!   the keymap lookup ([`keymap::KeymapConfig::find_command_or_action_from_key_sequence`])
//!   and [`state::TextInput::handle_key`]. A frontend converts its terminal's key events into
//!   [`key::Key`] and looks the resulting sequence up in the user's bindings.
//! - [`config`] — [`config::get_config`] exposes [`config::AppConfig`] (theme *name*, icons,
//!   layout ratios, durations, device/session settings), the user's loaded
//!   [`config::Configs::keymap_config`] and the config/cache folders. The frontend-owned
//!   `theme.toml` is loaded by the frontend from [`config::Configs::config_folder`].
//! - [`state::State::queue_image_fetch`] — core drains this queue on a timer and refetches
//!   cover art a frontend reported missing.
//! - [`state::State::redraw`] — the redraw signal ([`state::RedrawNotify`]): every path that
//!   changes what a frontend draws raises it, so a frontend awaits the next change instead of
//!   polling. A frontend that cannot be woken from another thread may ignore it and poll.
//!
//! # Redraws
//!
//! A frontend redraws when [`state::State::redraw`] says something changed. A few views move for
//! reasons of their own rather than on a change — the audio visualizer interpolates from the time
//! since the audio sink's last write, the playback clock advances in real time, the startup
//! spinner animates, and the `image` compositor owes grid-anchored surfaces periodic
//! re-transmissions — so a frontend also needs a deadline for those. `AppConfig::app_refresh_duration_in_ms`
//! is that frame interval, no longer the redraw cadence.
//!
//! Shutdown is cooperative through [`state::ViewState::is_running`], which the executor
//! clears for [`command::Command::Quit`]; every path clearing it raises the signal, so a waiting
//! frontend wakes and can exit.

pub mod auth;
pub mod client;
pub mod clipboard;
pub mod command;
pub mod config;
pub mod controller;
pub mod key;
pub mod keymap;
pub mod log_layer;
#[cfg(feature = "media-control")]
pub mod media_control;
pub mod playback;
pub mod playlist_folders;
pub mod runtime;
pub mod state;
#[cfg(feature = "streaming")]
pub mod streaming;
pub mod token;
pub mod utils;
#[cfg(feature = "streaming")]
pub mod visualization;
