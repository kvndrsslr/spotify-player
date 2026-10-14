//! Which rendering frontend runs this session, and how one is started.
//!
//! The application ships several ways to draw the same shared state; exactly one runs per process.
//! [`resolve`] validates what the user asked for, [`select`] turns it into a frontend this build
//! and this terminal can run, and [`start`] starts it.

use anyhow::{anyhow, bail, Context, Result};
use flume::Sender;
use std::sync::Once;

use spotify_player_core::{client::ClientRequest, state::SharedState};

mod tui;

#[cfg(feature = "tsp")]
mod tsp;

/// A rendering frontend a user can ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrontendKind {
    /// Pick automatically: the TSP frontend in a Tern terminal, the TUI anywhere else.
    Auto,
    /// The ratatui TUI.
    Tui,
    /// The Tern Surface Protocol frontend.
    Tsp,
}

impl FrontendKind {
    /// Every value, as `--frontend` and the `frontend` config option spell it.
    pub const NAMES: [&'static str; 3] = ["auto", "tui", "tsp"];

    /// The name [`FrontendKind::NAMES`] lists for this value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Tui => "tui",
            Self::Tsp => "tsp",
        }
    }

    /// Parses a name as [`FrontendKind::NAMES`] spells it.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "auto" => Some(Self::Auto),
            "tui" => Some(Self::Tui),
            "tsp" => Some(Self::Tsp),
            _ => None,
        }
    }
}

/// What a frontend is handed: the shared application state and the sender core's client task reads.
pub struct FrontendContext {
    /// The application's shared state.
    pub state: SharedState,
    /// How the frontend asks core to talk to Spotify or to control playback.
    pub client_tx: Sender<ClientRequest>,
}

/// A way to run the application in the current terminal.
pub struct Frontend {
    /// The name `--frontend` and the `frontend` config option spell, one of [`FrontendKind::NAMES`].
    pub name: &'static str,
    /// Whether this frontend can take over the terminal right now.
    ///
    /// Must leave the terminal exactly as found when it answers `false` (and on failure), so
    /// another frontend can take it.
    pub probe: fn() -> Result<bool>,
    /// Starts the frontend's work on its own threads and returns once it is running; the process
    /// stays alive through the caller's keep-alive loop.
    pub run: fn(&FrontendContext) -> Result<()>,
}

/// Every frontend this build has, in the order `auto` prefers them.
static FRONTENDS: &[&Frontend] = &[
    #[cfg(feature = "tsp")]
    &tsp::FRONTEND,
    &tui::FRONTEND,
];

/// Resolves which frontend the user asked for: `--frontend` wins over `app.toml`'s `frontend`
/// option.
///
/// Does no terminal or network I/O, so callers can validate the request before anything starts. It
/// reads the environment to report what `auto` will do, including the case that would otherwise stay
/// invisible: a Tern terminal and a build without the `tsp` frontend. Both reports happen before
/// authentication, so they reach the log even when the run then blocks at the first-run login.
///
/// # Errors
///
/// An unknown `--frontend` value, or a frontend this build does not have.
pub fn resolve(cli: Option<&str>, configured: &str) -> Result<FrontendKind> {
    let requested = match cli {
        Some(name) => FrontendKind::from_name(name).ok_or_else(|| {
            anyhow!(
                "unknown frontend {name:?}; known values: {}",
                FrontendKind::NAMES.join(", ")
            )
        })?,
        None => FrontendKind::from_name(configured).unwrap_or_else(|| {
            tracing::warn!(
                "Unknown `frontend` value {configured:?}; using `auto`. Known values: {}",
                FrontendKind::NAMES.join(", ")
            );
            FrontendKind::Auto
        }),
    };

    // Name the value that won, so a frontend choice is never silent: `--frontend` beats `app.toml`,
    // and an absent or unknown `app.toml` value means `auto`.
    match cli {
        Some(name) => tracing::info!(
            "Frontend: --frontend {name:?} wins over app.toml's frontend = {configured:?}; resolved to {}",
            requested.as_str()
        ),
        None if FrontendKind::from_name(configured).is_some() => tracing::info!(
            "Frontend: app.toml's frontend = {configured:?} selected; resolved to {}",
            requested.as_str()
        ),
        None => tracing::info!(
            "Frontend: no --frontend and no known app.toml frontend; resolved to {}",
            requested.as_str()
        ),
    }

    // Report what `auto` will do *now*, because this runs before authentication: a run that blocks
    // at the first-run login never reaches `select`, and this is the only chance to explain the
    // fallback. The warning itself is emitted once per process (see [`warn_no_tsp`]).
    if requested == FrontendKind::Auto && tern_environment_hint() {
        let term = term_program().unwrap_or_else(|| "<unset>".to_owned());
        if find(FrontendKind::Tsp).is_none() {
            warn_no_tsp(&term);
        } else {
            tracing::info!(
                "Frontend: Tern detected (TERM_PROGRAM={term}); TSP will be probed after startup"
            );
        }
    }

    match requested {
        FrontendKind::Auto | FrontendKind::Tui => Ok(requested),
        FrontendKind::Tsp => {
            if find(FrontendKind::Tsp).is_none() {
                bail!("TSP support is not compiled in this build (rebuild with the `tsp` feature)");
            }
            Ok(requested)
        }
    }
}

/// The frontend to run, out of the frontends this build has.
///
/// `tui` runs without touching the terminal. `tsp` probes it: an explicit request that the terminal
/// cannot serve is an error. `auto` uses the environment: in a Tern terminal it probes TSP and
/// falls back to the TUI (with a warning) when the probe declines.
///
/// Must be called before any frontend takes over the terminal, and after anything that needs the
/// terminal to itself (such as the first-run authentication prompts).
///
/// # Errors
///
/// A frontend this build lacks, or an explicit `tsp` request this terminal cannot serve.
pub fn select(requested: FrontendKind) -> Result<&'static Frontend> {
    match requested {
        FrontendKind::Tui => {
            tracing::info!("Frontend: `tui` was requested; running the TUI");
            tui()
        }
        FrontendKind::Tsp => {
            let Some(tsp) = find(FrontendKind::Tsp) else {
                bail!("TSP support is not compiled in this build (rebuild with the `tsp` feature)");
            };
            if (tsp.probe)()? {
                tracing::info!(
                    "Frontend: `tsp` was requested and this terminal answered the TSP handshake; running the TSP frontend"
                );
                Ok(tsp)
            } else {
                bail!(
                    "this terminal does not speak TSP; run with `--frontend auto` to let the TUI run here"
                );
            }
        }
        FrontendKind::Auto => {
            if tern_environment_hint() {
                tracing::info!(
                    "Frontend: Tern detected (TERM_PROGRAM={}); probing with the TSP handshake",
                    term_program().as_deref().unwrap_or("<unset>")
                );
                if let Some(tsp) = probe_tsp_for_auto() {
                    tracing::info!(
                        "Frontend: the TSP handshake answered; running the TSP frontend"
                    );
                    return Ok(tsp);
                }
                tracing::info!(
                    "Frontend: the TSP frontend was not selected; running the TUI instead"
                );
                return tui();
            }
            tracing::info!(
                "Frontend: Tern was not detected; running the TUI (the debug log names the condition that failed)"
            );
            tui()
        }
    }
}

/// Starts `frontend` with `ctx`.
///
/// # Errors
///
/// Whatever the frontend fails with while starting.
pub fn start(frontend: &Frontend, ctx: &FrontendContext) -> Result<()> {
    (frontend.run)(ctx).with_context(|| format!("start the {} frontend", frontend.name))
}

/// `TERM_PROGRAM`'s value, when the environment sets a non-empty one.
fn term_program() -> Option<String> {
    std::env::var("TERM_PROGRAM")
        .ok()
        .filter(|value| !value.is_empty())
}

/// Whether the "Tern detected, but this build has no TSP frontend" warning has already been logged.
static WARNED_NO_TSP: Once = Once::new();

/// Warns, once per process, that Tern was detected but this build cannot serve TSP.
///
/// Both [`resolve`] (before authentication, where a run blocked at the first-run login can still see
/// it) and [`probe_tsp_for_auto`] (after it) know this condition; `Once` keeps it to a single
/// warning, from whichever ran first.
fn warn_no_tsp(term: &str) {
    WARNED_NO_TSP.call_once(|| {
        tracing::warn!(
            "Tern detected (TERM_PROGRAM={term}), but TSP support is not compiled in this build (rebuild with the `tsp` feature); using the TUI"
        );
    });
}

/// Probes the TSP frontend for `auto`: `None` means "use the TUI instead", with the reason logged.
fn probe_tsp_for_auto() -> Option<&'static Frontend> {
    let term = term_program().unwrap_or_else(|| "<unset>".to_owned());
    let Some(tsp) = find(FrontendKind::Tsp) else {
        // `resolve` reports this before authentication; `Once` keeps it to one warning per run.
        warn_no_tsp(&term);
        return None;
    };

    match (tsp.probe)() {
        Ok(true) => Some(tsp),
        Ok(false) => {
            tracing::warn!(
                "Tern detected (TERM_PROGRAM={term}), but the TSP probe got no reply to its `hello` handshake; using the TUI"
            );
            None
        }
        Err(err) => {
            tracing::warn!(
                "Tern detected (TERM_PROGRAM={term}), but the TSP probe failed: {err:#}; using the TUI"
            );
            None
        }
    }
}

fn find(kind: FrontendKind) -> Option<&'static Frontend> {
    FRONTENDS
        .iter()
        .find(|frontend| frontend.name == kind.as_str())
        .copied()
}

fn tui() -> Result<&'static Frontend> {
    find(FrontendKind::Tui).ok_or_else(|| anyhow!("the TUI frontend is missing from this build"))
}

/// Whether the environment says the terminal speaks TSP, without touching the terminal.
///
/// Mirrors the cheap checks `tern_sdk`'s own detection makes; the real detection is its `hello`
/// handshake, which [`Frontend::probe`] runs.
fn tern_environment_hint() -> bool {
    let set = |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());

    if std::env::var_os("TERN_TSP").is_some_and(|value| value == "0") {
        tracing::debug!("Frontend: no Tern hint: TERN_TSP=0 disables TSP");
        return false;
    }
    // these multiplexers swallow the APC strings TSP travels in
    for name in ["TMUX", "STY", "ZELLIJ"] {
        if set(name) {
            tracing::debug!("Frontend: no Tern hint: {name} is set, and it swallows TSP");
            return false;
        }
    }
    match std::env::var("TERM_PROGRAM") {
        Ok(program) if program == "tern" => true,
        Ok(program) => {
            tracing::debug!("Frontend: no Tern hint: TERM_PROGRAM={program:?}, expected \"tern\"");
            false
        }
        Err(_) => {
            tracing::debug!("Frontend: no Tern hint: TERM_PROGRAM is not set, expected \"tern\"");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_line_value_wins_over_the_configured_one() {
        assert_eq!(
            resolve(Some("tui"), "tsp").expect("known frontend"),
            FrontendKind::Tui
        );
        assert_eq!(
            resolve(Some("auto"), "tui").expect("known frontend"),
            FrontendKind::Auto
        );
        assert_eq!(
            resolve(None, "tui").expect("configured frontend"),
            FrontendKind::Tui
        );
        assert_eq!(
            resolve(None, "auto").expect("configured frontend"),
            FrontendKind::Auto
        );
    }

    #[test]
    fn an_unknown_command_line_value_lists_the_known_ones() {
        let err = resolve(Some("tern"), "tui").expect_err("unknown frontend");
        let message = format!("{err:#}");

        assert!(message.contains("tern"), "{message}");
        for name in FrontendKind::NAMES {
            assert!(message.contains(name), "{message} must list {name}");
        }
    }

    #[test]
    fn an_unknown_configured_value_falls_back_to_auto() {
        assert_eq!(
            resolve(None, "tern").expect("a bad config value is not fatal"),
            FrontendKind::Auto
        );
        assert_eq!(
            resolve(None, "").expect("a bad config value is not fatal"),
            FrontendKind::Auto
        );
    }

    #[test]
    fn a_frontend_this_build_lacks_is_only_an_error_for_that_frontend() {
        for kind in [FrontendKind::Auto, FrontendKind::Tui] {
            assert_eq!(
                resolve(None, kind.as_str()).expect("always resolvable"),
                kind
            );
        }

        match resolve(Some("tsp"), "auto") {
            Ok(kind) => {
                assert_eq!(kind, FrontendKind::Tsp);
                assert!(
                    find(kind).is_some(),
                    "resolving `tsp` means the build has it"
                );
            }
            Err(err) => {
                let message = format!("{err:#}");
                assert!(message.contains("not compiled in"), "{message}");
                assert!(
                    find(FrontendKind::Tsp).is_none(),
                    "only without the `tsp` frontend"
                );

                // the configured value goes through the same check, but `--frontend` wins first
                assert!(resolve(None, "tsp").is_err());
                assert_eq!(
                    resolve(Some("tui"), "tsp").expect("the command line wins"),
                    FrontendKind::Tui
                );
            }
        }
    }

    #[test]
    fn names_and_kinds_agree() {
        for (kind, name) in [
            (FrontendKind::Auto, "auto"),
            (FrontendKind::Tui, "tui"),
            (FrontendKind::Tsp, "tsp"),
        ] {
            assert_eq!(kind.as_str(), name);
            assert_eq!(FrontendKind::from_name(name), Some(kind));
        }
        assert_eq!(FrontendKind::from_name("tern"), None);
        assert_eq!(FrontendKind::from_name("AUTO"), None);
    }

    #[test]
    fn every_frontend_in_the_table_is_a_known_kind() {
        assert!(!FRONTENDS.is_empty());

        for frontend in FRONTENDS {
            assert!(
                FrontendKind::NAMES.contains(&frontend.name),
                "{} is not a known frontend name",
                frontend.name
            );
        }
    }

    #[test]
    fn the_tui_can_always_run() {
        let tui = select(FrontendKind::Tui).expect("the TUI is always available");

        assert_eq!(tui.name, "tui");
        assert!((tui.probe)().expect("the TUI probe never fails"));
    }
}
