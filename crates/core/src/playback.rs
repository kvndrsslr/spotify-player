//! The playback description, built from the user's `playback_format`.
//!
//! Framework-agnostic: the runs and what produced them are decided here, and a frontend only
//! styles them. [`playback_spans`] takes its configuration and the data it reads as explicit
//! parameters; [`current_playback_spans`] fills them in from the shared application state.

use std::sync::LazyLock;

use rspotify::model::{Id, PlayableItem};

use crate::{
    config,
    state::{AppData, PlaybackMetadata, PlayerState, Track},
    utils::{map_join, to_bidi_string},
};

/// A `{…}` format argument or a newline in [`config::AppConfig::playback_format`].
static FORMAT_TOKEN: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\{.*?\}|\n").expect("valid regex"));

/// What produced a [`PlaybackSpan`]'s text, which decides how a frontend styles it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackToken {
    /// Literal text from the format string, unstyled.
    Literal,
    /// `{status}`: the play or pause icon.
    Status,
    /// `{liked}`: the liked icon, only for a saved track.
    Liked,
    /// `{track}`.
    Track,
    /// `{track_number}`.
    TrackNumber,
    /// `{artists}`.
    Artists,
    /// `{album}`.
    Album,
    /// `{genres}`.
    Genres,
    /// `{metadata}`: the fields listed in [`config::AppConfig::playback_metadata_fields`].
    Metadata,
}

/// One run of the playback description.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaybackSpan {
    /// What produced the run.
    pub token: PlaybackToken,
    /// The run's text.
    pub text: String,
}

/// Everything [`playback_spans`] formats with: the user's format string and icons, and the library
/// data the format can reach.
pub struct PlaybackFormat<'a> {
    /// [`config::AppConfig::playback_format`].
    pub format: &'a str,
    /// [`config::AppConfig::playback_metadata_fields`], which `{metadata}` renders.
    pub metadata_fields: &'a [String],
    /// [`config::AppConfig::play_icon`].
    pub play_icon: &'a str,
    /// [`config::AppConfig::pause_icon`].
    pub pause_icon: &'a str,
    /// [`config::AppConfig::liked_icon`].
    pub liked_icon: &'a str,
    /// [`config::AppConfig::genre_num`], the number of genres `{genres}` keeps.
    pub genre_num: u8,
    /// The data `{liked}` (saved tracks) and `{genres}` (cached artist genres) read.
    pub data: &'a AppData,
}

/// The description of `playable`, formatted with `format`.
///
/// One `Vec<PlaybackSpan>` per line: a `\n` in the format string starts a new line (an empty one
/// included), and the last line is only produced when it has runs. A run whose data is missing —
/// `{track_number}` for an episode, `{liked}` for an unsaved or unknown item, `{track}` and
/// friends for an item the app cannot describe, or a `{token}` the app does not know — is dropped.
#[must_use]
pub fn playback_spans(
    format: &PlaybackFormat<'_>,
    playable: &PlayableItem,
    playback: &PlaybackMetadata,
) -> Vec<Vec<PlaybackSpan>> {
    let mut lines: Vec<Vec<PlaybackSpan>> = Vec::new();
    let mut spans: Vec<PlaybackSpan> = Vec::new();

    let mut ptr = 0;
    for m in FORMAT_TOKEN.find_iter(format.format) {
        let s = m.start();
        let e = m.end();
        if ptr < s {
            spans.push(literal(&format.format[ptr..s]));
        }
        ptr = e;

        let (token, text) = match m.as_str() {
            // upon encountering a newline, start a new line
            "\n" => {
                lines.push(std::mem::take(&mut spans));
                continue;
            }
            "{status}" => (
                PlaybackToken::Status,
                if playback.is_playing {
                    format.play_icon
                } else {
                    format.pause_icon
                }
                .to_owned(),
            ),
            "{liked}" => match playable {
                PlayableItem::Track(track) => match &track.id {
                    Some(id) => {
                        if format.data.user_data.saved_tracks.contains_key(&id.uri()) {
                            (PlaybackToken::Liked, format.liked_icon.to_owned())
                        } else {
                            continue;
                        }
                    }
                    None => continue,
                },
                PlayableItem::Episode(_) | PlayableItem::Unknown(_) => continue,
            },
            "{track}" => match playable {
                PlayableItem::Track(track) => (
                    PlaybackToken::Track,
                    Track::try_from_full_track(track.clone()).map_or_else(
                        || "Unknown Track".to_string(),
                        |t| to_bidi_string(&t.display_name()),
                    ),
                ),
                PlayableItem::Episode(episode) => {
                    let bidi_string = to_bidi_string(&episode.name);
                    (
                        PlaybackToken::Track,
                        if episode.explicit {
                            format!("{bidi_string} (E)")
                        } else {
                            bidi_string
                        },
                    )
                }
                PlayableItem::Unknown(_) => continue,
            },
            "{track_number}" => match playable {
                PlayableItem::Track(track) => (
                    PlaybackToken::TrackNumber,
                    to_bidi_string(&track.track_number.to_string()),
                ),
                PlayableItem::Episode(_) | PlayableItem::Unknown(_) => continue,
            },
            "{artists}" => match playable {
                PlayableItem::Track(track) => (
                    PlaybackToken::Artists,
                    to_bidi_string(&map_join(&track.artists, |a| &a.name, ", ")),
                ),
                PlayableItem::Episode(episode) => {
                    (PlaybackToken::Artists, episode.show.publisher.clone())
                }
                PlayableItem::Unknown(_) => continue,
            },
            "{album}" => match playable {
                PlayableItem::Track(track) => {
                    (PlaybackToken::Album, to_bidi_string(&track.album.name))
                }
                PlayableItem::Episode(episode) => {
                    (PlaybackToken::Album, to_bidi_string(&episode.show.name))
                }
                PlayableItem::Unknown(_) => continue,
            },
            "{genres}" => match playable {
                PlayableItem::Track(full_track) => {
                    let genre = match format.data.caches.genres.get(&full_track.artists[0].name) {
                        Some(genres) => &format_genres(genres, format.genre_num),
                        None => "no genre",
                    };
                    (PlaybackToken::Genres, to_bidi_string(genre))
                }
                PlayableItem::Episode(_) => (PlaybackToken::Genres, to_bidi_string("no genre")),
                PlayableItem::Unknown(_) => continue,
            },
            "{metadata}" => (
                PlaybackToken::Metadata,
                playback_metadata_text(playback, format.metadata_fields),
            ),
            _ => continue,
        };

        spans.push(PlaybackSpan { token, text });
    }
    if ptr < format.format.len() {
        spans.push(literal(&format.format[ptr..]));
    }
    if !spans.is_empty() {
        lines.push(spans);
    }

    lines
}

/// The current playback's description, or an empty vec when there is nothing to describe (no
/// buffered playback metadata or no playback item).
///
/// Reads the data cache through the `data` guard the caller already holds instead of locking it:
/// this is called from render paths, and a nested acquisition would block forever on the guard the
/// calling thread holds. Nothing under a `data` guard may take a `data` lock.
#[must_use]
pub fn current_playback_spans(data: &AppData, player: &PlayerState) -> Vec<Vec<PlaybackSpan>> {
    let Some(playback) = player.buffered_playback.as_ref() else {
        return Vec::new();
    };
    let Some(playable) = player.playback.as_ref().and_then(|p| p.item.as_ref()) else {
        return Vec::new();
    };

    let configs = config::get_config();

    playback_spans(
        &PlaybackFormat {
            format: &configs.app_config.playback_format,
            metadata_fields: &configs.app_config.playback_metadata_fields,
            play_icon: &configs.app_config.play_icon,
            pause_icon: &configs.app_config.pause_icon,
            liked_icon: &configs.app_config.liked_icon,
            genre_num: configs.app_config.genre_num,
            data,
        },
        playable,
        playback,
    )
}

/// The `{metadata}` text: the configured fields, joined with `" | "`.
fn playback_metadata_text(playback: &PlaybackMetadata, fields: &[String]) -> String {
    let repeat_value = <&'static str>::from(playback.repeat_state).to_string();

    let volume_value = if let Some(volume) = playback.mute_state {
        format!("{volume}% (muted)")
    } else {
        format!("{}%", playback.volume.unwrap_or_default())
    };

    let mut parts = vec![];

    for field in fields {
        match field.as_str() {
            "repeat" => parts.push(format!("repeat: {repeat_value}")),
            "shuffle" => parts.push(format!("shuffle: {}", playback.shuffle_state)),
            "volume" => parts.push(format!("volume: {volume_value}")),
            "device" => parts.push(format!("device: {}", playback.device_name)),
            _ => {}
        }
    }

    parts.join(" | ")
}

fn literal(text: &str) -> PlaybackSpan {
    PlaybackSpan {
        token: PlaybackToken::Literal,
        text: text.to_owned(),
    }
}

/// formats genres depending on the number of genres and `genre_num`
///
/// Examples for `genre_num = 2`
/// - 1 genre: "genre1"
/// - 2 genres: "genre1, genre2"
/// - \>= 3 genres: "genre1, genre2, ..."
fn format_genres(genres: &[String], genre_num: u8) -> String {
    let mut genre_str = String::with_capacity(64);

    if genre_num > 0 {
        for i in 0..genres.len() {
            genre_str.push_str(&genres[i]);

            if i + 1 != genres.len() {
                genre_str.push_str(", ");

                if i + 1 >= genre_num as usize {
                    genre_str.push_str("...");
                    break;
                }
            }
        }
    }

    genre_str
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AlbumId, ArtistId, TrackId};
    use chrono::Duration as ChronoDuration;
    use rspotify::model::{
        FullTrack, RepeatState, SimplifiedAlbum, SimplifiedArtist, TrackId as RspotifyTrackId, Type,
    };
    use std::collections::HashMap;
    use std::path::Path;
    use std::time::Duration;

    const TRACK_ID: &str = "1eX57c7iU9Q2b0t6s0kJ2vW";

    fn simplified_artist(name: &str) -> SimplifiedArtist {
        SimplifiedArtist {
            external_urls: HashMap::new(),
            href: None,
            id: Some(ArtistId::from_id("0hXJ7TZ9a5p0MZf2s2X1vJ").expect("valid artist id")),
            name: name.to_owned(),
        }
    }

    /// A track whose title, artists, album and number the format can render.
    fn full_track() -> FullTrack {
        FullTrack {
            album: SimplifiedAlbum {
                album_group: None,
                album_type: Some("album".to_owned()),
                artists: vec![simplified_artist("The Artist")],
                available_markets: Vec::new(),
                external_urls: HashMap::new(),
                href: None,
                id: Some(AlbumId::from_id("4aawyAB9vmqN3uQ7FjRGTy").expect("valid album id")),
                images: Vec::new(),
                name: "The Album".to_owned(),
                release_date: Some("2024-01-01".to_owned()),
                release_date_precision: Some("day".to_owned()),
                restrictions: None,
            },
            artists: vec![simplified_artist("The Artist"), simplified_artist("Other")],
            available_markets: Vec::new(),
            disc_number: 1,
            duration: ChronoDuration::seconds(200),
            explicit: false,
            external_ids: HashMap::new(),
            external_urls: HashMap::new(),
            href: None,
            id: Some(RspotifyTrackId::from_id(TRACK_ID).expect("valid track id")),
            is_local: false,
            is_playable: None,
            linked_from: None,
            restrictions: None,
            name: "The Title".to_owned(),
            popularity: 0,
            preview_url: None,
            track_number: 7,
            r#type: Type::Track,
        }
    }

    fn playable() -> PlayableItem {
        PlayableItem::Track(full_track())
    }

    fn playback_metadata() -> PlaybackMetadata {
        PlaybackMetadata {
            device_name: "The Device".to_owned(),
            device_id: None,
            volume: Some(55),
            is_playing: true,
            repeat_state: RepeatState::Context,
            shuffle_state: true,
            mute_state: None,
        }
    }

    fn track_uri() -> String {
        RspotifyTrackId::from_id(TRACK_ID)
            .expect("valid track id")
            .uri()
    }

    /// Data with the saved tracks and cached artist genres a format can read. The cache folder does
    /// not exist, so the file caches load nothing.
    fn app_data(saved_uris: &[&str], genres: &[(&str, &[&str])]) -> AppData {
        let mut data = AppData::new(Path::new("/nonexistent-spotify-player-test-cache"));

        for uri in saved_uris {
            data.user_data.saved_tracks.insert(
                (*uri).to_owned(),
                Track {
                    id: TrackId::from_id(TRACK_ID).expect("valid track id"),
                    name: "The Title".to_owned(),
                    artists: Vec::new(),
                    album: None,
                    duration: Duration::from_secs(200),
                    explicit: false,
                    added_at: 0,
                },
            );
        }
        for (artist, genres) in genres {
            let genres = genres.iter().map(|g| (*g).to_owned()).collect::<Vec<_>>();
            data.caches
                .genres
                .insert((*artist).to_owned(), genres, Duration::from_secs(60));
        }

        data
    }

    fn line(pairs: &[(PlaybackToken, &str)]) -> Vec<PlaybackSpan> {
        pairs
            .iter()
            .map(|(token, text)| PlaybackSpan {
                token: *token,
                text: (*text).to_owned(),
            })
            .collect()
    }

    fn metadata_fields(fields: &[&str]) -> Vec<String> {
        fields.iter().map(|f| (*f).to_owned()).collect()
    }

    #[test]
    fn renders_every_supported_token() {
        let data = app_data(&[&track_uri()], &[("The Artist", &["art rock", "pop"])]);
        let fields = metadata_fields(&["repeat", "shuffle", "volume", "device"]);
        let format = PlaybackFormat {
            format: "{status} {liked}{track} • {artists} • {album} #{track_number} [{genres}] {metadata}",
            metadata_fields: &fields,
            play_icon: "PLAY",
            pause_icon: "PAUSE",
            liked_icon: "LIKE",
            genre_num: 2,
            data: &data,
        };

        let lines = playback_spans(&format, &playable(), &playback_metadata());

        assert_eq!(
            lines,
            vec![line(&[
                (PlaybackToken::Status, "PLAY"),
                (PlaybackToken::Literal, " "),
                (PlaybackToken::Liked, "LIKE"),
                (PlaybackToken::Track, "The Title"),
                (PlaybackToken::Literal, " • "),
                (PlaybackToken::Artists, "The Artist, Other"),
                (PlaybackToken::Literal, " • "),
                (PlaybackToken::Album, "The Album"),
                (PlaybackToken::Literal, " #"),
                (PlaybackToken::TrackNumber, "7"),
                (PlaybackToken::Literal, " ["),
                (PlaybackToken::Genres, "art rock, pop"),
                (PlaybackToken::Literal, "] "),
                (
                    PlaybackToken::Metadata,
                    "repeat: context | shuffle: true | volume: 55% | device: The Device"
                ),
            ])]
        );
    }

    #[test]
    fn drops_runs_whose_data_is_missing() {
        let data = app_data(&[], &[]); // nothing saved, no cached genres
        let fields = metadata_fields(&["volume"]);
        let format = PlaybackFormat {
            format: "{status}{liked}{genres}{metadata}",
            metadata_fields: &fields,
            play_icon: "PLAY",
            pause_icon: "PAUSE",
            liked_icon: "LIKE",
            genre_num: 2,
            data: &data,
        };
        let mut playback = playback_metadata();
        playback.is_playing = false;
        playback.mute_state = Some(30); // the muted value, as before

        let lines = playback_spans(&format, &playable(), &playback);

        assert_eq!(
            lines,
            vec![line(&[
                (PlaybackToken::Status, "PAUSE"),
                // `{liked}` is dropped: the track is not saved
                (PlaybackToken::Genres, "no genre"),
                (PlaybackToken::Metadata, "volume: 30% (muted)"),
            ])]
        );
    }

    #[test]
    fn drops_unknown_tokens_and_keeps_unmatched_braces_literal() {
        let data = app_data(&[], &[]);
        let fields = metadata_fields(&[]);
        let format = PlaybackFormat {
            format: "a {nope} b {c",
            metadata_fields: &fields,
            play_icon: "PLAY",
            pause_icon: "PAUSE",
            liked_icon: "LIKE",
            genre_num: 2,
            data: &data,
        };

        let lines = playback_spans(&format, &playable(), &playback_metadata());

        // `{nope}` matches the format grammar but names no token, so it is dropped;
        // `{c` has no closing brace, so the regex never matched it and it stays literal.
        assert_eq!(
            lines,
            vec![line(&[
                (PlaybackToken::Literal, "a "),
                (PlaybackToken::Literal, " b {c"),
            ])]
        );
    }

    #[test]
    fn drops_item_tokens_for_an_unknown_item() {
        let data = app_data(&[], &[]);
        let fields = metadata_fields(&["device"]);
        let format = PlaybackFormat {
            format: "{status}{track}{artists}{album}{track_number}{genres}{metadata}",
            metadata_fields: &fields,
            play_icon: "PLAY",
            pause_icon: "PAUSE",
            liked_icon: "LIKE",
            genre_num: 2,
            data: &data,
        };

        let lines = playback_spans(
            &format,
            &PlayableItem::Unknown(serde_json::json!({})),
            &playback_metadata(),
        );

        assert_eq!(
            lines,
            vec![line(&[
                (PlaybackToken::Status, "PLAY"),
                (PlaybackToken::Metadata, "device: The Device"),
            ])]
        );
    }

    #[test]
    fn splits_lines_on_newlines() {
        let data = app_data(&[], &[]);
        let fields = metadata_fields(&[]);
        let format = PlaybackFormat {
            format: "a\n\nb\n",
            metadata_fields: &fields,
            play_icon: "PLAY",
            pause_icon: "PAUSE",
            liked_icon: "LIKE",
            genre_num: 2,
            data: &data,
        };

        let lines = playback_spans(&format, &playable(), &playback_metadata());

        assert_eq!(
            lines,
            vec![
                line(&[(PlaybackToken::Literal, "a")]),
                // the empty line between the two newlines is kept
                Vec::new(),
                line(&[(PlaybackToken::Literal, "b")]),
            ],
            "a trailing newline must not add a last empty line"
        );
    }

    #[test]
    fn empty_formats_produce_no_lines() {
        let data = app_data(&[], &[]);
        let fields = metadata_fields(&[]);
        let mut format = PlaybackFormat {
            format: "",
            metadata_fields: &fields,
            play_icon: "PLAY",
            pause_icon: "PAUSE",
            liked_icon: "LIKE",
            genre_num: 2,
            data: &data,
        };

        assert_eq!(
            playback_spans(&format, &playable(), &playback_metadata()),
            Vec::<Vec<PlaybackSpan>>::new()
        );

        format.format = "\n";
        assert_eq!(
            playback_spans(&format, &playable(), &playback_metadata()),
            vec![Vec::new()],
            "a single newline is one empty line"
        );
    }

    #[test]
    fn metadata_without_fields_is_an_empty_run() {
        let data = app_data(&[], &[]);
        let fields = metadata_fields(&[]);
        let format = PlaybackFormat {
            format: "{track}{metadata}",
            metadata_fields: &fields,
            play_icon: "PLAY",
            pause_icon: "PAUSE",
            liked_icon: "LIKE",
            genre_num: 2,
            data: &data,
        };

        let lines = playback_spans(&format, &playable(), &playback_metadata());

        assert_eq!(
            lines,
            vec![line(&[
                (PlaybackToken::Track, "The Title"),
                (PlaybackToken::Metadata, ""),
            ])]
        );
    }

    #[test]
    fn format_genres_truncates_at_genre_num() {
        let genres = ["rock".to_string(), "pop".to_string(), "jazz".to_string()];

        assert_eq!(format_genres(&genres[..1], 2), "rock");
        assert_eq!(format_genres(&genres[..2], 2), "rock, pop");
        assert_eq!(format_genres(&genres, 2), "rock, pop, ...");
        assert_eq!(format_genres(&genres, 8), "rock, pop, jazz");
        assert_eq!(format_genres(&genres, 0), "");
        assert_eq!(format_genres(&[], 1), "");
    }
}
