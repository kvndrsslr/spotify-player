mod cli;
mod event;
mod frontend;
mod frontend_config;
mod theme;
mod ui;

// The core crate owns the model, state, client, configuration and command execution.
// Re-exporting its modules keeps the frontend's `crate::…` paths unchanged.
#[cfg(feature = "media-control")]
use spotify_player_core::media_control;
#[cfg(feature = "streaming")]
use spotify_player_core::visualization;
use spotify_player_core::{
    auth, client, command, config, controller, key, playback, runtime, state, utils,
};

use anyhow::{Context, Result};
use parking_lot::Mutex;
use std::{collections::VecDeque, sync::Arc};

use crate::config::apply_config_override;

#[tokio::main]
async fn start_app(
    state: &state::SharedState,
    requested_frontend: Option<frontend::FrontendKind>,
) -> Result<()> {
    let runtime = runtime::start(state)
        .await
        .context("start the application runtime")?;

    // Now that the client is up (and any first-run authentication is over, which needs the
    // terminal to itself), pick the frontend that can actually run here: this is where TSP's
    // handshake probes the terminal. A daemon renders nothing, so it has no frontend.
    let frontend = match requested_frontend {
        None => None,
        Some(requested) => Some(frontend::select(requested)?),
    };

    // client socket task (for handling CLI commands)
    tokio::task::spawn({
        let client = runtime.client.clone();
        let state = state.clone();
        async move {
            cli::start_socket(&client, Some(&state), None).await;
        }
    });

    if let Some(frontend) = frontend {
        frontend::start(
            frontend,
            &frontend::FrontendContext {
                state: state.clone(),
                client_tx: runtime.client_tx.clone(),
            },
        )?;
    }

    #[cfg(feature = "media-control")]
    if config::get_config().app_config.enable_media_control {
        // media control task
        std::thread::Builder::new()
            .name("media-control".to_string())
            .spawn({
                let state = state.clone();
                let client_tx = runtime.client_tx.clone();
                move || {
                    if let Err(err) = media_control::start_event_watcher(&state, client_tx) {
                        tracing::error!(
                            "Failed to start the application's media control event watcher: err={err:#?}"
                        );
                    }
                }
            })?;

        // the winit's event loop must be run in the main thread
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            // Start an event loop that listens to OS window events.
            //
            // MacOS and Windows require an open window to be able to listen to media
            // control events. The below code will create an invisible window on startup
            // to listen to such events.
            let event_loop = winit::event_loop::EventLoop::new()?;
            #[allow(deprecated)]
            event_loop.run(move |_, _| {})?;
        }
    }

    // infinite loop to keep the main thread alive
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

fn main() -> Result<()> {
    // librespot depends on hyper-rustls which requires a crypto provider to be set up.
    // TODO: see if this can be fixed upstream
    rustls::crypto::ring::default_provider()
        .install_default()
        .unwrap();

    // parse command line arguments
    let args = cli::init_cli()?.get_matches();

    // initialize the application's cache and config folders
    let config_folder: std::path::PathBuf = args
        .get_one::<String>("config-folder")
        .expect("config-folder should have default value")
        .into();
    if !config_folder.exists() {
        std::fs::create_dir_all(&config_folder)?;
    }

    let cache_folder: std::path::PathBuf = args
        .get_one::<String>("cache-folder")
        .expect("cache-folder should have a default value")
        .into();
    let cache_audio_folder = cache_folder.join("audio");
    if !cache_audio_folder.exists() {
        std::fs::create_dir_all(&cache_audio_folder)?;
    }
    let cache_image_folder = cache_folder.join("image");
    if !cache_image_folder.exists() {
        std::fs::create_dir_all(&cache_image_folder)?;
    }

    // initialize the application configs
    {
        let mut configs = config::Configs::new(&config_folder, &cache_folder)?;
        if configs.app_config.log_folder.is_none() {
            // set the log folder to be the cache folder if it is not set
            configs.app_config.log_folder = Some(cache_folder);
        }
        if let Some(overrides) = args.get_many::<String>("config-override") {
            for override_str in overrides {
                let (key, value) = override_str.split_once('=').context(format!(
                    "Invalid override format: '{override_str}'. Expected KEY=VALUE"
                ))?;

                apply_config_override(&mut configs.app_config, key, value)?;
            }
        }
        config::set_config(configs);
    }

    // initialize the frontend (theme) config
    frontend_config::set(frontend_config::FrontendConfigs::new(&config_folder)?);

    // the frontend the command line asks for, if any; `app.toml`'s `frontend` option and `auto`
    // are resolved once the configuration is loaded
    let requested_frontend = args.get_one::<String>("frontend").map(String::as_str);

    match args.subcommand() {
        None => {
            // initialize the application's log
            let log_folder = config::get_config()
                .app_config
                .log_folder
                .as_deref()
                .expect("log_folder is set");

            let log_buffer: Arc<Mutex<VecDeque<String>>> =
                Arc::new(Mutex::new(VecDeque::with_capacity(1000)));

            runtime::init_logging(log_folder, log_buffer.clone())
                .context("failed to initialize application's logging")?;

            // log the application's configurations
            tracing::info!("Configurations: {:?}", config::get_config());

            let is_daemon;

            #[cfg(feature = "daemon")]
            {
                is_daemon = args.get_flag("daemon");
                if is_daemon {
                    if cfg!(any(target_os = "macos", target_os = "windows"))
                        && cfg!(feature = "media-control")
                    {
                        eprintln!("Running the application as a daemon on windows/macos with `media-control` feature enabled is not supported!");
                        std::process::exit(1);
                    }

                    tracing::info!("Starting the application as a daemon...");
                    let daemonize = daemonize::Daemonize::new();
                    daemonize.start()?;
                }
            }

            #[cfg(not(feature = "daemon"))]
            {
                is_daemon = false;
            }

            // Resolve which frontend to run before the client and the socket server start: an
            // unknown `--frontend`, or a frontend this build does not have, fails immediately. A
            // daemon renders nothing, so it has no frontend.
            let frontend = if is_daemon {
                None
            } else {
                Some(frontend::resolve(
                    requested_frontend,
                    &config::get_config().app_config.frontend,
                )?)
            };

            let state = std::sync::Arc::new(state::State::new(is_daemon, log_buffer));
            start_app(&state, frontend)
        }
        Some((cmd, args)) => cli::handle_cli_subcommand(cmd, args),
    }
}
