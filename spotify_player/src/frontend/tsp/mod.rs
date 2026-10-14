//! The Tern Surface Protocol frontend: the application drawn by Tern itself.
//!
//! The terminal owns layout, animation, hover and scrolling; this frontend sends a document of
//! semantic nodes and applies the frames of ops Tern acks, and receives the user's keys on the pty
//! like any other terminal input. See `design.md` for the protocol mapping.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use spotify_player_core::{
    client::{ClientRequest, PlayerRequest},
    command::{Action, ActionTarget, Command, CommandOrAction},
    config,
    state::SharedState,
};
use tern_sdk::{
    wire::{feature, Event},
    Input, Options, Session, Surface, SurfaceOptions,
};

use crate::{
    event::dispatch,
    frontend::{Frontend, FrontendContext},
    ui::{Ctx, UIState},
};

mod blobs;
mod keys;
mod palette;
mod view;

#[cfg(test)]
mod tests;

use view::Msg;

/// The stylesheet the surface installs: Tern's look, squared off a little.
const SHEET: &str = ".sf-card { border-radius: 6px }";

/// The TSP frontend: the application drawn by Tern.
pub static FRONTEND: Frontend = Frontend {
    name: "tsp",
    probe,
    run,
};

/// Whether this terminal speaks TSP.
///
/// Runs the handshake (`hello` plus a DA1 request); the terminal is left exactly as it was found,
/// including a drain of the DA1 reply when TSP is not there.
fn probe() -> Result<bool> {
    let Some(session) = connect()? else {
        drain_input();
        return Ok(false);
    };

    // give the terminal back: the probe only asks whether it speaks TSP
    session.close().context("close the probe session")?;
    Ok(true)
}

/// Starts the session on its own thread and returns.
///
/// The session owns the terminal and blocks in its input, so `main` keeps the main thread free for
/// the OS media-control event loop.
fn run(ctx: &FrontendContext) -> Result<()> {
    let state = ctx.state.clone();
    let client_tx = ctx.client_tx.clone();

    std::thread::Builder::new()
        .name("tsp".to_string())
        .spawn(move || {
            if let Err(err) = session(&state, &client_tx) {
                tracing::error!("The TSP frontend stopped: {err:#}");
                std::process::exit(1);
            }
        })
        .context("spawn the TSP thread")?;

    Ok(())
}

/// The state one session keeps between frames.
struct SessionState {
    /// The TUI state this frontend drives.
    ui_state: UIState,
    /// The cover art blobs the session sent.
    blobs: blobs::Blobs,
    /// What the document needs remembered between frames.
    frontend: view::FrontendState,
}

/// Runs one session until the application quits.
fn session(state: &SharedState, client_tx: &flume::Sender<ClientRequest>) -> Result<()> {
    let Some(mut session) = connect()? else {
        bail!("this terminal does not speak TSP");
    };

    let caps = session.caps().clone();
    let surface = session
        .open(
            SurfaceOptions::screen()
                .id("sp")
                .title("spotify-player")
                .role("spotify-player.session"),
        )
        .context("open the surface")?;

    let mut session_state = SessionState {
        ui_state: UIState::new(),
        blobs: blobs::Blobs::default(),
        frontend: view::FrontendState::default(),
    };

    // the palette first, then the sheet, then the document (the protocol's order)
    session
        .palette(surface, palette::palette(&session_state.ui_state.theme))
        .context("send the palette")?;
    let mut palette_name = session_state.ui_state.theme.name.clone();
    if caps.has_feature(feature::STYLES) {
        session
            .stylesheet(surface, "app", Some(SHEET))
            .context("send the stylesheet")?;
    }

    while turn(&mut session, surface, &mut session_state, state, client_tx)? {
        // the theme list popup changes the theme in place: tell Tern about the new colours
        if session_state.ui_state.theme.name != palette_name {
            palette_name.clone_from(&session_state.ui_state.theme.name);
            session
                .palette(surface, palette::palette(&session_state.ui_state.theme))
                .context("send the palette")?;
        }
    }

    session
        .close_surface(surface, false)
        .context("close the surface")?;
    session.close().context("close the session")?;

    std::process::exit(0);
}

/// Draws the current state and handles the next input: one turn of the session loop.
///
/// Returns whether the application is still running. Input wakes the read immediately; the timeout
/// is when the view is drawn again, which is what shows a change a background task made.
fn turn(
    session: &mut Session<Msg>,
    surface: Surface,
    session_state: &mut SessionState,
    state: &SharedState,
    client_tx: &flume::Sender<ClientRequest>,
) -> Result<bool> {
    let refresh = Duration::from_millis(config::get_config().app_config.app_refresh_duration_in_ms);

    let (view, ops) = view::build(
        session,
        &mut session_state.blobs,
        state,
        &mut session_state.frontend,
    );
    session.render(surface, view).context("render a frame")?;
    if !ops.is_empty() {
        session.send(surface, ops).context("send frame ops")?;
    }

    // Input wakes this immediately; the timeout is when the view is re-rendered.
    //
    // Unlike the TUI, this frontend cannot await the core redraw signal: `Session::next` blocks
    // in a read on the session's pty with no cross-thread waker (tern-sdk `session.rs::next` ->
    // `read`, both `&mut self`), so a signal raised by a background task could not interrupt it.
    // Polling at `app_refresh_duration_in_ms` is therefore still how a change made elsewhere
    // reaches the surface.
    match session.next(Some(refresh)).context("read the terminal")? {
        Some(Input::Key(key)) => {
            keys::handle(&key, state, &mut session_state.ui_state, client_tx)?;
        }
        Some(Input::Msg(msg, event)) => {
            if let Err(err) = handle_msg(msg, state, &mut session_state.ui_state, client_tx, &event)
            {
                tracing::warn!("Failed to handle a Tern event: {err:#}");
            }
        }
        Some(Input::Event(event)) => tracing::debug!("Unhandled Tern event: {event:?}"),
        None => {}
    }

    Ok(state.view.lock().is_running)
}

/// Opens a session, or `None` when the terminal does not speak TSP.
fn connect() -> Result<Option<Session<Msg>>> {
    let options = Options::new()
        .app("spotify-player")
        .version(env!("CARGO_PKG_VERSION"));

    Session::connect(options).map_err(anyhow::Error::from)
}

/// Reads whatever a failed handshake left in the terminal (its DA1 reply), so the frontend that
/// starts next sees a clean input queue.
fn drain_input() {
    use tern_sdk::term::{Terminal, Tty};

    let Ok(mut tty) = Tty::open() else {
        // not a terminal (or it is already owned): nothing to drain
        return;
    };

    let mut buf = [0u8; 256];
    loop {
        match tty.read(&mut buf, Some(Duration::from_millis(20))) {
            Ok(Some(n)) if n > 0 => {}
            // nothing left, or the input closed
            Ok(_) => break,
            Err(err) => {
                tracing::debug!("Failed to drain the terminal: {err}");
                break;
            }
        }
    }

    if let Err(err) = tty.restore() {
        tracing::debug!("Failed to restore the terminal: {err}");
    }
}

/// Runs what a Tern event asked for.
fn handle_msg(
    msg: Msg,
    state: &SharedState,
    ui_state: &mut UIState,
    client_tx: &flume::Sender<ClientRequest>,
    event: &Event,
) -> Result<()> {
    let mut ui = Ctx::new(state.view.lock(), ui_state);

    match msg {
        Msg::Select { target, index } => {
            view::select(&mut ui, target, index);
            Ok(())
        }
        Msg::Activate { target, index } => view::activate(&mut ui, target, index, client_tx, state),
        Msg::Act(name) => act(&name, &mut ui, client_tx, state, event),
        Msg::Choice { name, value } => {
            view::choice(&mut ui, &name, &value);
            Ok(())
        }
    }
}

/// Runs one of the `actions` names this frontend puts on its nodes.
fn act(
    name: &str,
    ui: &mut Ctx<'_>,
    client_tx: &flume::Sender<ClientRequest>,
    state: &SharedState,
    event: &Event,
) -> Result<()> {
    // an action name is `name`, or `name=value` when it carries one (`"seek=0.4"`)
    let (name, value) = name
        .split_once('=')
        .map_or((name, None), |(name, value)| (name, Some(value)));

    let found = match name {
        "play-pause" => CommandOrAction::Command(Command::ResumePause),
        "next" => CommandOrAction::Command(Command::NextTrack),
        "prev" => CommandOrAction::Command(Command::PreviousTrack),
        "shuffle" => CommandOrAction::Command(Command::Shuffle),
        "repeat" => CommandOrAction::Command(Command::Repeat),
        "mute" => CommandOrAction::Command(Command::Mute),
        "devices" => CommandOrAction::Command(Command::SwitchDevice),
        "themes" => CommandOrAction::Command(Command::SwitchTheme),
        "like" => CommandOrAction::Action(Action::ToggleLiked, ActionTarget::PlayingTrack),
        "seek" => {
            return seek(value, client_tx, state);
        }
        "cmd" => {
            let Some(command) =
                value.and_then(|name| serde_json::from_value::<Command>(name.into()).ok())
            else {
                tracing::warn!("Unknown command in the TSP action {value:?}");
                return Ok(());
            };
            CommandOrAction::Command(command)
        }
        _ => {
            tracing::debug!("Unhandled TSP action {name:?} ({event:?})");
            return Ok(());
        }
    };

    dispatch::handle_command_or_action(&found, client_tx, state, ui)?;
    Ok(())
}

/// Seeks to `fraction` of the playing item.
///
/// Tern reports no click position, so the progress bar offers fixed positions instead of a
/// position-precise click.
fn seek(
    fraction: Option<&str>,
    client_tx: &flume::Sender<ClientRequest>,
    state: &SharedState,
) -> Result<()> {
    let Some(fraction) = fraction.and_then(|value| value.parse::<f64>().ok()) else {
        tracing::warn!("The seek action needs a fraction");
        return Ok(());
    };

    let player = state.player.read();
    let Some(item) = player.playback.as_ref().and_then(|p| p.item.as_ref()) else {
        return Ok(());
    };
    let duration = match item {
        rspotify::model::PlayableItem::Track(track) => track.duration,
        rspotify::model::PlayableItem::Episode(episode) => episode.duration,
        rspotify::model::PlayableItem::Unknown(_) => return Ok(()),
    };

    let position = chrono::TimeDelta::milliseconds(
        (duration.num_milliseconds() as f64 * fraction.clamp(0.0, 1.0)) as i64,
    );
    client_tx.send(ClientRequest::Player(PlayerRequest::SeekTrack(position)))?;

    Ok(())
}
