//! Frontend-agnostic application startup: logging, the Spotify client and the background tasks.
//!
//! A frontend calls [`init_logging`] and then [`start`], and keeps only its own loop, terminal
//! lifecycle and configuration handling.

use std::{collections::VecDeque, io::Write, path::Path, sync::Arc};

use anyhow::{Context, Result};
use flume::Sender;
use parking_lot::Mutex;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::{
    client::{self, AppClient, ClientRequest},
    state,
    state::SharedState,
};

/// The application's running client and its request channel.
///
/// [`start`] returns this so a frontend can make its own API calls through [`Runtime::client`]
/// and ask core to talk to Spotify or control playback through [`Runtime::client_tx`].
pub struct Runtime {
    /// The Spotify API client, shared with the background tasks.
    pub client: AppClient,
    /// How a frontend sends [`ClientRequest`]s to core's client task.
    pub client_tx: Sender<ClientRequest>,
}

/// Initializes the application's logging: `log_folder`'s log file, the in-memory buffer a log view
/// reads ([`state::State::logs`]) and the panic backtrace file.
///
/// `redraw` is the signal in the state the caller is about to build: the buffer layer raises it so
/// a log view redraws on new lines.
///
/// Does nothing when `RUST_LOG=off`.
pub fn init_logging(
    log_folder: &Path,
    log_buffer: Arc<Mutex<VecDeque<String>>>,
    redraw: state::RedrawNotify,
) -> Result<()> {
    if std::env::var_os("RUST_LOG").is_some_and(|x| x == "off") {
        // Don't create log files if logging is disabled.
        return Ok(());
    }

    let log_prefix = format!(
        "spotify-player-{}",
        chrono::Local::now().format("%y-%m-%d-%H-%M")
    );

    // initialize the application's logging
    if std::env::var("RUST_LOG").is_err() {
        // default to log the current crate and librespot crates
        std::env::set_var("RUST_LOG", "spotify_player=info,librespot=info");
    }
    if !log_folder.exists() {
        std::fs::create_dir_all(log_folder)?;
    }
    let log_file = std::fs::File::create(log_folder.join(format!("{log_prefix}.log")))
        .context("failed to create log file")?;

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(std::sync::Mutex::new(log_file));

    let buffer_layer = crate::log_layer::BufferLayer::new(log_buffer, 1000, redraw);

    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(fmt_layer)
        .with(buffer_layer)
        .init();

    // initialize the application's panic backtrace
    let backtrace_file = std::fs::File::create(log_folder.join(format!("{log_prefix}.backtrace")))
        .context("failed to create backtrace file")?;
    // `parking_lot::Mutex` so a panic while the hook holds it can never poison it and abort on a
    // second panic.
    let backtrace_file = Mutex::new(backtrace_file);
    std::panic::set_hook(Box::new(move |info| {
        let mut file = backtrace_file.lock();
        let backtrace = backtrace::Backtrace::new();
        writeln!(&mut file, "Got a panic: {info:#?}\n").unwrap();
        writeln!(&mut file, "Stack backtrace:\n{backtrace:?}").unwrap();
    }));

    Ok(())
}

/// Starts the Spotify client and the background tasks every frontend needs.
///
/// Spawns, in order: the client request handler, the session watcher, the `image`-gated
/// cover-image recovery loop and the `player-event-watcher` thread. Anything frontend-specific
/// (the CLI socket server, os media control, the render loop) is the caller's to spawn.
///
/// Must be called from within a Tokio runtime.
pub async fn start(state: &SharedState) -> Result<Runtime> {
    // client channels
    let (client_pub, client_sub) = flume::unbounded::<client::ClientRequest>();

    #[cfg(feature = "pulseaudio-backend")]
    {
        // set environment variables for PulseAudio
        if std::env::var("PULSE_PROP_application.name").is_err() {
            std::env::set_var("PULSE_PROP_application.name", "spotify-player");
        }
        if std::env::var("PULSE_PROP_application.icon_name").is_err() {
            std::env::set_var("PULSE_PROP_application.icon_name", "spotify");
        }
        if std::env::var("PULSE_PROP_stream.description").is_err() {
            let configs = crate::config::get_config();
            std::env::set_var(
                "PULSE_PROP_stream.description",
                format!(
                    "Spotify Connect endpoint ({})",
                    configs.app_config.device.name
                ),
            );
        }
        if std::env::var("PULSE_PROP_media.software").is_err() {
            std::env::set_var("PULSE_PROP_media.software", "Spotify");
        }
        if std::env::var("PULSE_PROP_media.role").is_err() {
            std::env::set_var("PULSE_PROP_media.role", "music");
        }
    }

    // create a Spotify API client
    let client = client::AppClient::new()
        .await
        .context("construct app client")?;
    client
        .new_session(Some(state), true)
        .await
        .context("initialize new Spotify session")?;

    // request user data
    client_pub.send(client::ClientRequest::GetCurrentUser)?;
    client_pub.send(client::ClientRequest::GetUserPlaylists)?;
    client_pub.send(client::ClientRequest::GetUserFollowedArtists)?;
    client_pub.send(client::ClientRequest::GetUserSavedAlbums)?;
    client_pub.send(client::ClientRequest::GetContext(state::ContextId::Tracks(
        state::USER_LIKED_TRACKS_ID.to_owned(),
    )))?;
    client_pub.send(client::ClientRequest::GetUserSavedShows)?;

    // client event handler task
    tokio::task::spawn({
        let state = state.clone();
        let client = client.clone();
        async move {
            client::start_client_handler(&state, &client, &client_sub).await;
        }
    });

    // background task that detects an invalidated session and reconnects,
    // independent of any incoming client request
    tokio::task::spawn({
        let state = state.clone();
        let client = client.clone();
        async move {
            client::start_session_watcher(state, client).await;
        }
    });

    #[cfg(feature = "image")]
    // background task that refetches cover images the UI reported missing from cache
    // (e.g. evicted while browsing long shows), with exponential backoff per url
    tokio::task::spawn({
        let client = client.clone();
        let state = state.clone();
        async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
            loop {
                interval.tick().await;
                client.recover_missing_images(&state);
            }
        }
    });

    // player event watcher task
    std::thread::Builder::new()
        .name("player-event-watcher".to_string())
        .spawn({
            let state = state.clone();
            let client_pub = client_pub.clone();
            move || {
                client::start_player_event_watcher(&state, &client_pub);
            }
        })?;

    Ok(Runtime {
        client,
        client_tx: client_pub,
    })
}
