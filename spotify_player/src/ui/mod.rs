use crate::{
    config,
    state::{
        Album, Artist, ArtistFocusState, BrowsePageUIState, Context, ContextPageUIState, Cursor,
        DataReadGuard, Id, LibraryFocusState, MutableWindowState, PageState, PageType,
        PlaylistCreateCurrentField, PlaylistFolderItem, PlaylistPopupAction, PopupState,
        SearchFocusState, SharedState, ShowFocusState, Track,
    },
};

pub use self::{
    ctx::Ctx,
    state::{ProgressBarLayout, SharedUiState, UIState},
};
use anyhow::{Context as AnyhowContext, Result};
use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, BorderType, Borders, Cell, Gauge, LineGauge, List, ListItem, ListState, Paragraph,
        Row, Table, TableState, Wrap,
    },
    Frame,
};
use std::time::{Duration, Instant};

type Terminal = ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>;

mod ctx;
#[cfg(feature = "image")]
pub mod image_compositor;
mod page;
mod playback;
mod popup;
pub mod single_line_input;
mod state;
#[cfg(feature = "streaming")]
pub mod streaming;
pub mod utils;

/// Longest the loop waits with nothing due.
///
/// A raise that never arrives — a producer path missed, a frontend-side mutation with no owner —
/// must not freeze the UI forever, so every wait is bounded by this. One frame per interval is
/// 0.0x% of a core, negligible next to the frames the loop no longer draws.
const MAX_IDLE_WAIT: Duration = Duration::from_secs(10);

/// Elapsed-time refresh while a track plays: the progress bar, the lyrics highlight and the
/// playback clock are computed from a stored position plus wall-clock time, so they need a tick
/// even when nothing changes state.
const PLAYBACK_REFRESH: Duration = Duration::from_secs(1);

/// Refresh while the first playback fetch is still outstanding, matching the 100ms step the
/// "Loading..." spinner's glyphs advance on.
const SPINNER_REFRESH: Duration = Duration::from_millis(100);

/// How long to wait after a failed draw before trying again. A terminal that is momentarily gone
/// (a detached multiplexer, a closed pty) fails every draw; retrying hot would spin a core.
const DRAW_FAILURE_BACKOFF: Duration = Duration::from_millis(500);

/// Run the application UI.
///
/// The loop draws when it has a reason to: a `RedrawNotify` raise from anything that changed what
/// is on screen, or one of the few views that legitimately move on their own
/// ([`PeriodicRefresh`], plus the cover-art compositor's scheduled re-transmissions). Between
/// those it blocks, holding no `ui_state`/`view` guard.
pub fn run(state: &SharedState, ui_state: &SharedUiState, mut terminal: Terminal) -> Result<()> {
    let refresh = Duration::from_millis(config::get_config().app_config.app_refresh_duration_in_ms);

    let mut periodic = PeriodicRefresh::default();
    let mut retry_at = Instant::now();

    loop {
        let now = Instant::now();

        // Draw immediately when the last draw failed and its backoff has run out; otherwise wait
        // for a raise or the earliest deadline this frame owes.
        let deadline = if now < retry_at {
            retry_at
        } else {
            let mut deadline = now + MAX_IDLE_WAIT;
            if let Some(scheduled) = periodic.deadline(state, now, refresh) {
                deadline = deadline.min(scheduled);
            }
            if let Some(cover) = cover_deadline(ui_state, now) {
                deadline = deadline.min(cover);
            }
            deadline
        };

        let signalled = state
            .redraw
            .wait_timeout(deadline.saturating_duration_since(now));
        if !signalled && Instant::now() < deadline {
            // Nothing changed and no deadline is due yet: keep waiting.
            continue;
        }

        let drawn_at = Instant::now();
        // Ignore raises from this thread's own render path: a draw that logs must be retried on
        // the policy below, not on the log line it just wrote.
        let frame = state.redraw.frame();

        let result = {
            let mut tui = ui_state.lock();
            let mut ui = Ctx::new(state.view.lock(), &mut tui);

            if !ui.is_running {
                // Every path that clears `is_running` (the quit key, the CLI, media control)
                // raises, so the loop is awake here rather than parked.
                clean_up(terminal).context("clean up UI resources")?;
                std::process::exit(0);
            }

            #[cfg(feature = "image")]
            // Frame-skip detection: surfaces idle for a pass are healed on their next render.
            ui.tui.image_compositor.begin_frame();

            terminal.draw(|frame| {
                // set the background and foreground colors for the application
                let rect = frame.area();
                let block = Block::default().style(ui.tui.theme.app());
                frame.render_widget(block, rect);

                render_application(frame, state, &mut ui, rect);
            })
        };
        drop(frame);

        match result {
            Ok(_) => {
                periodic.serviced(drawn_at);
                retry_at = drawn_at;
            }
            Err(err) => {
                tracing::error!("Failed to render the application: {err:#}");
                retry_at = Instant::now() + DRAW_FAILURE_BACKOFF;
            }
        }
    }
}

/// The views whose content changes with the clock rather than with a state change, each with the
/// frame it was last drawn in.
///
/// Their intervals are the display cadence: the visualizer interpolates its bars from the time
/// since the audio sink's last write, so the frame rate *is* part of the picture; the progress
/// bar reads a position that advances in real time.
#[derive(Default)]
struct PeriodicRefresh {
    visualizer: Option<Instant>,
    playback: Option<Instant>,
    spinner: Option<Instant>,
}

impl PeriodicRefresh {
    /// When the next of these is due, arming or clearing each against `state`.
    ///
    /// A view that stopped moving clears its timer, so an idle application wakes for none of them.
    fn deadline(
        &mut self,
        state: &SharedState,
        now: Instant,
        refresh: Duration,
    ) -> Option<Instant> {
        let mut earliest: Option<Instant> = None;
        let mut arm = |timer: &mut Option<Instant>, running: bool, interval: Duration| {
            if !running {
                *timer = None;
                return;
            }
            let at = timer.unwrap_or(now) + interval;
            earliest = Some(earliest.map_or(at, |e| e.min(at)));
        };

        #[cfg(feature = "streaming")]
        let visualizer = state.is_local_streaming_active();
        #[cfg(not(feature = "streaming"))]
        let visualizer = false;
        arm(&mut self.visualizer, visualizer, refresh);

        let (playing, awaiting_first_playback) = {
            let player = state.player.read();
            (
                player.playback.as_ref().is_some_and(|p| p.is_playing),
                player.playback_last_updated_time.is_none(),
            )
        };
        arm(&mut self.playback, playing, PLAYBACK_REFRESH);
        arm(&mut self.spinner, awaiting_first_playback, SPINNER_REFRESH);

        earliest
    }

    /// Record a frame drawn at `now`, moving every running timer on.
    fn serviced(&mut self, now: Instant) {
        for timer in [&mut self.visualizer, &mut self.playback, &mut self.spinner] {
            if timer.is_some() {
                *timer = Some(now);
            }
        }
    }
}

/// The cover-art compositor's next scheduled re-render, if the `image` feature is built in.
#[cfg(feature = "image")]
fn cover_deadline(ui_state: &SharedUiState, now: Instant) -> Option<Instant> {
    ui_state.lock().image_compositor.next_deadline(now)
}

#[cfg(not(feature = "image"))]
fn cover_deadline(_ui_state: &SharedUiState, _now: Instant) -> Option<Instant> {
    None
}

pub fn init_terminal() -> Result<Terminal> {
    let mut stdout = std::io::stdout();
    crossterm::terminal::enable_raw_mode()?;
    crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableMouseCapture
    )?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend)?;
    terminal.clear()?;
    Ok(terminal)
}

#[cfg(feature = "image")]
pub fn init_image_picker(ui_state: &SharedUiState) -> Result<()> {
    let mut ui = ui_state.lock();
    crossterm::terminal::enable_raw_mode()?;

    // `Picker::from_query_stdio` reads the terminal's answer on a detached thread. Inside a
    // terminal multiplexer the query is wrapped in a DCS passthrough that only a capable outer
    // terminal ever sees, so when there is none the query stays unanswered: the picker gives up
    // at its timeout, but that thread stays blocked on stdin. Left there it swallows the reply
    // to the cursor query `init_terminal` waits for -- the startup failure under `tmux` -- and
    // then the user's keystrokes, so unblock it before the terminal gets set up.
    let options = ratatui_image::picker::cap_parser::QueryStdioOptions::default();
    let query_timeout = options.timeout;
    let query_started = std::time::Instant::now();
    ui.picker = match ratatui_image::picker::Picker::from_query_stdio_with_options(options) {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!("Failed to initialize query_stdio picker, error: {err:#}");
            ratatui_image::picker::Picker::halfblocks()
        }
    };
    if query_started.elapsed() >= query_timeout {
        unblock_picker_reader();
    }

    // The picker restores the terminal mode when its thread ends, which can happen after
    // `init_terminal` has enabled raw mode; claiming the stored mode here makes that restore a
    // no-op.
    crossterm::terminal::disable_raw_mode()?;

    // ratatui_image might detect the wrong protocol for iTerm2, so override it to the native iTerm2 protocol if detected
    // https://github.com/ratatui/ratatui-image/issues/158
    if is_iterm2() && ui.picker.protocol_type() != ratatui_image::picker::ProtocolType::Iterm2 {
        ui.picker
            .set_protocol_type(ratatui_image::picker::ProtocolType::Iterm2);
        tracing::info!("Detected iTerm2; overriding image protocol to native iTerm2");
    }
    tracing::info!("Image protocol: {:?}", ui.picker.protocol_type());
    Ok(())
}

/// Sends one query that every terminal answers, so a reply-reading thread the picker may have
/// left blocked on stdin reads something and exits. See [`init_image_picker`].
#[cfg(feature = "image")]
fn unblock_picker_reader() {
    use std::io::Write;

    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(b"\x1b[6n");
    let _ = stdout.flush();
    std::thread::sleep(std::time::Duration::from_millis(100));
}

/// Whether the application is running inside iTerm2.
///
/// iTerm2 sets `TERM_PROGRAM=iTerm.app` locally and forwards `LC_TERMINAL=iTerm2`
/// over SSH, so checking both covers the common cases.
#[cfg(feature = "image")]
fn is_iterm2() -> bool {
    std::env::var("TERM_PROGRAM").is_ok_and(|v| v == "iTerm.app")
        || std::env::var("LC_TERMINAL").is_ok_and(|v| v.eq_ignore_ascii_case("iTerm2"))
}

/// Clean up UI resources before quitting the application
fn clean_up(mut terminal: Terminal) -> Result<()> {
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::event::DisableMouseCapture,
    )?;
    terminal.show_cursor()?;
    Ok(())
}

/// Render the application
fn render_application(frame: &mut Frame, state: &SharedState, ui: &mut Ctx<'_>, rect: Rect) {
    // rendering order: playback window -> shortcut help popup -> other popups -> main layout

    // render playback window before other popups and windows to ensure nothing is rendered on top
    // of the playback window, which is to avoid "duplicated images" issue
    // See: https://github.com/aome510/spotify-player/issues/498
    let rect = playback::render_playback_window(frame, state, ui, rect);

    let rect = popup::render_shortcut_help_popup(frame, ui, rect);

    let (rect, is_active) = popup::render_popup(frame, state, ui, rect);

    render_main_layout(is_active, frame, state, ui, rect);
}

/// Render the application's main layout
fn render_main_layout(
    is_active: bool,
    frame: &mut Frame,
    state: &SharedState,
    ui: &mut Ctx<'_>,
    rect: Rect,
) {
    let page_type = ui.current_page().page_type();
    match page_type {
        PageType::Library => page::render_library_page(is_active, frame, state, ui, rect),
        PageType::Search => page::render_search_page(is_active, frame, state, ui, rect),
        PageType::Context => page::render_context_page(is_active, frame, state, ui, rect),
        PageType::Browse => page::render_browse_page(is_active, frame, state, ui, rect),
        PageType::Lyrics => page::render_lyrics_page(is_active, frame, state, ui, rect),
        PageType::Queue => page::render_queue_page(frame, state, ui, rect),
        PageType::CommandHelp => page::render_commands_help_page(frame, ui, rect),
        PageType::Logs => page::render_logs_page(frame, state, ui, rect),
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Orientation {
    Vertical,
    #[default]
    Horizontal,
}

impl Orientation {
    /// Construct screen orientation based on the terminal's size
    pub fn from_size(columns: u16, rows: u16) -> Self {
        let ratio = f64::from(columns) / f64::from(rows);

        // a larger ratio has to be used since terminal cells aren't square
        if ratio > 2.3 {
            Self::Horizontal
        } else {
            Self::Vertical
        }
    }

    pub fn layout<I>(self, constraints: I) -> Layout
    where
        I: IntoIterator,
        I::Item: Into<Constraint>,
    {
        match self {
            Self::Vertical => Layout::vertical(constraints),
            Self::Horizontal => Layout::horizontal(constraints),
        }
    }
}
