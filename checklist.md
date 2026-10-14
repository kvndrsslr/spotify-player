# Checklists

## Pre-release checklist

- [ ] change the `version` in `spotify_player/Cargo.toml`, in `crates/core/Cargo.toml`, and in the `spotify_player_core` dependency of `spotify_player/Cargo.toml`.
- [ ] run `cargo clippy` to check the codes as well as to update `Cargo.lock`
- [ ] run `./scripts/publish` to publish the crates (`spotify_player_core` first, then `spotify_player`, which depends on it)
- [ ] create a new release in [`github`](https://github.com/aome510/spotify-player/releases/new)

## Creating new commands

- [ ] add new entries to the `Command` enum defined in `command.rs` and to the `Command::desc` function
- [ ] add a new default key mapping for the command in `keymap.rs`
- [ ] update the command table in `readme.md`

**Note**: should follow a similar checklist when modifying a command
