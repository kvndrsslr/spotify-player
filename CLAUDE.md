# CLAUDE.md

This file guides Claude Code when working in this repository.

## Project Overview

`spotify-player` is a terminal Spotify client (requires Spotify Premium) written in Rust. It supports playback control, Spotify Connect, direct streaming via librespot, synced lyrics, desktop notifications, OS media controls, album art rendering, a daemon mode, and a full CLI interface.

## Architecture

The workspace has two crates: the **core** library and the **TUI binary**. Core owns the data
model, state, playback, Spotify client, configuration (`app.toml`), the key vocabulary and the
user's keybindings (`keymap.toml`), command execution, and must not depend on `ratatui`,
`crossterm` or `ratatui-image`. The binary owns rendering, terminal lifecycle, reading the
terminal's events, and the frontend theme configuration (`theme.toml`).

### Core crate — `crates/core/src/` (package `spotify_player_core`)

| Module                        | Responsibility                                                              |
| ----------------------------- | --------------------------------------------------------------------------- |
| `lib.rs`                      | Boundary documentation and the public module tree                           |
| `state/`                      | Shared app state (`Arc<State>`): view model, player data, library caches, queue |
| `state/model.rs`              | Core domain types: `Track`, `Album`, `Artist`, `Playlist`, `Playback`, etc. |
| `state/player.rs`             | `PlayerState`: current playback, devices, queue, progress estimation        |
| `state/data.rs`               | `AppData`: user library, TTL memory caches, file-cache persistence          |
| `state/view/`                 | `ViewState`: framework-agnostic page history, popups, cursors, text input   |
| `controller.rs`               | Command executor: `Command`/`Action` → `ClientRequest` + view-state updates |
| `client/mod.rs`               | `AppClient`: Spotify API calls, session management                          |
| `client/request.rs`           | `ClientRequest` / `PlayerRequest` enums (async message types)               |
| `client/handlers.rs`          | Tokio task: receives `ClientRequest`, dispatches API calls                  |
| `client/rate_limit.rs`        | Web API request pacing and the client-wide rate-limit cooldown              |
| `config.rs`                   | `Configs` (app config + keybindings + folders) loaded once; read via `config::get_config()` |
| `key.rs`                      | Framework-agnostic `Key` / `KeyCode` / `KeySequence`; vim-style key parsing (`C-x`, `M-x`)  |
| `keymap.rs`                   | `KeymapConfig`: default keybindings, `keymap.toml` merging, key-sequence → command lookup   |
| `command.rs`                  | `Command` enum (all TUI commands), `Action` / `CommandOrAction`             |
| `playback.rs`                 | `playback_format` → `PlaybackSpan`/`PlaybackToken` (the playback bar's text) |
| `runtime.rs`                  | Frontend-agnostic startup: `init_logging`, `start` (client + background tasks) |
| `clipboard.rs`                | OS clipboard read/copy                                                      |
| `visualization.rs`            | FFT audio bands (`VisBands`, `VisualizationSink`) shared with frontends     |
| `streaming.rs`                | librespot connection + audio backend setup (feature-gated)                  |
| `auth.rs`                     | OAuth scopes and librespot credential/session building                      |
| `media_control.rs`            | OS media key integration via `souvlaki` (feature-gated)                     |
| `playlist_folders.rs`, `token.rs`, `log_layer.rs`, `utils.rs` | Playlist folder tree, token persistence, tracing layer, helpers |

### TUI crate — `spotify_player/src/` (binary `spotify_player`)

Core modules are re-exported at the binary root, so `crate::state::…`, `crate::client::…` and
friends keep resolving; the frontend crate only adds the modules below.

| Module                        | Responsibility                                                              |
| ----------------------------- | --------------------------------------------------------------------------- |
| `main.rs`                     | CLI arg parsing, config/daemon init, `runtime::start`, frontend selection, the CLI socket task, the keep-alive/winit block |
| `frontend/mod.rs`             | Frontend selection (`--frontend` / `app.toml`'s `frontend`) and the `Frontend` table          |
| `frontend/tui.rs`             | The ratatui TUI frontend: terminal setup + its event and UI threads                          |
| `frontend/tsp/`               | The Tern Surface Protocol frontend (`tsp` feature): session loop, keys, document, blobs, palette |
| `frontend_config.rs`          | Frontend config store: the theme, loaded from the core's config folder      |
| `theme.rs`                    | Theme definitions and their ratatui style resolution                        |
| `event/mod.rs`                | crossterm input loop; converts key events to core `key::Key`; routes events to page or popup handler |
| `event/{page,popup}.rs`       | Key event dispatch per page / popup overlay (defers to `controller.rs`)     |
| `event/dispatch.rs`           | The key → command → handler pipeline both frontends share                   |
| `ui/mod.rs`                   | ratatui render loop; main layout dispatch                                   |
| `ui/{page,playback,popup}.rs` | Render functions for pages, playback bar, popups                            |
| `ui/state.rs`, `ui/ctx.rs`    | TUI-only state (theme, layout, image rendering) and its borrow context       |
| `ui/streaming.rs`             | Audio visualizer bar chart                                                  |
| `ui/image_compositor.rs`      | Cover-art encoding/transmission (`ratatui-image`, iTerm2 escapes)           |
| `cli/`                        | Unix socket server and client for inter-process CLI commands                |

### Concurrency model

Multiple OS threads communicate via `flume` channels and `Arc<State>`:

- **Event thread** — blocking `crossterm::event::read()` loop (spawned by `frontend/tui.rs`)
- **UI thread** — poll-based ratatui render loop (spawned by `frontend/tui.rs`)
- **Tokio runtime** — async tasks: client handler, session watcher, socket listener, streaming
  (client handler + session watcher + cover-image recovery + the player-event watcher thread are
  spawned by `crates/core/src/runtime.rs`; the socket listener by the frontend)
- **Player-event watcher** — polls librespot playback state, sends `ClientRequest`s
- **Media-control thread** — OS media key events (feature-gated)

Event/UI threads never call async functions directly. They send a `ClientRequest` over a `flume` channel; the async handler updates `Arc<State>`.

Frontends poll core state at their own cadence (`app_refresh_duration_in_ms`); there is no
redraw notification. See `crates/core/src/lib.rs` for the full frontend contract.

### Feature flags

| Feature                                                                                                                                 | Effect                                                   |
| --------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------- |
| `streaming`                                                                                                                             | librespot playback, Spotify Connect, audio visualization |
| `rodio-backend`                                                                                                                         | Default audio sink (rodio)                               |
| `alsa-backend`, `pulseaudio-backend`, `portaudio-backend`, `jackaudio-backend`, `rodiojack-backend`, `sdl-backend`, `gstreamer-backend` | Alternative audio sinks                                  |
| `media-control`                                                                                                                         | OS media key integration                                 |
| `image`                                                                                                                                 | Album art rendering                                      |
| `sixel`                                                                                                                                 | Sixel terminal image protocol (extends `image`)          |
| `pixelate`                                                                                                                              | Pixelated image rendering fallback (extends `image`)     |
| `notify`                                                                                                                                | Desktop notifications                                    |
| `daemon`                                                                                                                                | Daemonize mode (implies `streaming`)                     |
| `fzf`                                                                                                                                   | Fuzzy search                                             |
| `tsp`                                                                                                                                   | Tern Surface Protocol frontend (`dep:tern-sdk`)           |

Default: `rodio-backend` + `media-control` + `tsp`. Gate feature-specific code with `#[cfg(feature = "...")]`.

## Verifying changes

CI treats all warnings as errors. Before committing, run what CI runs:

```sh
cargo fmt --all                      # CI checks: cargo fmt --all -- --check
cargo test --no-default-features --features rodio-backend,media-control,image,notify,fzf,tsp
cargo clippy --workspace --all-targets --no-default-features --features rodio-backend,media-control,image,notify,fzf,tsp -- -D warnings
cargo clippy --workspace --all-targets --no-default-features --features rodio-backend,media-control,tsp -- -D warnings   # tsp without image
cargo clippy --no-default-features --features rodio-backend,media-control,image,notify,fzf,tsp -- -D warnings
cargo clippy --no-default-features -- -D warnings   # core paths, no features
```

The tests above run the TSP frontend against an in-memory terminal. To prove it against a real
Tern — a headless pane, injected keys and clicks, the wire recording — see
[docs/tsp-testing.md](docs/tsp-testing.md).

The core crate must stay frontend-free; verify with:

```sh
cargo tree -p spotify_player_core | grep -E 'ratatui|crossterm'   # must print nothing
```

When fixing no-feature clippy warnings, you may need `#[allow(dead_code)]` / `#[allow(unused_variables)]` on items only used in feature-gated paths. If you touch `daemon`/`streaming` code, add `daemon` to the feature list above to lint those paths too.

## Conventions

### Error handling

- Return `anyhow::Result<T>` from fallible functions.
- Add context with `.context("...")` / `.with_context(|| ...)`; early-exit with `anyhow::bail!("...")`.
- Format error chains with `{err:#}` (alternate Display), never `{err}`.
- At async task boundaries, log and continue — never let one failure crash a long-running task:
  ```rust
  if let Err(err) = do_something().await {
      tracing::error!("Failed to ...: {err:#}");
  }
  ```

### Logging

Use `tracing` exclusively — never `println!`, `eprintln!`, or the `log` crate.

```rust
tracing::info!("...");
tracing::error!("...: {err:#}");
tracing::debug!("{value:?}");
```

### Comments and doc comments

- Reserve comments for non-obvious intent: invariants, ordering constraints, workarounds, edge cases.
- Do not narrate the implementation. A doc comment states a type/function's purpose and contract (what callers need to know); leave the mechanics — which branch does what, field-by-field behaviour, control flow — to the code itself. Implementation rationale belongs in a focused inline comment at the relevant line, not in the doc comment.
- Keep comments concise and clear, avoid long paragraphs and try to keep comments up to date with code changes.

## Keeping docs up to date

`README.md` and `docs/config.md` are the primary user-facing references. Update them on any user-visible change:

- **New feature / config option** — describe it in `README.md` (`Features`, `Configuration`, …), document the field in `docs/config.md`, and update `examples/app.toml` if applicable.
- **Changed / removed behaviour** — update affected tables, command descriptions, and usage examples in both files.
- **New feature flag** — add it to the feature-flags table in `README.md`.
- **New CLI subcommand** — document it under the CLI section of `README.md`.

Keep `.github/copilot-instructions.md` and this `CLAUDE.md` in sync when project structure, architecture, or conventions change significantly.

### Adding a new `Command`

1. Add the variant to `Command` in `command.rs` and update `Command::desc()`.
2. Add a default keybinding in `crates/core/src/keymap.rs`.
3. Update the command table in `README.md`.

## Writing PR descriptions

Base the description on the actual branch diff (`git diff master...<branch>`), not assumptions. Keep it clear and concise:

- **Summary** — 2-4 sentences: what problem the change solves and the approach. State the _why_ (the prior behaviour / bug) before the _what_.
- **Changes** — a bullet per logical change, each tagged with the affected module/file. Lead with the user-facing or architectural change, not mechanical edits.
- Prefer plain prose over filler; omit empty sections. Add a short **Notes** section only for non-obvious trade-offs or follow-ups.

Output the description as raw markdown in a fenced code block so it can be copied and pasted directly into the PR.
