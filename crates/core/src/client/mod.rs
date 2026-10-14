use std::collections::HashSet;
use std::ops::Deref;
use std::{borrow::Cow, collections::HashMap, sync::Arc};

use crate::state::Lyrics;
use crate::{auth, config};
use crate::{
    auth::AuthConfig,
    state::{
        load_context_from_file_cache, store_context_in_file_cache, store_data_into_file_cache,
        Album, AlbumId, Artist, ArtistId, Category, Context, ContextId, Device, Episode, EpisodeId,
        FileCacheKey, Item, ItemId, MemoryCaches, PageState, Playback, PlaybackMetadata, Playlist,
        PlaylistFolderItem, PlaylistId, SearchResults, SharedState, Show, ShowId, Track, TrackId,
        UserId, TTL_CACHE_DURATION, USER_LIKED_TRACKS_URI, USER_RECENTLY_PLAYED_TRACKS_URI,
        USER_TOP_TRACKS_URI,
    },
};
// Only the local (librespot) transport starts `Tracks` contexts directly.
#[cfg(feature = "streaming")]
use crate::state::TracksId;

use std::io::Write;

use anyhow::Context as _;
use anyhow::Result;

use librespot_core::SpotifyUri;
#[cfg(feature = "streaming")]
use parking_lot::Mutex;

use reqwest::StatusCode;
use rspotify::{http::Query, prelude::*};

mod handlers;
mod rate_limit;
mod request;
mod spotify;
mod volume;

pub use handlers::*;
pub use request::*;
use serde::Deserialize;
pub(crate) use spotify::WebApiClient;

const SPOTIFY_API_ENDPOINT: &str = "https://api.spotify.com/v1";

/// The application's Spotify client
#[derive(Clone)]
pub struct AppClient {
    http: reqwest::Client,
    /// Paces Web API requests and holds the client-wide rate-limit cooldown.
    rate_limiter: Arc<rate_limit::RateLimiter>,
    /// Coalesces bursts of remote (Web API) volume adjustments into single calls.
    remote_volume: volume::RemoteVolume,
    /// The integrated Spotify client, mainly used for streaming and librespot integration
    spotify: Arc<spotify::Spotify>,
    auth_config: AuthConfig,
    /// The Spotify Web API client, used for interacting with Spotify Web APIs
    api_client: WebApiClient,
    /// The integrated streaming connection (librespot spirc), replaced on
    /// (re)connect and torn down explicitly during deliberate replacements.
    #[cfg(feature = "streaming")]
    stream_conn: Arc<Mutex<Option<librespot_connect::Spirc>>>,
    /// Sender half of the link-drop notification channel; the streaming
    /// watchdog uses it to signal an unexpected Connect link drop.
    #[cfg(feature = "streaming")]
    stream_drop_tx: flume::Sender<()>,
    /// Receiver half of [`Self::stream_drop_tx`]; consumed by the session
    /// watcher to trigger reconnection on an unexpected link drop.
    #[cfg(feature = "streaming")]
    stream_drop_rx: flume::Receiver<()>,
    /// Monotonic counter advanced whenever the streaming connection is
    /// deliberately replaced (re-auth or `RestartIntegratedClient`). A spirc
    /// watchdog task captures the epoch at spawn and only signals a link drop
    /// if the counter is unchanged — i.e. the drop was not caused by a
    /// deliberate reconnect.
    #[cfg(feature = "streaming")]
    stream_epoch: Arc<std::sync::atomic::AtomicU64>,
    /// Whether this session has already claimed the integrated device as the active one
    /// (`activate()`), and thus whether librespot's own player commands can be used.
    #[cfg(feature = "streaming")]
    stream_claimed_active: Arc<std::sync::atomic::AtomicBool>,
    /// Guards the post-change playback poll so rapid player events coalesce into a single
    /// in-flight loop rather than flooding Spotify's Web API (which 429 rate-limits).
    update_playback_in_flight: Arc<std::sync::atomic::AtomicBool>,
    /// Context URIs whose `GetContext` fetch is currently running. The page-change handler
    /// re-requests an uncached context every few seconds; while the first fetch is still in
    /// flight (slow API or a shared 429 cooldown) a duplicate would start a second fetch that
    /// resumes on the same cooldown and re-429s in waves. Duplicates are dropped here.
    context_fetch_in_flight: Arc<parking_lot::Mutex<HashSet<String>>>,
    /// Per-url refetch bookkeeping for render-path image cache misses: attempts + next
    /// eligible instant (exponential backoff).
    #[cfg(feature = "image")]
    image_retries:
        Arc<parking_lot::Mutex<std::collections::HashMap<String, (u8, std::time::Instant)>>>,
}

impl Deref for AppClient {
    type Target = WebApiClient;
    fn deref(&self) -> &Self::Target {
        &self.api_client
    }
}

/// Build the Spotify Web API client from the configured client ID.
///
/// The returned client is unauthenticated; call [`auth::prompt_for_user_token`] to obtain an
/// access token.
pub fn new_api_client() -> Result<WebApiClient> {
    let configs = config::get_config();

    let id = configs.app_config.get_client_id()?;
    // The bundled default (ncspot's client ID) is registered with extended quota mode and
    // predates Spotify's 2024 Web API changes, so it is far less likely to hit rate limits
    // than a freshly-registered client. Warn users who override it that they may run into
    // `429 Too Many Requests` / `403 Forbidden` errors.
    //
    // See https://github.com/aome510/spotify-player/issues/890 for details.
    if id != auth::NCSPOT_CLIENT_ID {
        tracing::warn!(
            "A custom `client_id` ({id}) is configured. Newly-registered Spotify clients \
             use the restricted default quota mode and may hit rate-limit (429) or \
             forbidden (403) errors. Unless you specifically need your own client, \
             consider removing `client_id`/`client_id_command` to use the bundled default. \
             See https://github.com/aome510/spotify-player/issues/890 for details."
        );
    }

    let creds = rspotify::Credentials { id, secret: None };
    let mut scopes = auth::OAUTH_SCOPES
        .iter()
        .map(ToString::to_string)
        .collect::<HashSet<_>>();
    // `user-personalized` scope is not supported by the Web API client and only available to the official Spotify client
    scopes.remove("user-personalized");
    let oauth = rspotify::OAuth {
        redirect_uri: configs.app_config.login_redirect_uri.clone(),
        scopes,
        ..Default::default()
    };
    let config = rspotify::Config {
        token_cached: true,
        cache_path: configs.cache_folder.join("user_client_token.json"),
        ..Default::default()
    };
    Ok(WebApiClient::new(
        rspotify::AuthCodePkceSpotify::with_config(creds, oauth, config),
    ))
}

#[cfg(feature = "streaming")]
impl AppClient {
    /// Returns a clone of the sender used to signal an unexpected Connect link drop.
    #[must_use]
    pub fn stream_drop_sender(&self) -> flume::Sender<()> {
        self.stream_drop_tx.clone()
    }

    /// Returns the receiver that yields a signal whenever the integrated Connect
    /// link drops unexpectedly. Consumed by the session watcher to reconnect.
    #[must_use]
    pub fn stream_drop_receiver(&self) -> flume::Receiver<()> {
        self.stream_drop_rx.clone()
    }

    /// Returns the current streaming-connection epoch. A spirc watchdog task
    /// captures this at spawn; an unexpected link drop is signalled only if the
    /// epoch is unchanged when the task ends.
    #[must_use]
    pub fn stream_epoch(&self) -> u64 {
        self.stream_epoch.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Advances the streaming-connection epoch, marking the current connection
    /// as deliberately replaced. Call before tearing down an old connection so
    /// its watchdog task does not spuriously signal a link drop.
    pub fn advance_stream_epoch(&self) {
        self.stream_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Minimal deserialization target for a Spotify show. rspotify 0.15.3's `SimplifiedShow`/
/// `FullShow` types require the `available_markets` field, which Spotify removed from API
/// responses; this struct only captures the fields the app uses.
#[derive(Deserialize, Debug, Clone)]
struct ShowInfo {
    id: ShowId<'static>,
    name: String,
    #[serde(default)]
    publisher: String,
    #[cfg(feature = "image")]
    /// Cover art, carried so the episode footer can fall back to show artwork.
    #[serde(default)]
    images: Vec<EpisodeImageInfo>,
}

/// A saved show object returned by `/me/shows`.
#[derive(Deserialize, Debug, Clone)]
struct SavedShow {
    show: ShowInfo,
}

/// A full show object returned by `/shows/{id}`.
#[derive(Deserialize, Debug, Clone)]
struct FullShow {
    id: ShowId<'static>,
    name: String,
    #[serde(default)]
    publisher: String,
    #[cfg(feature = "image")]
    #[serde(default)]
    images: Vec<EpisodeImageInfo>,
    episodes: rspotify::model::Page<SimplifiedEpisodeInfo>,
}

/// Deserialization target for a show's episodes. Carries the fields the episode table and detail
/// footer need, avoiding a per-episode `/episodes/{id}` request. `images` is gated on the `image`
/// feature since only the cover-image footer reads it.
#[derive(Deserialize, Debug, Clone)]
struct SimplifiedEpisodeInfo {
    id: EpisodeId<'static>,
    name: String,
    release_date: String,
    duration_ms: u64,
    #[serde(default)]
    description: String,
    #[cfg(feature = "image")]
    #[serde(default)]
    images: Vec<EpisodeImageInfo>,
}

impl From<SimplifiedEpisodeInfo> for Episode {
    fn from(e: SimplifiedEpisodeInfo) -> Self {
        Self {
            id: e.id,
            name: e.name,
            description: e.description,
            duration: std::time::Duration::from_millis(e.duration_ms),
            show: None,
            release_date: e.release_date,
            #[cfg(feature = "image")]
            image_url: e.images.first().map(|i| i.url.clone()),
        }
    }
}

/// A `/search?type=show` response body.
#[derive(Deserialize, Debug)]
struct ShowSearchResult {
    shows: rspotify::model::Page<ShowInfo>,
}

/// A `/me/player/devices` response body.
#[derive(Deserialize, Debug)]
struct DevicesResponse {
    devices: Vec<rspotify::model::Device>,
}
/// A single cover image from an episode's `images` array.
#[cfg(feature = "image")]
#[derive(Deserialize, Debug, Clone)]
struct EpisodeImageInfo {
    url: String,
}

// ---------------------------------------------------------------------------------------------
// Tolerant deserialization targets for Spotify's reduced response shapes.
//
// A newly registered client ID receives payloads that omit fields rspotify's models require
// (`followers`/`genres`/`popularity` on artists, `popularity` on tracks and albums, `tracks` on
// playlists, `item` instead of `track` in playlist entries). These local targets capture only
// the fields the app uses, so one withheld non-essential field can no longer abort a whole list
// or context fetch. When a field *is* present the extracted values are the same as rspotify's,
// so other client IDs keep the same behaviour.
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize, Debug, Clone)]
struct TolerantArtist {
    id: ArtistId<'static>,
    name: String,
}

impl From<TolerantArtist> for Artist {
    fn from(artist: TolerantArtist) -> Self {
        Artist {
            id: artist.id,
            name: artist.name,
        }
    }
}

/// An artist as returned by `/artists/{id}`.
///
/// Only `name` and `genres` are read by the app (the playback pane's genre field), and the
/// reduced shape carries no follower count, genres or popularity at all.
#[derive(Deserialize, Debug, Clone)]
struct TolerantFullArtist {
    id: ArtistId<'static>,
    name: String,
    #[serde(default)]
    genres: Vec<String>,
    #[serde(default)]
    followers: Option<TolerantFollowers>,
    #[serde(default)]
    popularity: Option<u32>,
}

#[derive(Deserialize, Debug, Clone)]
struct TolerantFollowers {
    #[serde(default)]
    total: u32,
}

impl From<TolerantFullArtist> for rspotify::model::FullArtist {
    fn from(artist: TolerantFullArtist) -> Self {
        Self {
            followers: rspotify::model::Followers {
                total: artist.followers.map_or(0, |followers| followers.total),
            },
            genres: artist.genres,
            id: artist.id,
            name: artist.name,
            popularity: artist.popularity.unwrap_or_default(),
            // Not part of the app's artist model and never read for artists (only album and
            // episode covers are rendered), so the reduced shape's `images` are ignored.
            external_urls: HashMap::new(),
            href: String::new(),
            images: Vec::new(),
        }
    }
}

/// A cover image, as Spotify returns it.
#[derive(Deserialize, Debug, Clone)]
struct TolerantImage {
    url: String,
    #[serde(default)]
    height: Option<u32>,
    #[serde(default)]
    width: Option<u32>,
}

#[derive(Deserialize, Debug, Clone)]
struct TolerantAlbum {
    #[serde(default)]
    id: Option<AlbumId<'static>>,
    name: String,
    #[serde(default)]
    release_date: String,
    #[serde(default)]
    artists: Vec<TolerantArtist>,
    #[serde(default)]
    album_type: Option<String>,
    /// The pane renders the currently playing item's cover, so these are kept.
    #[serde(default)]
    images: Vec<TolerantImage>,
}

impl TolerantAlbum {
    fn into_album(self) -> Option<Album> {
        Some(Album {
            id: self.id?,
            name: self.name,
            release_date: self.release_date,
            artists: self.artists.into_iter().map(Into::into).collect(),
            typ: self.album_type.as_deref().and_then(album_type_from_str),
            added_at: 0,
        })
    }
}

/// Mirrors rspotify's `album_type` string to [`rspotify::model::AlbumType`] mapping.
fn album_type_from_str(album_type: &str) -> Option<rspotify::model::AlbumType> {
    match album_type.to_ascii_lowercase().as_str() {
        "album" => Some(rspotify::model::AlbumType::Album),
        "single" => Some(rspotify::model::AlbumType::Single),
        "appears_on" => Some(rspotify::model::AlbumType::AppearsOn),
        "compilation" => Some(rspotify::model::AlbumType::Compilation),
        _ => None,
    }
}

#[derive(Deserialize, Debug, Clone)]
struct TolerantTrackLink {
    #[serde(default)]
    id: Option<TrackId<'static>>,
}

#[derive(Deserialize, Debug, Clone)]
struct TolerantTrack {
    #[serde(default)]
    id: Option<TrackId<'static>>,
    name: String,
    #[serde(default)]
    artists: Vec<TolerantArtist>,
    #[serde(default)]
    album: Option<TolerantAlbum>,
    #[serde(default)]
    duration_ms: u64,
    #[serde(default)]
    explicit: bool,
    #[serde(default)]
    is_playable: Option<bool>,
    #[serde(default)]
    linked_from: Option<TolerantTrackLink>,
}

impl TolerantTrack {
    /// Convert into the app's track, mirroring `Track::try_from_full_track` (honouring
    /// `is_playable` and track relinking).
    fn into_track(self, added_at: Option<chrono::DateTime<chrono::Utc>>) -> Option<Track> {
        if !self.is_playable.unwrap_or(true) {
            return None;
        }
        let id = match self.linked_from {
            Some(link) => link.id?,
            None => self.id?,
        };
        Some(Track {
            id,
            name: self.name,
            artists: self.artists.into_iter().map(Into::into).collect(),
            album: self.album.and_then(TolerantAlbum::into_album),
            duration: std::time::Duration::from_millis(self.duration_ms),
            explicit: self.explicit,
            added_at: added_at.map(|t| t.timestamp() as u64).unwrap_or_default(),
        })
    }
}

#[derive(Deserialize, Debug, Clone)]
struct TolerantShow {
    id: ShowId<'static>,
    name: String,
    #[serde(default)]
    publisher: String,
    #[cfg(feature = "image")]
    #[serde(default)]
    images: Vec<EpisodeImageInfo>,
}

#[derive(Deserialize, Debug, Clone)]
struct TolerantEpisode {
    id: EpisodeId<'static>,
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    duration_ms: u64,
    #[serde(default)]
    release_date: String,
    #[serde(default)]
    show: Option<TolerantShow>,
    #[cfg(feature = "image")]
    #[serde(default)]
    images: Vec<EpisodeImageInfo>,
}

impl From<TolerantEpisode> for Episode {
    fn from(episode: TolerantEpisode) -> Self {
        Self {
            id: episode.id,
            name: episode.name,
            description: episode.description,
            duration: std::time::Duration::from_millis(episode.duration_ms),
            show: episode.show.map(|show| Show {
                id: show.id,
                name: show.name,
                publisher: show.publisher,
                #[cfg(feature = "image")]
                image_url: show.images.first().map(|i| i.url.clone()),
            }),
            release_date: episode.release_date,
            #[cfg(feature = "image")]
            image_url: episode.images.first().map(|i| i.url.clone()),
        }
    }
}

/// Parse a playable item from raw JSON, tolerating the reduced shapes. Returns a real
/// [`rspotify::model::PlayableItem`] (never `Unknown`) so the playback pane can display it.
fn playable_item_from_value(value: &serde_json::Value) -> Option<rspotify::model::PlayableItem> {
    match value.get("type").and_then(|typ| typ.as_str()) {
        Some("episode") => {
            let episode: TolerantEpisode = serde_json::from_value(value.clone()).ok()?;
            app_episode_to_playable(&Episode::from(episode))
        }
        Some("track") => {
            let track: TolerantTrack = serde_json::from_value(value.clone()).ok()?;
            // The pane renders the currently playing item's cover, so the album images are
            // carried over (the app's own track model has no image field).
            let images = track.album.as_ref().map_or_else(Vec::new, |album| {
                album
                    .images
                    .iter()
                    .map(|image| rspotify::model::Image {
                        url: image.url.clone(),
                        height: image.height,
                        width: image.width,
                    })
                    .collect()
            });
            let track = track.into_track(None)?;
            let mut item = app_track_to_playable(&track);
            if let rspotify::model::PlayableItem::Track(full_track) = &mut item {
                full_track.album.images = images;
            }
            Some(item)
        }
        _ => None,
    }
}

/// Like [`playable_item_from_value`], but keeps the raw JSON for anything that cannot be turned
/// into a track or episode. This mirrors rspotify's own `PlayableItem` fallback, so local files
/// and other shapes the reduced payloads cannot express still reach the UI (which renders them
/// as blank rows) instead of disappearing from the queue.
fn playable_item_from_value_or_unknown(value: &serde_json::Value) -> rspotify::model::PlayableItem {
    playable_item_from_value(value)
        .unwrap_or_else(|| rspotify::model::PlayableItem::Unknown(value.clone()))
}

/// A cursor-paginated list (`/me/following`, `/me/player/recently-played`).
#[derive(Deserialize, Debug)]
// `items` is defaulted, so the derive would otherwise demand `T: Default`; only `Deserialize`
// is actually needed.
#[serde(bound(deserialize = "T: serde::de::Deserialize<'de>"))]
struct TolerantCursorPage<T> {
    #[serde(default)]
    items: Vec<T>,
    #[serde(default)]
    next: Option<String>,
}

/// `/me/following?type=artist`.
#[derive(Deserialize, Debug)]
struct TolerantFollowedArtists {
    artists: TolerantCursorPage<TolerantArtist>,
}

/// An entry of `/me/tracks` (and of the liked-tracks context).
#[derive(Deserialize, Debug, Clone)]
struct TolerantSavedTrack {
    track: TolerantTrack,
    #[serde(default)]
    added_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// An entry of `/me/albums`.
#[derive(Deserialize, Debug, Clone)]
struct TolerantSavedAlbum {
    album: TolerantAlbum,
    #[serde(default)]
    added_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// The owner object of a playlist.
#[derive(Deserialize, Debug, Clone)]
struct TolerantPlaylistOwner {
    #[serde(default)]
    display_name: Option<String>,
    id: UserId<'static>,
}

/// A playlist as returned by `/me/playlists`.
#[derive(Deserialize, Debug, Clone)]
struct TolerantSimplifiedPlaylist {
    id: PlaylistId<'static>,
    #[serde(default)]
    collaborative: bool,
    name: String,
    owner: TolerantPlaylistOwner,
    #[serde(default)]
    snapshot_id: String,
}

impl From<TolerantSimplifiedPlaylist> for Playlist {
    fn from(playlist: TolerantSimplifiedPlaylist) -> Self {
        // Mirrors rspotify's `SimplifiedPlaylist` conversion: the library list carries no
        // description (it is filled in by the playlist detail fetch).
        Self {
            id: playlist.id,
            name: playlist.name,
            collaborative: playlist.collaborative,
            owner: (
                playlist.owner.display_name.unwrap_or_default(),
                playlist.owner.id,
            ),
            desc: String::new(),
            current_folder_id: 0,
            snapshot_id: playlist.snapshot_id,
        }
    }
}

/// The `{ total }` of a track page; the reduced response moves it from `tracks` to `items`.
#[derive(Deserialize, Debug, Clone)]
struct TolerantTrackTotal {
    #[serde(default)]
    total: u64,
}

/// A playlist as returned by `/playlists/{id}`.
#[derive(Deserialize, Debug, Clone)]
struct TolerantPlaylistDetail {
    id: PlaylistId<'static>,
    #[serde(default)]
    collaborative: bool,
    name: String,
    owner: TolerantPlaylistOwner,
    #[serde(default, rename = "description")]
    desc: String,
    #[serde(default)]
    snapshot_id: String,
    /// Classic shape: the first page of tracks.
    #[serde(default)]
    tracks: Option<TolerantTrackTotal>,
    /// Reduced shape: the same page under `items`.
    #[serde(default)]
    items: Option<TolerantTrackTotal>,
}

impl TolerantPlaylistDetail {
    /// The playlist's track total, from either shape. `0` means "unknown", which the pagination
    /// treats as "fetch until an empty page".
    fn track_total(&self) -> usize {
        self.tracks
            .as_ref()
            .or(self.items.as_ref())
            .map_or(0, |page| page.total as usize)
    }
}

impl From<TolerantPlaylistDetail> for Playlist {
    fn from(playlist: TolerantPlaylistDetail) -> Self {
        // Mirrors rspotify's `FullPlaylist` conversion: strip HTML tags from the description.
        let re = regex::Regex::new("(<.*?>|</.*?>)").expect("valid regex");
        let desc =
            html_escape::decode_html_entities(&re.replace_all(&playlist.desc, "")).to_string();

        Self {
            id: playlist.id,
            name: playlist.name,
            collaborative: playlist.collaborative,
            owner: (
                playlist.owner.display_name.unwrap_or_default(),
                playlist.owner.id,
            ),
            desc,
            current_folder_id: 0,
            snapshot_id: playlist.snapshot_id,
        }
    }
}

/// An entry of `/playlists/{id}/items`. The reduced response names the item `item`; the classic
/// shape names it `track`. The item stays raw so its `type` can be checked (episodes are not
/// tracks) and so a track with withheld fields still converts.
#[derive(Deserialize, Debug, Clone)]
struct TolerantPlaylistItem {
    #[serde(default)]
    added_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default, alias = "track")]
    item: Option<serde_json::Value>,
}

impl TolerantPlaylistItem {
    /// Convert into the app's track. Episodes are not tracks, and unplayable tracks and local
    /// files without an id yield `None`, so they are skipped as before.
    fn into_track(self) -> Option<Track> {
        let item = self.item?;
        if item.get("type").and_then(|typ| typ.as_str()) != Some("track") {
            return None;
        }
        serde_json::from_value::<TolerantTrack>(item)
            .ok()?
            .into_track(self.added_at)
    }
}

/// An album as returned by `/albums/{id}`.
#[derive(Deserialize, Debug, Clone)]
struct TolerantAlbumDetail {
    #[serde(default)]
    id: Option<AlbumId<'static>>,
    name: String,
    #[serde(default)]
    release_date: String,
    #[serde(default)]
    artists: Vec<TolerantArtist>,
    #[serde(default)]
    album_type: Option<String>,
    #[serde(default)]
    total_tracks: u64,
    #[serde(default)]
    tracks: Option<TolerantTrackTotal>,
}

impl TolerantAlbumDetail {
    /// The album's track total, from either `total_tracks` or the embedded page.
    fn track_total(&self) -> usize {
        match self.tracks.as_ref() {
            Some(page) if page.total > 0 => page.total as usize,
            _ => self.total_tracks as usize,
        }
    }

    fn into_album(self) -> Option<Album> {
        TolerantAlbum {
            id: self.id,
            name: self.name,
            release_date: self.release_date,
            artists: self.artists,
            album_type: self.album_type,
            // the app's album model has no cover field, so they are not carried further
            images: Vec::new(),
        }
        .into_album()
    }
}

/// An entry of `/me/player/recently-played`.
#[derive(Deserialize, Debug, Clone)]
struct TolerantPlayHistory {
    track: TolerantTrack,
}

/// The device object of `/me/player`.
#[derive(Deserialize, Debug, Clone)]
struct TolerantPlaybackDevice {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    is_active: bool,
    #[serde(default)]
    is_private_session: bool,
    #[serde(default)]
    is_restricted: bool,
    name: String,
    #[serde(default, rename = "type")]
    device_type: Option<rspotify::model::DeviceType>,
    #[serde(default)]
    volume_percent: Option<u32>,
}

impl From<TolerantPlaybackDevice> for rspotify::model::Device {
    fn from(device: TolerantPlaybackDevice) -> Self {
        Self {
            id: device.id,
            is_active: device.is_active,
            is_private_session: device.is_private_session,
            is_restricted: device.is_restricted,
            name: device.name,
            _type: device
                .device_type
                .unwrap_or(rspotify::model::DeviceType::Speaker),
            volume_percent: device.volume_percent,
        }
    }
}

/// The context object of `/me/player`.
#[derive(Deserialize, Debug, Clone)]
struct TolerantPlaybackContext {
    uri: String,
    #[serde(default, rename = "type")]
    context_type: Option<rspotify::model::Type>,
}

/// `/me/player`. `item` is kept raw so a withheld `popularity`/`external_ids` field cannot make
/// the whole response unparseable; [`playable_item_from_value`] then rebuilds a real item.
#[derive(Deserialize, Debug, Clone)]
struct TolerantPlayback {
    #[serde(default)]
    device: Option<TolerantPlaybackDevice>,
    #[serde(default)]
    repeat_state: Option<rspotify::model::RepeatState>,
    #[serde(default)]
    shuffle_state: bool,
    #[serde(default)]
    context: Option<TolerantPlaybackContext>,
    #[serde(default, rename = "progress_ms")]
    progress_ms: Option<u64>,
    #[serde(default)]
    is_playing: bool,
    #[serde(default)]
    item: Option<serde_json::Value>,
    /// The payload's epoch-milliseconds `timestamp`, converted like rspotify does.
    #[serde(default, rename = "timestamp")]
    timestamp_ms: Option<i64>,
    /// Usually present; inferred from `item` when absent.
    #[serde(default)]
    currently_playing_type: Option<rspotify::model::CurrentlyPlayingType>,
}

impl TolerantPlayback {
    fn into_current_playback(self) -> rspotify::model::CurrentPlaybackContext {
        let item = self.item.as_ref().map(playable_item_from_value_or_unknown);
        let currently_playing_type = self.currently_playing_type.unwrap_or_else(|| {
            item.as_ref().map_or(
                rspotify::model::CurrentlyPlayingType::Unknown,
                playing_type_of,
            )
        });
        let context = self.context.map(|context| rspotify::model::Context {
            _type: context
                .context_type
                .unwrap_or_else(|| context_type_from_uri(&context.uri)),
            uri: context.uri,
            href: String::new(),
            external_urls: HashMap::new(),
        });
        let device = self.device.map_or_else(
            || rspotify::model::Device {
                id: None,
                is_active: true,
                is_private_session: false,
                is_restricted: false,
                name: config::get_config().app_config.device.name.clone(),
                _type: rspotify::model::DeviceType::Speaker,
                volume_percent: Some(u32::from(config::get_config().app_config.device.volume)),
            },
            Into::into,
        );

        rspotify::model::CurrentPlaybackContext {
            device,
            repeat_state: self
                .repeat_state
                .unwrap_or(rspotify::model::RepeatState::Off),
            shuffle_state: self.shuffle_state,
            context,
            timestamp: self
                .timestamp_ms
                .and_then(chrono::DateTime::from_timestamp_millis)
                .unwrap_or_else(chrono::Utc::now),
            progress: self
                .progress_ms
                .map(|ms| chrono::Duration::milliseconds(ms as i64)),
            is_playing: self.is_playing,
            item,
            currently_playing_type,
            actions: rspotify::model::Actions::default(),
        }
    }
}

/// `/me/player/queue`.
#[derive(Deserialize, Debug)]
struct TolerantQueue {
    #[serde(default)]
    currently_playing: Option<serde_json::Value>,
    #[serde(default)]
    queue: Vec<serde_json::Value>,
}

impl TolerantQueue {
    fn into_current_user_queue(self) -> rspotify::model::CurrentUserQueue {
        rspotify::model::CurrentUserQueue {
            currently_playing: self
                .currently_playing
                .as_ref()
                .map(playable_item_from_value_or_unknown),
            queue: self
                .queue
                .iter()
                .map(playable_item_from_value_or_unknown)
                .collect(),
        }
    }
}

/// The Web API `Type` implied by a context URI.
fn context_type_from_uri(uri: &str) -> rspotify::model::Type {
    match uri.split(':').nth(1) {
        Some("album") => rspotify::model::Type::Album,
        Some("artist") => rspotify::model::Type::Artist,
        Some("show") => rspotify::model::Type::Show,
        Some("playlist") => rspotify::model::Type::Playlist,
        _ => rspotify::model::Type::Collection,
    }
}

/// Warn once per session that a best-effort artist sub-resource is unavailable.
///
/// A reduced app registration is refused (`top-tracks` answers 403) and the `related-artists`
/// endpoint no longer exists; the artist page reloads on every navigation, so warning each time
/// would turn a stable refusal into a log storm.
fn warn_once_unavailable(once: &std::sync::Once, what: &str, err: &anyhow::Error) {
    once.call_once(|| {
        tracing::warn!("Not showing the artist's {what}, continuing without it: {err:#}");
    });
}

static ARTIST_TOP_TRACKS_UNAVAILABLE: std::sync::Once = std::sync::Once::new();
static ARTIST_ALBUMS_UNAVAILABLE: std::sync::Once = std::sync::Once::new();
static ARTIST_RELATED_ARTISTS_UNAVAILABLE: std::sync::Once = std::sync::Once::new();

/// How a player request is executed.
#[cfg(feature = "streaming")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transport {
    /// Executed on the integrated (librespot) player. No Web API request is made, and the device
    /// is already the active one, so nothing is claimed or transferred.
    Integrated,
    /// Sent to the Web API, exactly as it was before the local transport existed.
    WebApi,
    /// The Web API is tried first; when that call fails, the request is executed on the
    /// integrated player instead, which claims the device with `activate()`.
    WebApiWithLocalFallback,
}

/// Decide how `request` is executed.
///
/// - The integrated device is the active one: `Integrated`, so playback commands keep working
///   while the Web API is rate-limited, and no device ownership changes.
/// - Any other device, or no known one, is active: the Web API, unchanged. Only an explicit
///   "start playback" request that the Web API *failed* to serve falls back to the integrated
///   player; that is the quota-outage case and the only one where the local player claims a
///   device that is not already active.
///
/// `web_api_failed` is the second input of the fallback rule: the first attempt asks with
/// `false`, and the retry decision after a failed attempt asks with `true`.
#[cfg(feature = "streaming")]
fn transport_for(
    request: &PlayerRequest,
    integrated_is_active: bool,
    web_api_failed: bool,
) -> Transport {
    if integrated_is_active {
        return Transport::Integrated;
    }
    let explicit_playback_start = matches!(request, PlayerRequest::StartPlayback(..));
    if explicit_playback_start && web_api_failed {
        Transport::WebApiWithLocalFallback
    } else {
        Transport::WebApi
    }
}

/// Whether a failed Web API command looks like a rate-limit or connectivity problem, i.e. the
/// kind of failure the integrated player can work around by taking over locally.
///
/// rspotify reports both as `ClientError::Http`: transport failures as `HttpError::Client`, and
/// HTTP statuses (the 429 of the rate limiter, or a 5xx) as `HttpError::StatusCode`. Errors from
/// the app's own paced HTTP layer name the condition in their message instead.
#[cfg(feature = "streaming")]
fn is_transient_api_failure(err: &anyhow::Error) -> bool {
    if let Some(rspotify::ClientError::Http(http)) = err.downcast_ref::<rspotify::ClientError>() {
        return match &**http {
            rspotify::http::HttpError::Client(_) => true,
            rspotify::http::HttpError::StatusCode(response) => {
                let status = response.status().as_u16();
                status == 429 || status == 408 || (500..=599).contains(&status)
            }
        };
    }
    let text = format!("{err:#}").to_ascii_lowercase();
    text.contains("rate-limited") || text.contains("rate limited") || text.contains("429")
}

/// Whether a local playback fallback has already been logged at info level in this session;
/// later fallbacks stay at debug level so a long outage cannot flood the log.
#[cfg(feature = "streaming")]
static LOCAL_FALLBACK_LOGGED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// The `CurrentlyPlayingType` implied by a playable item.
fn playing_type_of(item: &rspotify::model::PlayableItem) -> rspotify::model::CurrentlyPlayingType {
    match item {
        rspotify::model::PlayableItem::Track(_) => rspotify::model::CurrentlyPlayingType::Track,
        rspotify::model::PlayableItem::Episode(_) => rspotify::model::CurrentlyPlayingType::Episode,
        rspotify::model::PlayableItem::Unknown(_) => rspotify::model::CurrentlyPlayingType::Unknown,
    }
}

impl AppClient {
    /// Construct a new client
    pub async fn new() -> Result<Self> {
        let configs = config::get_config();
        let auth_config = AuthConfig::new(configs)?;

        let mut api_client = new_api_client()?;
        auth::prompt_for_user_token(&mut api_client, false)
            .await
            .context("authenticate Spotify Web API client")?;
        // Coalescer for Web API volume updates on external devices; spawned under this
        // constructor's tokio runtime context.
        let auth_api = api_client.clone();
        let remote_volume = volume::RemoteVolume::new(move |device, volume_percent| {
            let auth_api = auth_api.clone();
            Box::pin(async move {
                if let Err(err) = auth_api.volume(volume_percent, device.as_deref()).await {
                    tracing::warn!("Failed to apply remote volume: {err:#}");
                }
            })
        });

        #[cfg(feature = "streaming")]
        let (stream_drop_tx, stream_drop_rx) = flume::unbounded();

        Ok(Self {
            spotify: Arc::new(spotify::Spotify::new()),
            // A hung request must not pin its caller forever; the Web API answers well within
            // this bound, and the retry logic below re-issues anything that trips it.
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()?,
            rate_limiter: Arc::new(rate_limit::RateLimiter::new()),
            auth_config,
            api_client,
            remote_volume,

            #[cfg(feature = "streaming")]
            stream_conn: Arc::new(Mutex::new(None)),
            #[cfg(feature = "streaming")]
            stream_drop_tx,
            #[cfg(feature = "streaming")]
            stream_drop_rx,
            #[cfg(feature = "streaming")]
            stream_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            #[cfg(feature = "streaming")]
            stream_claimed_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            update_playback_in_flight: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            context_fetch_in_flight: Arc::new(parking_lot::Mutex::default()),
            #[cfg(feature = "image")]
            image_retries: Arc::new(parking_lot::Mutex::default()),
        })
    }

    /// Apply a volume change directly through the integrated player's local mixer.
    ///
    /// Returns `true` when the active playback device is this process' own integrated client
    /// and the volume was applied via librespot's spirc (no Web API roundtrip); `false` when
    /// the request must fall back to the debounced Web API path.
    #[cfg(feature = "streaming")]
    async fn set_integrated_volume(
        &self,
        playback_device: Option<&str>,
        volume_percent: u8,
    ) -> bool {
        if !self.active_device_is_integrated(playback_device).await {
            return false;
        }
        match self.stream_conn.lock().as_ref() {
            Some(spirc) => {
                // The app uses percentage volume (0-100); librespot's connect state uses 0-65535.
                let volume = (f64::from(volume_percent) / 100.0 * 65_535.0).round() as u16;
                match spirc.set_volume(volume) {
                    Ok(()) => true,
                    Err(err) => {
                        tracing::warn!("Failed to set integrated volume locally: {err:#}");
                        false
                    }
                }
            }
            None => false,
        }
    }

    /// Whether the active playback device is this process' own integrated client with a live
    /// session, meaning local librespot commands apply directly.
    #[cfg(feature = "streaming")]
    async fn active_device_is_integrated(&self, playback_device: Option<&str>) -> bool {
        let Some(device) = playback_device else {
            return false;
        };
        let session = self.spotify.session().await;
        !session.is_invalid() && session.device_id() == device
    }

    /// See [`Self::set_integrated_volume`]; without the streaming feature there is no
    /// integrated player, so every volume change targets the debounced remote path.
    #[cfg(not(feature = "streaming"))]
    // Kept `async` to match the streaming signature so call sites stay feature-independent.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    async fn set_integrated_volume(
        &self,
        _playback_device: Option<&str>,
        _volume_percent: u8,
    ) -> bool {
        false
    }

    /// Seek the integrated player through librespot's local spirc when the active device is
    /// our own; returns `false` so callers fall back to the Web API path.
    #[cfg(feature = "streaming")]
    async fn set_integrated_seek(
        &self,
        playback_device: Option<&str>,
        position: chrono::Duration,
    ) -> bool {
        if !self.active_device_is_integrated(playback_device).await {
            return false;
        }
        let position_ms = u32::try_from(position.num_milliseconds()).unwrap_or(u32::MAX);
        match self.stream_conn.lock().as_ref() {
            Some(spirc) => match spirc.set_position_ms(position_ms) {
                Ok(()) => true,
                Err(err) => {
                    tracing::warn!("Failed to set integrated seek locally: {err:#}");
                    false
                }
            },
            None => false,
        }
    }

    #[cfg(not(feature = "streaming"))]
    // Kept `async` to match the streaming signature so call sites stay feature-independent.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    async fn set_integrated_seek(
        &self,
        _playback_device: Option<&str>,
        _position: chrono::Duration,
    ) -> bool {
        false
    }
    /// Refetch image urls that render paths reported missing, with per-url exponential
    /// backoff; runs on playback polls and manual refreshes.
    #[cfg(feature = "image")]
    pub fn recover_missing_images(&self, state: &SharedState) {
        let pending = std::mem::take(&mut *state.image_fetch_queue.lock());

        for (url, _) in pending {
            // Skip urls that are already satisfied — window prefetch spam and successful
            // concurrent fetches both land here.
            if state.data.read().caches.episode_images.contains(&url) {
                self.image_retries.lock().remove(&url);
                continue;
            }

            let now = std::time::Instant::now();
            let (attempts, due) = *self
                .image_retries
                .lock()
                .entry(url.clone())
                .or_insert((0u8, now));
            if now < due {
                continue;
            }
            let backoff = match attempts {
                0 => std::time::Duration::from_secs(2),
                1 => std::time::Duration::from_secs(5),
                2 => std::time::Duration::from_secs(15),
                _ => std::time::Duration::from_secs(45),
            };
            if let Some(entry) = self.image_retries.lock().get_mut(&url) {
                *entry = ((attempts + 1).min(4), now + backoff);
            }

            let client = self.clone();
            let state = state.clone();
            tokio::spawn(async move {
                if let Err(err) = client.fetch_episode_image(&state, &url).await {
                    tracing::warn!("Image refetch failed for {url}: {err:#}");
                    // Requeue with an incremented attempt; the next drain honors backoff.
                    state.queue_image_fetch(&url);
                } else {
                    client.image_retries.lock().remove(&url);
                    // The cover just entered the cache the render paths draw from. This is the
                    // only way `caches.episode_images` grows outside the request handler, so the
                    // signal has to be raised here and not only by the loop that drains the queue.
                    state.redraw.raise();
                }
            });
        }
    }

    /// Snapshot the currently-playing context + track so a recreated session can
    /// resume the *same* position rather than whatever the cloud reports after the
    /// old device is torn down (which has no retained queue).
    fn remember_playback(state: &SharedState) -> Option<crate::state::Playback> {
        let player = state.player.read();
        let playback = player.playback.as_ref()?;
        let uri = match &playback.item {
            Some(rspotify::model::PlayableItem::Track(track)) => {
                let id = track.id.as_ref()?;
                id.uri()
            }
            Some(rspotify::model::PlayableItem::Episode(episode)) => {
                let id = episode.id.as_ref();
                id.uri()
            }
            _ => return None,
        };
        match player.playing_context_id() {
            // A `Tracks` context can't be restarted by uri, so there is no
            // rememberable playback to re-issue.
            Some(ContextId::Tracks(_)) | None => None,
            Some(context_id) => Some(crate::state::Playback::Context(
                context_id,
                Some(rspotify::model::Offset::Uri(uri)),
            )),
        }
    }

    async fn token(&self) -> Result<String> {
        self.auto_reauth().await?;
        Ok(self
            .get_token()
            .lock()
            .await
            .unwrap()
            .as_ref()
            .context("no access token")?
            .access_token
            .clone())
    }

    /// Initialize the application's playback upon creating a new session or during startup.
    ///
    /// `resume` controls whether playback should be (re)started on the device we connect to.
    /// `remembered` is the context+track that was playing before a session recreation; when
    /// provided it is re-issued at its remembered position instead of inheriting the cloud's
    /// (empty/stale) playback state.
    pub fn initialize_playback(
        &self,
        state: &SharedState,
        resume: bool,
        remembered: Option<crate::state::Playback>,
    ) {
        tokio::task::spawn({
            let client = self.clone();
            let state = state.clone();
            async move {
                // The main playback initialization logic is simple:
                // if there is no playback, connect to an available device
                //
                // However, because it takes time for Spotify server to show up new changes,
                // a retry logic is implemented to ensure the application's state is properly initialized
                let delay = std::time::Duration::from_secs(1);

                for i in 0..5 {
                    // The first attempt is immediate; later ones wait out Spotify's
                    // propagation delay rather than delaying the very first read.
                    if i > 0 {
                        tokio::time::sleep(delay).await;
                    }

                    if let Err(err) = client.retrieve_current_playback(&state, false).await {
                        tracing::error!("Failed to retrieve current playback: {err:#}");
                        continue;
                    }

                    // if playback exists, don't connect to a new device
                    if state.player.read().playback.is_some() {
                        continue;
                    }

                    let id = match client.find_available_device().await {
                        Ok(Some(id)) => Some(Cow::Owned(id)),
                        Ok(None) => None,
                        Err(err) => {
                            tracing::error!("Failed to find an available device: {err:#}");
                            None
                        }
                    };

                    if let Some(id) = id {
                        tracing::info!("Trying to connect to device (id={id}, resume={resume})");
                        if let Err(err) = client.transfer_playback(&id, Some(false)).await {
                            tracing::warn!("Connection failed (device_id={id}): {err:#}");
                        } else {
                            tracing::info!("Connection succeeded (device_id={id})!");
                            if resume {
                                if let Some(remembered) = remembered.as_ref() {
                                    // Re-issue the remembered context at its remembered track: the
                                    // fresh device has no retained queue after the old one was
                                    // torn down, so resuming it would restart a different track.
                                    if let Err(err) = client
                                        .start_playback(remembered.clone(), Some(id.as_ref()))
                                        .await
                                    {
                                        tracing::warn!(
                                            "Failed to resume remembered playback after reconnect: {err:#}"
                                        );
                                    }
                                } else if let Err(err) =
                                    client.resume_playback(Some(id.as_ref()), None).await
                                {
                                    tracing::warn!(
                                        "Failed to resume playback after reconnect: {err:#}"
                                    );
                                }
                            }
                            // upon new connection, reset the buffered playback
                            state.player.write().buffered_playback = None;
                            client.update_playback(&state);
                            break;
                        }
                    }
                }
            }
        });
    }

    /// Create a new client session
    pub async fn new_session(&self, state: Option<&SharedState>, reauth: bool) -> Result<()> {
        // Capture whether playback was active *before* tearing down any existing streaming
        // connection. Shutting down the old `librespot` spirc pauses playback Spotify-side
        // (and a broken session leaves it paused too), so we use this to resume on the new
        // device rather than reconnecting in a paused state.
        let was_playing = state.is_some_and(|state| {
            state
                .player
                .read()
                .buffered_playback
                .as_ref()
                .is_some_and(|p| p.is_playing)
        });

        // Snapshot the currently-playing context + track so the recreated device resumes the
        // *same* position rather than inheriting the cloud's (empty/stale) playback state.
        let remembered = state.and_then(Self::remember_playback);

        let session = self.auth_config.session();
        let creds = auth::get_creds(&self.auth_config, reauth, true).context("get credentials")?;
        self.spotify.set_session(session.clone()).await;

        #[allow(unused_mut)]
        let mut connected = false;

        #[cfg(feature = "streaming")]
        if let Some(state) = state {
            if state.is_streaming_enabled() {
                self.new_streaming_connection(state.clone(), session.clone(), creds.clone())
                    .await
                    .context("new streaming connection")?;
                connected = true;
            }
        }

        if !connected {
            // if session is not connected (triggered by `new_streaming_connection`), connect to the session
            session
                .connect(creds, true)
                .await
                .context("connect to a session")?;
        }

        tracing::info!("Used a new session for Spotify client.");

        if let Err(err) = self.refresh_token().await {
            tracing::warn!("Failed to refresh auth token after creating a new session: {err:#}");
        }

        if let Some(state) = state {
            // reset the application's caches
            state.data.write().caches = MemoryCaches::new();
            self.initialize_playback(state, was_playing, remembered);
            // A reconnect empties every cache and switches device, so every page has to be
            // rebuilt; `initialize_playback` then refills the playback on its own schedule.
            state.redraw.raise();
        }

        Ok(())
    }

    /// Check if the current session is valid and if invalid, create a new session
    pub async fn check_valid_session(&self, state: &SharedState) -> Result<()> {
        if self.spotify.session().await.is_invalid() {
            tracing::info!("Client's current session is invalid, creating a new session...");
            self.new_session(Some(state), false)
                .await
                .context("create new client session")?;
        }
        Ok(())
    }

    /// Create a new streaming connection
    #[cfg(feature = "streaming")]
    pub async fn new_streaming_connection(
        &self,
        state: SharedState,
        session: librespot_core::Session,
        creds: librespot_core::authentication::Credentials,
    ) -> Result<()> {
        // Deliberately replacing the connection: advance the epoch so the old
        // spirc watchdog task (which captured a prior epoch) does not treat
        // this teardown as an unexpected link drop.
        self.advance_stream_epoch();

        let new_conn =
            crate::streaming::new_connection(self.clone(), state, session, creds).await?;
        let mut stream_conn = self.stream_conn.lock();
        // shutdown old streaming connection and replace it with a new connection
        if let Some(conn) = stream_conn.as_ref() {
            if let Err(err) = conn.shutdown() {
                log::error!("Failed to shutdown old streaming connection: {err:#}");
            }
        }
        *stream_conn = Some(new_conn);
        Ok(())
    }

    /// Pause the integrated streaming client, if a connection exists.
    ///
    /// Returns `true` if a streaming connection was present and the pause
    /// command was issued. Used to suppress Spotify's auto-resume of the
    /// previous session on startup when `pause_on_startup` is enabled.
    #[cfg(feature = "streaming")]
    pub fn pause_streaming_on_startup(&self) -> bool {
        match self.stream_conn.lock().as_ref() {
            Some(spirc) => {
                if let Err(err) = spirc.pause() {
                    tracing::warn!("Failed to pause integrated client on startup: {err:#}");
                }
                true
            }
            None => false,
        }
    }

    /// Run a synchronous command on the integrated (librespot) connection.
    ///
    /// Every `Spirc` command is a synchronous send on an unbounded channel, so the lock is never
    /// held across an `await`. Returns `false` when there is no connection or the command could
    /// not be queued (channel closed).
    #[cfg(feature = "streaming")]
    fn with_spirc(&self, f: impl FnOnce(&librespot_connect::Spirc) -> bool) -> bool {
        match self.stream_conn.lock().as_ref() {
            Some(spirc) => f(spirc),
            None => false,
        }
    }

    /// Claim the integrated device as the active one, at most once per session.
    ///
    /// Requests are routed to the local player only while the Web API reports this device as the
    /// active one, and librespot normally learns that from the same connect-state updates (a
    /// transfer addressed to it activates it). The claim closes the remaining gap - notably an
    /// app restart while this device was already the active one, where librespot would otherwise
    /// ignore player commands - and is issued at most once per session because librespot logs a
    /// warning when it is asked to activate while it already considers itself active.
    ///
    /// `false` when there is no librespot connection to command.
    #[cfg(feature = "streaming")]
    fn ensure_local_device_claimed(&self) -> bool {
        if self
            .stream_claimed_active
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return true;
        }
        if !self.with_spirc(|spirc| spirc.activate().is_ok()) {
            return false;
        }
        self.stream_claimed_active
            .store(true, std::sync::atomic::Ordering::Release);
        true
    }

    /// Apply a local (spirc) change to the player state: the rich `playback` (item/context/
    /// progress) and the buffered metadata that `current_playback()` overlays for display.
    #[cfg(feature = "streaming")]
    fn apply_local_playback_change(
        state: &SharedState,
        playback: impl FnOnce(&mut rspotify::model::CurrentPlaybackContext),
        metadata: impl FnOnce(&mut PlaybackMetadata),
    ) {
        let mut player = state.player.write();
        if let Some(p) = player.playback.as_mut() {
            playback(p);
        }
        if let Some(m) = player.buffered_playback.as_mut() {
            metadata(m);
        }
        player.playback_last_updated_time = Some(std::time::Instant::now());
    }

    /// Publish a locally started playback into the player state.
    ///
    /// The Web API cannot refresh `/me/player` while it is rate-limited, so the pane would
    /// otherwise show "No playback found" while audio plays. Everything here is derived from
    /// local truth (the requested item, the caches, the configured device).
    #[cfg(feature = "streaming")]
    fn publish_local_playback(
        state: &SharedState,
        device_id: &str,
        item_uri: Option<&str>,
        context: Option<(String, rspotify::model::Type)>,
        shuffle: bool,
    ) {
        let configs = config::get_config();
        let device_name = configs.app_config.device.name.clone();
        let volume = u32::from(std::cmp::min(configs.app_config.device.volume, 100));

        let item = item_uri.and_then(|uri| playback_item_for_uri(state, uri));
        let currently_playing_type = item.as_ref().map_or(
            rspotify::model::CurrentlyPlayingType::Unknown,
            playing_type_of,
        );

        let repeat_state = state
            .player
            .read()
            .buffered_playback
            .as_ref()
            .map_or(rspotify::model::RepeatState::Off, |buffered| {
                buffered.repeat_state
            });

        let playback = rspotify::model::CurrentPlaybackContext {
            device: rspotify::model::Device {
                id: Some(device_id.to_string()),
                is_active: true,
                is_private_session: false,
                is_restricted: false,
                name: device_name.clone(),
                // The configured device type is a librespot string; the pane only displays the
                // device name, so the speaker default is used here.
                _type: rspotify::model::DeviceType::Speaker,
                volume_percent: Some(volume),
            },
            repeat_state,
            shuffle_state: shuffle,
            context: context.map(|(uri, context_type)| rspotify::model::Context {
                uri,
                href: String::new(),
                external_urls: HashMap::new(),
                _type: context_type,
            }),
            timestamp: chrono::Utc::now(),
            progress: Some(chrono::Duration::zero()),
            is_playing: true,
            item,
            currently_playing_type,
            actions: rspotify::model::Actions::default(),
        };

        let mut player = state.player.write();
        player.playback = Some(playback);
        player.playback_last_updated_time = Some(std::time::Instant::now());
        player.buffered_playback = Some(PlaybackMetadata {
            device_name,
            device_id: Some(device_id.to_string()),
            volume: Some(volume),
            // the playback was just started on the local player
            is_playing: true,
            repeat_state,
            shuffle_state: shuffle,
            mute_state: None,
        });
    }

    /// Reconcile the pane with what the integrated (librespot) player is doing.
    ///
    /// librespot is the local truth for playback: the Web API's view can lag behind it, and it
    /// is simply unavailable while rate-limited. Called from the player-event loop, which is a
    /// state change of its own, so it never logs per poll - a failure to resolve the item is
    /// logged once per event at debug level.
    #[cfg(feature = "streaming")]
    pub(crate) fn reconcile_local_playback_item(
        state: &SharedState,
        uri: &str,
        position: Option<chrono::Duration>,
    ) {
        let Some(item) = playback_item_for_uri(state, uri) else {
            tracing::debug!("No cached metadata for locally playing item {uri}");
            return;
        };
        let currently_playing_type = playing_type_of(&item);

        let mut player = state.player.write();
        if let Some(playback) = player.playback.as_mut() {
            let shows_item = playback
                .item
                .as_ref()
                .and_then(rspotify::model::PlayableItem::id)
                .is_some_and(|id| id.uri() == uri);
            if !shows_item {
                playback.item = Some(item);
                playback.currently_playing_type = currently_playing_type;
            }
            playback.is_playing = true;
            // The event carries the position librespot is playing from (a track start, a
            // resume after a pause, or a corrected position after a seek), so the progress
            // display follows the audio rather than the last Web API read.
            playback.progress = Some(position.unwrap_or_else(chrono::Duration::zero));
        }
        if let Some(metadata) = player.buffered_playback.as_mut() {
            metadata.is_playing = true;
        }
        player.playback_last_updated_time = Some(std::time::Instant::now());
    }

    /// Mark the pane's playback state as paused, from librespot's `Paused` event, so the
    /// progress display stops advancing while the Web API's view is stale or unavailable.
    #[cfg(feature = "streaming")]
    pub(crate) fn reconcile_local_playback_paused(state: &SharedState) {
        Self::apply_local_playback_change(
            state,
            |playback| playback.is_playing = false,
            |metadata| metadata.is_playing = false,
        );
    }

    /// Serve `request` on the integrated player.
    ///
    /// Only called while this process' device is the active one (`transport_for`), so the local
    /// player owns the playback state and no device is transferred. Returns `false` when the
    /// local player cannot serve the request - no librespot connection, or a context whose
    /// tracks are not cached locally - which leaves the request to the Web API.
    #[cfg(feature = "streaming")]
    async fn handle_local_player_request(
        &self,
        state: &SharedState,
        request: &PlayerRequest,
        buffered: Option<&PlaybackMetadata>,
    ) -> bool {
        if !self.ensure_local_device_claimed() {
            return false;
        }

        match request {
            PlayerRequest::StartPlayback(playback, shuffle) => {
                let shuffle = shuffle
                    .or_else(|| buffered.map(|playback| playback.shuffle_state))
                    .unwrap_or(false);
                let Some(load) = local_load(state, playback, shuffle) else {
                    return false;
                };
                if !self.with_spirc(|spirc| spirc.load(load.request).is_ok()) {
                    return false;
                }

                let device_id = self.spotify.session().await.device_id().to_string();
                Self::publish_local_playback(
                    state,
                    &device_id,
                    load.item_uri.as_deref(),
                    load.context,
                    shuffle,
                );
                true
            }
            PlayerRequest::Pause => {
                if !self.with_spirc(|spirc| spirc.pause().is_ok()) {
                    return false;
                }
                Self::apply_local_playback_change(
                    state,
                    |p| p.is_playing = false,
                    |m| m.is_playing = false,
                );
                true
            }
            PlayerRequest::Resume => {
                if !self.with_spirc(|spirc| spirc.play().is_ok()) {
                    return false;
                }
                Self::apply_local_playback_change(
                    state,
                    |p| p.is_playing = true,
                    |m| m.is_playing = true,
                );
                true
            }
            PlayerRequest::ResumePause => {
                if !self.with_spirc(|spirc| spirc.play_pause().is_ok()) {
                    return false;
                }
                let now_playing = !buffered.is_some_and(|playback| playback.is_playing);
                Self::apply_local_playback_change(
                    state,
                    |p| p.is_playing = now_playing,
                    |m| m.is_playing = now_playing,
                );
                true
            }
            PlayerRequest::NextTrack => {
                if !self.with_spirc(|spirc| spirc.next().is_ok()) {
                    return false;
                }
                // The new track is announced by the librespot player-event loop, which
                // re-resolves the item; only the progress is reset here.
                Self::apply_local_playback_change(
                    state,
                    |p| p.progress = Some(chrono::Duration::zero()),
                    |_| {},
                );
                true
            }
            PlayerRequest::PreviousTrack => {
                if !self.with_spirc(|spirc| spirc.prev().is_ok()) {
                    return false;
                }
                Self::apply_local_playback_change(
                    state,
                    |p| p.progress = Some(chrono::Duration::zero()),
                    |_| {},
                );
                true
            }
            PlayerRequest::SeekTrack(position) => {
                let position_ms = u32::try_from(position.num_milliseconds()).unwrap_or(u32::MAX);
                if !self.with_spirc(|spirc| spirc.set_position_ms(position_ms).is_ok()) {
                    return false;
                }
                Self::apply_local_playback_change(state, |p| p.progress = Some(*position), |_| {});
                true
            }
            PlayerRequest::Repeat => {
                let current = buffered.map_or(rspotify::model::RepeatState::Off, |playback| {
                    playback.repeat_state
                });
                let next = match current {
                    rspotify::model::RepeatState::Off => rspotify::model::RepeatState::Track,
                    rspotify::model::RepeatState::Track => rspotify::model::RepeatState::Context,
                    rspotify::model::RepeatState::Context => rspotify::model::RepeatState::Off,
                };
                let (repeat_context, repeat_track) = match next {
                    rspotify::model::RepeatState::Off => (false, false),
                    rspotify::model::RepeatState::Track => (false, true),
                    rspotify::model::RepeatState::Context => (true, false),
                };
                if !self.with_spirc(|spirc| {
                    spirc.repeat(repeat_context).is_ok() && spirc.repeat_track(repeat_track).is_ok()
                }) {
                    return false;
                }
                Self::apply_local_playback_change(
                    state,
                    |p| p.repeat_state = next,
                    |m| m.repeat_state = next,
                );
                true
            }
            PlayerRequest::Shuffle => {
                let next = !buffered.is_some_and(|playback| playback.shuffle_state);
                if !self.with_spirc(|spirc| spirc.shuffle(next).is_ok()) {
                    return false;
                }
                Self::apply_local_playback_change(
                    state,
                    |p| p.shuffle_state = next,
                    |m| m.shuffle_state = next,
                );
                true
            }
            PlayerRequest::Volume(volume) => {
                if !self.with_spirc(|spirc| spirc.set_volume(volume_to_librespot(*volume)).is_ok())
                {
                    return false;
                }
                Self::apply_local_playback_change(
                    state,
                    |p| p.device.volume_percent = Some(u32::from(*volume)),
                    |m| {
                        m.volume = Some(u32::from(*volume));
                        m.mute_state = None;
                    },
                );
                *state.volume_target.lock() = None;
                true
            }
            PlayerRequest::ToggleMute => {
                let (volume_percent, new_mute_state) = match buffered.and_then(|p| p.mute_state) {
                    None => (0, Some(buffered.and_then(|p| p.volume).unwrap_or_default())),
                    Some(restore) => (u8::try_from(restore).unwrap_or(u8::MAX), None),
                };
                if !self.with_spirc(|spirc| {
                    spirc
                        .set_volume(volume_to_librespot(volume_percent))
                        .is_ok()
                }) {
                    return false;
                }
                Self::apply_local_playback_change(
                    state,
                    |p| p.device.volume_percent = Some(u32::from(volume_percent)),
                    |m| {
                        m.volume = Some(u32::from(volume_percent));
                        m.mute_state = new_mute_state;
                    },
                );
                true
            }
            // `TransferPlayback` keeps the Web API behaviour.
            PlayerRequest::TransferPlayback(..) => false,
        }
    }

    /// Execute a playback start on the integrated player after the Web API failed to serve it.
    ///
    /// This is the quota-outage path: the command could not be delivered, so the local player
    /// takes over instead. Another device may own playback at that moment (or none is known),
    /// hence the device is claimed unconditionally here - unlike the already-active case, where
    /// the claim happens at most once per session. Returns `false` when the local player cannot
    /// serve the request, leaving the original error to the caller.
    #[cfg(feature = "streaming")]
    async fn start_local_playback(
        &self,
        state: &SharedState,
        request: &PlayerRequest,
        buffered: Option<&PlaybackMetadata>,
        err: &anyhow::Error,
    ) -> bool {
        let PlayerRequest::StartPlayback(playback, shuffle) = request else {
            return false;
        };
        let shuffle = shuffle
            .or_else(|| buffered.map(|playback| playback.shuffle_state))
            .unwrap_or(false);
        let Some(load) = local_load(state, playback, shuffle) else {
            return false;
        };
        if !self.with_spirc(|spirc| spirc.activate().is_ok() && spirc.load(load.request).is_ok()) {
            return false;
        }
        self.stream_claimed_active
            .store(true, std::sync::atomic::Ordering::Release);

        // One info log per session, then debug: an outage lasting longer than the session must
        // not flood the log.
        if LOCAL_FALLBACK_LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            tracing::debug!(
                "Web API playback start failed again, started on the integrated device instead: {err:#}"
            );
        } else {
            tracing::info!(
                "Web API playback start failed, falling back to the integrated device (which \
                 becomes the active device): {err:#}"
            );
        }

        let device_id = self.spotify.session().await.device_id().to_string();
        Self::publish_local_playback(
            state,
            &device_id,
            load.item_uri.as_deref(),
            load.context,
            shuffle,
        );
        true
    }

    /// Handle a player request, return a new playback metadata on success
    pub async fn handle_player_request(
        &self,
        request: PlayerRequest,
        mut playback: Option<PlaybackMetadata>,
    ) -> Result<Option<PlaybackMetadata>> {
        // handle requests that don't require an active playback
        match request {
            PlayerRequest::TransferPlayback(device_id, force_play) => {
                // `TransferPlayback` needs to be handled separately from other player requests
                // because `TransferPlayback` doesn't require an active playback
                self.transfer_playback(&device_id, Some(force_play)).await?;
                tracing::info!("Transferred playback to device with id={}", device_id);
                return Ok(None);
            }
            PlayerRequest::StartPlayback(p, shuffle) => {
                // Set the playback's shuffle state if specified in the request
                if let (Some(shuffle), Some(playback)) = (shuffle, playback.as_mut()) {
                    playback.shuffle_state = shuffle;
                }
                let device_id = playback.as_ref().and_then(|p| p.device_id.as_deref());
                self.start_playback(p, device_id).await?;
                // For some reasons, when starting a new playback, the integrated `spotify_player`
                // client doesn't respect the initial shuffle state, so we need to manually update the state
                if let Some(ref playback) = playback {
                    self.shuffle(playback.shuffle_state, device_id).await?;
                }
                return Ok(None);
            }
            _ => {}
        }

        let mut playback = playback.context("no playback found")?;
        let device_id = playback.device_id.as_deref();

        match request {
            PlayerRequest::NextTrack => self.next_track(device_id).await?,
            PlayerRequest::PreviousTrack => self.previous_track(device_id).await?,
            PlayerRequest::Resume => {
                if !playback.is_playing {
                    self.resume_playback(device_id, None).await?;
                    playback.is_playing = true;
                }
            }

            PlayerRequest::Pause => {
                if playback.is_playing {
                    self.pause_playback(device_id).await?;
                    playback.is_playing = false;
                }
            }
            PlayerRequest::ResumePause => {
                if playback.is_playing {
                    self.pause_playback(device_id).await?;
                } else {
                    self.resume_playback(device_id, None).await?;
                }
                playback.is_playing = !playback.is_playing;
            }
            PlayerRequest::SeekTrack(position_ms) => {
                if !self
                    .set_integrated_seek(playback.device_id.as_deref(), position_ms)
                    .await
                {
                    self.seek_track(position_ms, device_id).await?;
                }
            }
            PlayerRequest::Repeat => {
                let next_repeat_state = match playback.repeat_state {
                    rspotify::model::RepeatState::Off => rspotify::model::RepeatState::Track,
                    rspotify::model::RepeatState::Track => rspotify::model::RepeatState::Context,
                    rspotify::model::RepeatState::Context => rspotify::model::RepeatState::Off,
                };

                self.repeat(next_repeat_state, device_id).await?;

                playback.repeat_state = next_repeat_state;
            }
            PlayerRequest::Shuffle => {
                self.shuffle(!playback.shuffle_state, device_id).await?;

                playback.shuffle_state = !playback.shuffle_state;
            }
            PlayerRequest::Volume(volume) => {
                if !self
                    .set_integrated_volume(playback.device_id.as_deref(), volume)
                    .await
                {
                    self.remote_volume
                        .push(playback.device_id.clone(), volume)
                        .await;
                }

                playback.volume = Some(u32::from(volume));
                playback.mute_state = None;
            }
            PlayerRequest::ToggleMute => {
                let new_mute_state = match playback.mute_state {
                    None => {
                        if !self
                            .set_integrated_volume(playback.device_id.as_deref(), 0)
                            .await
                        {
                            self.remote_volume.push(playback.device_id.clone(), 0).await;
                        }
                        Some(playback.volume.unwrap_or_default())
                    }
                    Some(restore) => {
                        let percent = u8::try_from(restore).unwrap_or(u8::MAX);
                        if !self
                            .set_integrated_volume(playback.device_id.as_deref(), percent)
                            .await
                        {
                            self.remote_volume
                                .push(playback.device_id.clone(), percent)
                                .await;
                        }
                        None
                    }
                };

                playback.mute_state = new_mute_state;
            }
            PlayerRequest::StartPlayback(..) => {
                anyhow::bail!("`StartPlayback` should be handled earlier")
            }
            PlayerRequest::TransferPlayback(..) => {
                anyhow::bail!("`TransferPlayback` should be handled earlier")
            }
        }

        Ok(Some(playback))
    }

    /// Handle a `GetContext` request, dropping a duplicate whose identical fetch is already
    /// running.
    ///
    /// The page-change handler re-issues `GetContext` every few seconds while the context is
    /// uncached. While a fetch is in flight that duplicate would start a second fetch which
    /// then resumes on the same shared 429 cooldown and re-429s in waves; skip it instead —
    /// the in-flight fetch publishes the result.
    async fn handle_get_context(&self, state: &SharedState, context: ContextId) -> Result<()> {
        let uri = context.uri();

        // Take the guard in a temporary so it is dropped before the fetch's first await.
        if !self.context_fetch_in_flight.lock().insert(uri.clone()) {
            tracing::debug!(
                "GetContext for {uri} is already in flight; skipping duplicate request"
            );
            return Ok(());
        }

        let result = self.get_context(state, context).await;

        // Remove on both paths: the next page-change tick must be able to retry after a failure.
        self.context_fetch_in_flight.lock().remove(&uri);
        result
    }

    async fn get_context(&self, state: &SharedState, context: ContextId) -> Result<()> {
        let uri = context.uri();
        let is_liked = uri == USER_LIKED_TRACKS_URI;
        // Liked tracks are kept in sync through `user_data.saved_tracks` and are always
        // refreshed; every other context can be served from the on-disk cache.
        let cache_miss = !is_liked && !state.data.read().caches.context.contains_key(&uri);
        let cache_folder = config::get_config().cache_folder.clone();
        let mut needs_fetch = false;

        if cache_miss {
            match load_context_from_file_cache(&cache_folder, &uri) {
                // Fresh and complete: render from disk without a request.
                Some(cached) if cached.is_fresh() && !cached.context.is_incomplete() => {
                    state.data.write().caches.context.insert(
                        uri.clone(),
                        cached.context,
                        *TTL_CACHE_DURATION,
                    );
                }
                // Stale or partial: render from disk now, replace once the refresh lands.
                Some(cached) => {
                    state.data.write().caches.context.insert(
                        uri.clone(),
                        cached.context,
                        *TTL_CACHE_DURATION,
                    );
                    needs_fetch = true;
                }
                None => needs_fetch = true,
            }
        }

        if is_liked {
            // Seed the page from the disk-cached library so it paints before the
            // always-refreshed paginated fetch lands; `/me/tracks` returns newest-first.
            if !state.data.read().caches.context.contains_key(&uri) {
                let mut tracks: Vec<Track> = state
                    .data
                    .read()
                    .user_data
                    .saved_tracks
                    .values()
                    .cloned()
                    .collect();
                // Many tracks share `added_at` (bulk library imports), and `saved_tracks` is a
                // `HashMap`, so a tiebreak on track id is required for the seed order to be
                // identical across runs.
                tracks.sort_by_key(|t| (std::cmp::Reverse(t.added_at), t.id.uri()));
                if !tracks.is_empty() {
                    state.data.write().caches.context.insert(
                        uri.clone(),
                        Context::Tracks {
                            tracks,
                            desc: "User's liked tracks".to_string(),
                        },
                        *TTL_CACHE_DURATION,
                    );
                }
            }
            needs_fetch = true; // liked tracks stay always-refreshed
        }

        if needs_fetch {
            let ctx = match context {
                ContextId::Playlist(playlist_id) => self.playlist_context(playlist_id).await?,
                ContextId::Album(album_id) => self.album_context(album_id).await?,
                ContextId::Artist(artist_id) => self.artist_context(artist_id).await?,
                ContextId::Tracks(tracks_id) => match tracks_id.uri.as_str() {
                    USER_TOP_TRACKS_URI => {
                        let Some(tracks) =
                            self.top_tracks(rate_limit::Priority::Background).await?
                        else {
                            // Deferred while the client is backed off; the seeded/previous
                            // snapshot stays in place and a later trigger retries it.
                            tracing::debug!(
                                "deferring the top-tracks refresh: client is backed off"
                            );
                            return Ok(());
                        };
                        Context::Tracks {
                            tracks,
                            desc: "User's top tracks".to_string(),
                        }
                    }
                    USER_RECENTLY_PLAYED_TRACKS_URI => {
                        let Some(tracks) = self
                            .recently_played_tracks(rate_limit::Priority::Background)
                            .await?
                        else {
                            tracing::debug!(
                                "deferring the recently-played refresh: client is backed off"
                            );
                            return Ok(());
                        };
                        Context::Tracks {
                            tracks,
                            desc: "User's recently played tracks".to_string(),
                        }
                    }
                    USER_LIKED_TRACKS_URI => {
                        let Some(tracks) =
                            self.saved_tracks(rate_limit::Priority::Background).await?
                        else {
                            // Keep the seed written above; refresh later.
                            tracing::debug!(
                                "deferring the liked-tracks refresh: client is backed off"
                            );
                            return Ok(());
                        };
                        let tracks_hm = tracks
                            .iter()
                            .map(|t| (t.id.uri(), t.clone()))
                            .collect::<HashMap<_, _>>();
                        store_data_into_file_cache(
                            FileCacheKey::SavedTracks,
                            &config::get_config().cache_folder,
                            &tracks_hm,
                        )
                        .context("store user's saved tracks into the cache folder")?;
                        state.data.write().user_data.saved_tracks = tracks_hm;
                        Context::Tracks {
                            tracks,
                            desc: "User's liked tracks".to_string(),
                        }
                    }
                    u if u.starts_with("radio:") => Context::Tracks {
                        tracks: self.radio_tracks(u["radio:".len()..].to_string()).await?,
                        desc: tracks_id.kind.clone(),
                    },
                    uri => anyhow::bail!("unsupported Tracks context: {uri}"),
                },
                ContextId::Show(show_id) => {
                    let prefetch_id = show_id.clone_static();
                    let ctx = self.show_context(show_id).await?;
                    if let Context::Show {
                        episodes,
                        total_episodes,
                        ..
                    } = &ctx
                    {
                        if episodes.len() < *total_episodes {
                            self.spawn_show_episodes_prefetch(
                                state.clone(),
                                prefetch_id,
                                uri.clone(),
                                episodes.len(),
                                *total_episodes,
                            );
                        }
                    }
                    ctx
                }
            };

            state
                .data
                .write()
                .caches
                .context
                .insert(uri.clone(), ctx.clone(), *TTL_CACHE_DURATION);
            if !is_liked {
                // Liked tracks persist through `SavedTracks_cache.json` instead.
                if let Err(err) = store_context_in_file_cache(&cache_folder, &uri, &ctx) {
                    tracing::warn!("Failed to store context {uri} in the file cache: {err:#}");
                }
            }

            // A background refresh may have shrunk the list the user is looking at.
            let mut view = state.view.lock();
            let page = view.current_page_mut();
            if matches!(page, PageState::Context { id: Some(page_id), .. } if page_id.uri() == uri)
            {
                page.clamp_context_selection(&ctx);
            }
        }

        Ok(())
    }

    /// Handle a client request
    pub(crate) async fn handle_request(
        &self,
        state: &SharedState,
        request: ClientRequest,
    ) -> Result<()> {
        let timer = tokio::time::Instant::now();

        match request {
            ClientRequest::GetBrowseCategories => {
                let categories = self.browse_categories().await?;
                state.data.write().browse.categories = categories;
            }
            ClientRequest::GetBrowseCategoryPlaylists(category) => {
                let playlists = self.browse_category_playlists(&category.id).await?;
                state
                    .data
                    .write()
                    .browse
                    .category_playlists
                    .insert(category.id, playlists);
            }
            ClientRequest::GetLyrics { track_id } => {
                let uri = track_id.uri();
                if !state.data.read().caches.lyrics.contains_key(&uri) {
                    let lyrics = self.lyrics(track_id).await?;
                    state
                        .data
                        .write()
                        .caches
                        .lyrics
                        .insert(uri, lyrics, *TTL_CACHE_DURATION);
                }
            }
            #[cfg(feature = "streaming")]
            ClientRequest::RestartIntegratedClient => {
                self.new_session(Some(state), false).await?;
            }
            ClientRequest::GetCurrentUser => {
                let user = self.current_user().await?;
                state.data.write().user_data.user = Some(user);
            }
            ClientRequest::Player(request) => {
                let is_volume = matches!(&request, PlayerRequest::Volume(_));
                let seek_to = match &request {
                    PlayerRequest::SeekTrack(position) => Some(*position),
                    _ => None,
                };
                let buffered = state.player.read().buffered_playback.clone();

                // The integrated device is served by its own transport, so those commands never
                // reach the Web API; every other device keeps the Web API path.
                // (`transport_for` documents the exact rules.)
                #[cfg(feature = "streaming")]
                let active_device = buffered.as_ref().and_then(|p| p.device_id.as_deref());
                #[cfg(feature = "streaming")]
                let integrated_is_active = self.active_device_is_integrated(active_device).await;

                #[cfg(feature = "streaming")]
                if transport_for(&request, integrated_is_active, false) == Transport::Integrated
                    && self
                        .handle_local_player_request(state, &request, buffered.as_ref())
                        .await
                {
                    return Ok(());
                }

                // An explicit playback start that the Web API cannot serve may still run on the
                // integrated player, so its arguments are kept for that path.
                #[cfg(feature = "streaming")]
                let local_fallback = (transport_for(&request, integrated_is_active, true)
                    == Transport::WebApiWithLocalFallback)
                    .then(|| (request.clone(), buffered.clone()));

                let web_api_result = self.handle_player_request(request, buffered).await;
                let playback = match web_api_result {
                    Ok(playback) => playback,
                    Err(err) => {
                        // Quota outage or connectivity: the command still happens, on the
                        // integrated device. Anything else keeps failing exactly as before.
                        #[cfg(feature = "streaming")]
                        if let Some((request, buffered)) = local_fallback.as_ref() {
                            if is_transient_api_failure(&err)
                                && self
                                    .start_local_playback(state, request, buffered.as_ref(), &err)
                                    .await
                            {
                                return Ok(());
                            }
                        }
                        return Err(err);
                    }
                };

                state.player.write().buffered_playback = playback;
                if is_volume {
                    // The commanded target is now applied locally; drop the pending rebase
                    // so the next scroll tick computes from fresh state.
                    *state.volume_target.lock() = None;
                }
                if let Some(position) = seek_to {
                    // Reflect a seek immediately in local state so the progress bar jumps right
                    // away; the background `update_playback()` poll reconciles the exact
                    // server-side position shortly after.
                    let mut player = state.player.write();
                    if let Some(p) = player.playback.as_mut() {
                        p.progress = Some(position);
                    }
                    player.playback_last_updated_time = Some(std::time::Instant::now());
                }
                self.update_playback(state);
            }
            ClientRequest::GetCurrentPlayback => {
                self.retrieve_current_playback(state, true).await?;
                #[cfg(feature = "image")]
                self.recover_missing_images(state);
            }
            ClientRequest::GetDevices => {
                #[allow(unused_mut)]
                let mut devices: Vec<Device> = self
                    .available_devices()
                    .await?
                    .into_iter()
                    .filter_map(Device::try_from_device)
                    .collect();

                #[cfg(feature = "streaming")]
                self.ensure_integrated_device(&mut devices).await;

                state.player.write().devices = devices;
            }
            ClientRequest::GetUserPlaylists => {
                let Some(playlists) = self
                    .user_playlists(rate_limit::Priority::Background)
                    .await?
                else {
                    tracing::debug!("deferring the playlists refresh: client is backed off");
                    return Ok(());
                };
                let node = state.data.read().user_data.playlist_folder_node.clone();
                let playlists = if let Some(node) = node.filter(|n| !n.children.is_empty()) {
                    crate::playlist_folders::structurize(playlists, &node.children)
                } else {
                    playlists
                        .into_iter()
                        .map(PlaylistFolderItem::Playlist)
                        .collect()
                };
                store_data_into_file_cache(
                    FileCacheKey::Playlists,
                    &config::get_config().cache_folder,
                    &playlists,
                )
                .context("store user's playlists into the cache folder")?;
                state.data.write().user_data.playlists = playlists;
            }
            ClientRequest::GetUserFollowedArtists => {
                let Some(artists) = self
                    .followed_artists(rate_limit::Priority::Background)
                    .await?
                else {
                    tracing::debug!("deferring the followed artists refresh: client is backed off");
                    return Ok(());
                };
                store_data_into_file_cache(
                    FileCacheKey::FollowedArtists,
                    &config::get_config().cache_folder,
                    &artists,
                )
                .context("store user's followed artists into the cache folder")?;
                state.data.write().user_data.followed_artists = artists;
            }
            ClientRequest::GetUserSavedAlbums => {
                let Some(albums) = self.saved_albums(rate_limit::Priority::Background).await?
                else {
                    tracing::debug!("deferring the saved albums refresh: client is backed off");
                    return Ok(());
                };
                store_data_into_file_cache(
                    FileCacheKey::SavedAlbums,
                    &config::get_config().cache_folder,
                    &albums,
                )
                .context("store user's saved albums into the cache folder")?;
                state.data.write().user_data.saved_albums = albums;
            }
            ClientRequest::GetUserSavedShows => {
                let Some(shows) = self.saved_shows(rate_limit::Priority::Background).await? else {
                    tracing::debug!("deferring the saved shows refresh: client is backed off");
                    return Ok(());
                };
                store_data_into_file_cache(
                    FileCacheKey::SavedShows,
                    &config::get_config().cache_folder,
                    &shows,
                )
                .context("store user's saved shows into the cache folder")?;
                state.data.write().user_data.saved_shows = shows;
            }
            ClientRequest::GetContext(context) => {
                self.handle_get_context(state, context).await?;
            }
            ClientRequest::Search(query) => {
                if !state.data.read().caches.search.contains_key(&query) {
                    let results = self.search(&query).await?;

                    state
                        .data
                        .write()
                        .caches
                        .search
                        .insert(query, results, *TTL_CACHE_DURATION);
                }
            }

            ClientRequest::AddPlayableToQueue(playable_id) => {
                self.add_item_to_queue(playable_id, None).await?;
            }
            ClientRequest::AddPlayableToPlaylist(playlist_id, playable_id) => {
                self.add_item_to_playlist(state, playlist_id, playable_id)
                    .await?;
            }
            ClientRequest::AddAlbumToQueue(album_id) => {
                let album_context = self.album_context(album_id).await?;

                if let Context::Album { album: _, tracks } = album_context {
                    for track in tracks {
                        self.add_item_to_queue(PlayableId::Track(track.id), None)
                            .await?;
                    }
                }
            }
            ClientRequest::DeleteTrackFromPlaylist(playlist_id, track_id) => {
                self.delete_track_from_playlist(state, playlist_id, track_id)
                    .await?;
            }
            ClientRequest::AddToLibrary(item) => {
                self.add_to_library(state, item).await?;
            }
            ClientRequest::DeleteFromLibrary(id) => {
                self.delete_from_library(state, id).await?;
            }
            ClientRequest::GetCurrentUserQueue => {
                let queue = self.current_user_queue().await?;
                state.player.write().queue = Some(queue);
            }
            ClientRequest::ReorderPlaylistItems {
                playlist_id,
                insert_index,
                range_start,
                range_length,
                snapshot_id,
            } => {
                self.reorder_playlist_items(
                    state,
                    playlist_id,
                    insert_index,
                    range_start,
                    range_length,
                    snapshot_id.as_deref(),
                )
                .await?;
            }
            ClientRequest::CreatePlaylist {
                playlist_name,
                public,
                collab,
                desc,
            } => {
                let user_id = state
                    .data
                    .read()
                    .user_data
                    .user
                    .as_ref()
                    .map(|u| u.id.clone())
                    .unwrap();
                self.create_new_playlist(
                    state,
                    user_id,
                    playlist_name.as_str(),
                    public,
                    collab,
                    desc.as_str(),
                )
                .await?;
            }
        }

        tracing::info!(
            "Successfully handled the client request, took: {}ms",
            timer.elapsed().as_millis()
        );

        Ok(())
    }

    /// Get lyrics of a given track, return None if no lyrics is available
    pub async fn lyrics(&self, track_id: TrackId<'static>) -> Result<Option<Lyrics>> {
        let session = self.spotify.session().await;
        let uri = SpotifyUri::from_uri(&track_id.uri())?;
        match uri {
            SpotifyUri::Track { id } => {
                match librespot_metadata::Lyrics::get(&session, &id).await {
                    Ok(lyrics) => Ok(Some(lyrics.into())),
                    Err(err) => {
                        if err.to_string().to_lowercase().contains("not found") {
                            Ok(None)
                        } else {
                            Err(err.into())
                        }
                    }
                }
            }
            _ => Ok(None),
        }
    }

    /// Get the current user's profile.
    ///
    /// Inherent so it goes through the throttled [`Self::http_get_with`] layer instead of
    /// rspotify (whose HTTP client has no pacing or 429 handling).
    pub async fn current_user(&self) -> Result<rspotify::model::PrivateUser> {
        self.http_get_with(
            &format!("{SPOTIFY_API_ENDPOINT}/me"),
            &Query::new(),
            rate_limit::Priority::High,
        )
        .await
    }

    /// Get user available devices
    pub async fn available_devices(&self) -> Result<Vec<rspotify::model::Device>> {
        let response = self
            .http_get_with::<DevicesResponse>(
                &format!("{SPOTIFY_API_ENDPOINT}/me/player/devices"),
                &Query::new(),
                rate_limit::Priority::High,
            )
            .await?;
        Ok(response.devices)
    }

    pub fn update_playback(&self, state: &SharedState) {
        // After handling a request changing the player's playback,
        // update the playback state by making a few get-playback requests.
        //
        // Q: Why do we need more than one request?
        // A: It can take a moment for Spotify to reflect the change. Rapid player events each
        // triggering this loop would otherwise flood the Web API and trip its 429 rate limit, so
        // coalesce them into a single in-flight loop.
        if self
            .update_playback_in_flight
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }

        let client = self.clone();
        let state = state.clone();
        let in_flight = self.update_playback_in_flight.clone();
        tokio::task::spawn(async move {
            // Startup catch-up + track-change lag: the integrated player starts audio
            // locally while Spotify's Web API still reports stale or empty playback
            // (eventual consistency). Keep polling at a 1s cadence until the API reports
            // the track the local stream is actually playing; bounded so nothing polls
            // forever.
            let mut attempts = 0u32;
            let mut logged_failure = false;
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                if let Err(err) = client.retrieve_current_playback(&state, false).await {
                    // One log line per poll run: an outage lasting the whole loop (up to 30
                    // attempts) must not produce 30 identical lines.
                    if logged_failure {
                        tracing::debug!("Playback state still failing to update: {err:#}");
                    } else {
                        tracing::error!(
                            "Encountered an error when updating the playback state: {err:#}"
                        );
                        logged_failure = true;
                    }
                }
                attempts += 1;

                let ready = {
                    let player = state.player.read();
                    player.playback.is_some() && player.buffered_playback.is_some()
                };
                if ready && client.playback_matches_stream(&state).await && attempts >= 2 {
                    break;
                }
                if attempts >= 30 {
                    break;
                }
            }
            in_flight.store(false, std::sync::atomic::Ordering::SeqCst);
        });
    }

    /// Whether the Web API's playback state has caught up to what the local librespot
    /// stream reports as the currently played track. Always true for non-integrated
    /// devices, where there is no local truth to compare against.
    #[cfg(feature = "streaming")]
    async fn playback_matches_stream(&self, state: &SharedState) -> bool {
        let (playback, streaming_track_id) = {
            let player = state.player.read();
            (
                player.playback.as_ref().map(|p| {
                    (
                        p.device.id.clone(),
                        p.item
                            .as_ref()
                            .and_then(rspotify::model::PlayableItem::id)
                            .map(|id| id.uri()),
                    )
                }),
                player.streaming_track_id.clone(),
            )
        };

        let Some((device_id, item_uri)) = playback else {
            // no playback yet (startup): keep chasing
            return false;
        };
        if !self.active_device_is_integrated(device_id.as_deref()).await {
            return true;
        }
        match (item_uri, streaming_track_id) {
            (_, None) => true,
            (Some(item_uri), Some(track_uri)) => item_uri == track_uri,
            (None, Some(_)) => false,
        }
    }

    /// Without the streaming feature there is no local player, so nothing to wait for.
    #[cfg(not(feature = "streaming"))]
    // Kept `async` to match the streaming signature so call sites stay feature-independent.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    async fn playback_matches_stream(&self, _state: &SharedState) -> bool {
        true
    }

    /// Get Spotify's available browse categories
    pub async fn browse_categories(&self) -> Result<Vec<Category>> {
        let first_page = self
            .categories_manual(Some("EN"), None, Some(50), None)
            .await?;

        Ok(first_page.items.into_iter().map(Category::from).collect())
    }

    /// Get Spotify's available browse playlists of a given category
    pub async fn browse_category_playlists(&self, category_id: &str) -> Result<Vec<Playlist>> {
        // TODO: this should use `rspotify::category_playlists_manual` API instead of `http_get`
        // The current implementation is a workaround for https://github.com/ramsayleung/rspotify/issues/535

        // Ok(self
        //     .category_playlists_manual(
        //         category_id,
        //         Some(rspotify::model::Market::FromToken),
        //         Some(50),
        //         None,
        //     )
        //     .await?
        //     .items
        //     .into_iter()
        //     .map(Into::into)
        //     .collect())

        #[derive(Deserialize, Debug)]
        struct BrowseCategoryPlaylistsResponse {
            playlists: rspotify::model::Page<serde_json::Value>,
        }

        Ok(self
            .http_get::<BrowseCategoryPlaylistsResponse>(
                &format!("{SPOTIFY_API_ENDPOINT}/browse/categories/{category_id}/playlists"),
                &Query::from([("limit", "50")]),
            )
            .await?
            .playlists
            .items
            .into_iter()
            .filter_map(|item| {
                serde_json::from_value::<rspotify::model::SimplifiedPlaylist>(item).ok()
            })
            .map(Into::into)
            .collect())
    }

    /// Find an available device. If found, return the device's ID.
    async fn find_available_device(&self) -> Result<Option<String>> {
        let devices = self.available_devices().await?;

        // if there is an active device, return it
        if let Some(d) = devices.iter().find(|d| d.is_active) {
            return Ok(d.id.clone());
        }

        #[allow(unused_mut)]
        let mut devices = devices
            .into_iter()
            .filter_map(Device::try_from_device)
            .collect::<Vec<_>>();

        #[cfg(feature = "streaming")]
        self.ensure_integrated_device(&mut devices).await;

        tracing::info!("no active device found, available devices: {devices:?}");

        if devices.is_empty() {
            return Ok(None);
        }

        // Prioritize the integrated device; otherwise, use the first available device.
        let id = devices
            .iter()
            .position(|d| d.is_integrated)
            .unwrap_or_default();

        Ok(Some(devices.remove(id).id))
    }

    /// Ensures the integrated librespot device (of *this* running instance) is present in `devices`.
    ///
    /// The integrated device may not show up in the device list returned by the Spotify API because
    /// 1. The device is just initialized and hasn't been registered in Spotify server.
    ///    Related issue/discussion: <https://github.com/aome510/spotify-player/issues/79>
    /// 2. The device list is empty. This might be because user doesn't specify their own client ID.
    ///    By default, the application uses Spotify web app's client ID, which doesn't have
    ///    access to user's active devices.
    #[cfg(feature = "streaming")]
    async fn ensure_integrated_device(&self, devices: &mut Vec<Device>) {
        let session = self.spotify.session().await;
        let session_device_id = session.device_id().to_string();

        // Mark the integrated device if it's already in the list; otherwise, add it, so it's
        // always present without duplicating an entry the API already returned.
        match devices.iter_mut().find(|d| d.id == session_device_id) {
            Some(device) => device.is_integrated = true,
            None => devices.insert(
                0,
                Device {
                    id: session_device_id,
                    name: config::get_config().app_config.device.name.clone(),
                    is_integrated: true,
                },
            ),
        }
    }

    /// Get the saved (liked) tracks of the current user
    pub async fn current_user_saved_tracks(&self) -> Result<Vec<Track>> {
        Self::require_ready(
            "saved tracks",
            self.saved_tracks(rate_limit::Priority::Normal).await?,
        )
    }

    /// Get the saved tracks at `priority`; `Ok(None)` when a
    /// [`rate_limit::Priority::Background`] fetch was deferred.
    pub async fn saved_tracks(&self, priority: rate_limit::Priority) -> Result<Option<Vec<Track>>> {
        let Some(tracks) = self
            .all_paging_items_optional::<TolerantSavedTrack>(
                &format!("{SPOTIFY_API_ENDPOINT}/me/tracks"),
                0, // we don't know the total number of saved tracks beforehand
                priority,
            )
            .await?
        else {
            return Ok(None);
        };

        Ok(Some(
            tracks
                .into_iter()
                .filter_map(|t| t.track.into_track(t.added_at))
                .collect(),
        ))
    }

    /// Get the recently played tracks at `priority`; `Ok(None)` when a
    /// [`rate_limit::Priority::Background`] fetch was deferred.
    pub async fn recently_played_tracks(
        &self,
        priority: rate_limit::Priority,
    ) -> Result<Option<Vec<Track>>> {
        let Some(first_page) = self
            .http_get_optional::<TolerantCursorPage<TolerantPlayHistory>>(
                &format!("{SPOTIFY_API_ENDPOINT}/me/player/recently-played"),
                &Query::from([("limit", "50")]),
                priority,
            )
            .await?
        else {
            return Ok(None);
        };

        let Some(play_histories) = self
            .all_cursor_based_paging_items_optional(first_page, priority)
            .await?
        else {
            return Ok(None);
        };

        // de-duplicate the tracks returned from the recently-played API
        let mut tracks = Vec::<Track>::new();
        for history in play_histories {
            if let Some(track) = history.track.into_track(None) {
                if !tracks.iter().any(|t| t.name == track.name) {
                    tracks.push(track);
                }
            }
        }
        Ok(Some(tracks))
    }

    /// Get the top tracks of the current user
    pub async fn current_user_top_tracks(&self) -> Result<Vec<Track>> {
        Self::require_ready(
            "top tracks",
            self.top_tracks(rate_limit::Priority::Normal).await?,
        )
    }

    /// Get the top tracks at `priority`; `Ok(None)` when a
    /// [`rate_limit::Priority::Background`] fetch was deferred.
    pub async fn top_tracks(&self, priority: rate_limit::Priority) -> Result<Option<Vec<Track>>> {
        let Some(tracks) = self
            .all_paging_items_optional::<TolerantTrack>(
                &format!("{SPOTIFY_API_ENDPOINT}/me/top/tracks"),
                0, // we don't know the total number of top tracks beforehand
                priority,
            )
            .await?
        else {
            return Ok(None);
        };

        Ok(Some(
            tracks
                .into_iter()
                .filter_map(|t| t.into_track(None))
                .collect(),
        ))
    }

    /// Get all playlists of the current user
    pub async fn current_user_playlists(&self) -> Result<Vec<Playlist>> {
        Self::require_ready(
            "playlists",
            self.user_playlists(rate_limit::Priority::Normal).await?,
        )
    }

    /// Get the current user's playlists at `priority`; `Ok(None)` when a
    /// [`rate_limit::Priority::Background`] fetch was deferred.
    pub async fn user_playlists(
        &self,
        priority: rate_limit::Priority,
    ) -> Result<Option<Vec<Playlist>>> {
        let Some(playlists) = self
            .all_paging_items_optional::<TolerantSimplifiedPlaylist>(
                &format!("{SPOTIFY_API_ENDPOINT}/me/playlists"),
                0, // we don't know the total number of playlists beforehand
                priority,
            )
            .await?
        else {
            return Ok(None);
        };

        Ok(Some(
            playlists
                .into_iter()
                .map(std::convert::Into::into)
                .collect(),
        ))
    }

    /// Get all followed artists of the current user
    pub async fn current_user_followed_artists(&self) -> Result<Vec<Artist>> {
        Self::require_ready(
            "followed artists",
            self.followed_artists(rate_limit::Priority::Normal).await?,
        )
    }

    /// Get the followed artists at `priority`; `Ok(None)` when a
    /// [`rate_limit::Priority::Background`] fetch was deferred.
    pub async fn followed_artists(
        &self,
        priority: rate_limit::Priority,
    ) -> Result<Option<Vec<Artist>>> {
        // Inherent fetch (throttled + 429-handled) instead of rspotify's unthrottled call.
        let Some(first_page) = self
            .http_get_optional::<TolerantFollowedArtists>(
                &format!("{SPOTIFY_API_ENDPOINT}/me/following"),
                &Query::from([("type", "artist"), ("limit", "50")]),
                priority,
            )
            .await?
        else {
            return Ok(None);
        };
        let first_page = first_page.artists;

        // followed artists pagination is handled different from
        // other paginations. The endpoint uses cursor-based pagination.
        let mut artists = first_page.items;
        let mut maybe_next = first_page.next;
        while let Some(url) = maybe_next {
            let Some(next_page) = self
                .http_get_optional::<TolerantFollowedArtists>(&url, &Query::new(), priority)
                .await?
            else {
                return Ok(None);
            };
            let mut next_page = next_page.artists;
            artists.append(&mut next_page.items);
            maybe_next = next_page.next;
        }

        Ok(Some(artists.into_iter().map(Into::into).collect()))
    }

    /// Get the current user's playback queue.
    ///
    /// Inherent so it goes through the throttled [`Self::http_get_with`] layer instead of
    /// rspotify.
    pub async fn current_user_queue(&self) -> Result<rspotify::model::CurrentUserQueue> {
        let queue = self
            .http_get_with::<TolerantQueue>(
                &format!("{SPOTIFY_API_ENDPOINT}/me/player/queue"),
                &Query::new(),
                rate_limit::Priority::High,
            )
            .await?;
        Ok(queue.into_current_user_queue())
    }

    /// Get all saved albums of the current user
    pub async fn current_user_saved_albums(&self) -> Result<Vec<Album>> {
        Self::require_ready(
            "saved albums",
            self.saved_albums(rate_limit::Priority::Normal).await?,
        )
    }

    /// Get the saved albums at `priority`; `Ok(None)` when a
    /// [`rate_limit::Priority::Background`] fetch was deferred.
    pub async fn saved_albums(&self, priority: rate_limit::Priority) -> Result<Option<Vec<Album>>> {
        let Some(albums) = self
            .all_paging_items_optional::<TolerantSavedAlbum>(
                &format!("{SPOTIFY_API_ENDPOINT}/me/albums"),
                0, // we don't know the total number of saved albums beforehand
                priority,
            )
            .await?
        else {
            return Ok(None);
        };

        // Converts the saved album entries into `state::Album`
        Ok(Some(
            albums
                .into_iter()
                .filter_map(|saved| {
                    let added_at = saved.added_at.map_or(0, |t| t.timestamp() as u64);
                    saved.album.into_album().map(|mut album| {
                        album.added_at = added_at;
                        album
                    })
                })
                .collect(),
        ))
    }

    /// Get the saved shows at `priority`; `Ok(None)` when a
    /// [`rate_limit::Priority::Background`] fetch was deferred.
    pub async fn saved_shows(&self, priority: rate_limit::Priority) -> Result<Option<Vec<Show>>> {
        let Some(shows) = self
            .all_paging_items_optional::<SavedShow>(
                &format!("{SPOTIFY_API_ENDPOINT}/me/shows"),
                0, // we don't know the total number of saved shows beforehand
                priority,
            )
            .await?
        else {
            return Ok(None);
        };

        Ok(Some(
            shows
                .into_iter()
                .map(|s| Show {
                    id: s.show.id,
                    name: s.show.name,
                    publisher: s.show.publisher,
                    #[cfg(feature = "image")]
                    image_url: s.show.images.first().map(|i| i.url.clone()),
                })
                .collect(),
        ))
    }

    /// Get all albums of an artist
    pub async fn artist_albums(&self, artist_id: ArtistId<'_>) -> Result<Vec<Album>> {
        let albums = self
            .all_paging_items::<rspotify::model::SimplifiedAlbum>(
                &format!(
                    "{SPOTIFY_API_ENDPOINT}/artists/{}/albums?include_groups=album,single",
                    artist_id.id()
                ),
                0, // we don't know the total number of artist albums beforehand
                rate_limit::Priority::Normal,
            )
            .await?
            .into_iter()
            .filter_map(Album::try_from_simplified_album)
            .collect();

        Ok(AppClient::process_artist_albums(albums))
    }

    /// Start a playback
    async fn start_playback(&self, playback: Playback, device_id: Option<&str>) -> Result<()> {
        match playback {
            Playback::Context(id, offset) => match id {
                ContextId::Album(id) => {
                    self.start_context_playback(PlayContextId::from(id), device_id, offset, None)
                        .await?;
                }
                ContextId::Artist(id) => {
                    self.start_context_playback(PlayContextId::from(id), device_id, offset, None)
                        .await?;
                }
                ContextId::Playlist(id) => {
                    self.start_context_playback(PlayContextId::from(id), device_id, offset, None)
                        .await?;
                }
                ContextId::Show(id) => {
                    self.start_context_playback(PlayContextId::from(id), device_id, offset, None)
                        .await?;
                }
                ContextId::Tracks(_) => {
                    anyhow::bail!("`StartPlayback` request for `tracks` context is not supported")
                }
            },
            Playback::URIs(ids, offset) => {
                self.start_uris_playback(ids, device_id, offset, None)
                    .await?;
            }
        }

        Ok(())
    }

    /// Get recommendation (radio) tracks based on a seed
    pub async fn radio_tracks(&self, seed_uri: String) -> Result<Vec<Track>> {
        #[derive(Debug, Deserialize)]
        struct TrackData {
            original_gid: String,
        }
        #[derive(Debug, Deserialize)]
        struct RadioStationResponse {
            tracks: Vec<TrackData>,
        }

        let session = self.spotify.session().await;

        // Get an autoplay URI from the seed URI.
        // The return URI is a Spotify station's URI
        let autoplay_query_url = format!("hm://autoplay-enabled/query?uri={seed_uri}");
        let response = session
            .mercury()
            .get(autoplay_query_url)
            .map_err(|err| anyhow::anyhow!("Failed to get autoplay URI: {err:#}"))?
            .await?;
        if response.status_code != 200 {
            anyhow::bail!(
                "Failed to get autoplay URI: got non-OK status code: {}",
                response.status_code
            );
        }
        let autoplay_uri = String::from_utf8(response.payload[0].clone())?;

        // Retrieve radio's data based on the autoplay URI
        let radio_query_url = format!("hm://radio-apollo/v3/stations/{autoplay_uri}");
        let response = session
            .mercury()
            .get(radio_query_url)
            .map_err(|err| anyhow::anyhow!("Failed to get radio data of {autoplay_uri}: {err:#}"))?
            .await?;
        if response.status_code != 200 {
            anyhow::bail!(
                "Failed to get radio data of {autoplay_uri}: got non-OK status code: {}",
                response.status_code
            );
        }

        // Parse a list consisting of IDs of tracks inside the radio station
        let track_ids = serde_json::from_slice::<RadioStationResponse>(&response.payload[0])?
            .tracks
            .into_iter()
            .filter_map(|t| TrackId::from_id(t.original_gid).ok());

        // Retrieve tracks based on IDs
        let tracks = self
            .tracks(track_ids, Some(rspotify::model::Market::FromToken))
            .await?;
        let mut tracks: Vec<_> = tracks
            .into_iter()
            .filter_map(Track::try_from_full_track)
            .collect();

        // Track-seeded radios in the official Spotify clients include the seed track itself
        // as the first item in the generated session.
        if let Ok(track_id) = TrackId::from_uri(&seed_uri) {
            match self.track(track_id).await {
                Ok(track) => move_seed_track_to_front(&mut tracks, track),
                Err(err) => {
                    tracing::warn!("Failed to fetch track radio seed {seed_uri}: {err:#}");
                }
            }
        }

        Ok(tracks)
    }

    /// Search for items (tracks, artists, albums, playlists) matching a given query
    pub async fn search(&self, query: &str) -> Result<SearchResults> {
        let (track_result, artist_result, album_result, playlist_result, shows, episode_result) = tokio::try_join!(
            self.search_specific_type(query, rspotify::model::SearchType::Track),
            self.search_specific_type(query, rspotify::model::SearchType::Artist),
            self.search_specific_type(query, rspotify::model::SearchType::Album),
            self.search_specific_type(query, rspotify::model::SearchType::Playlist),
            self.search_shows(query),
            self.search_specific_type(query, rspotify::model::SearchType::Episode)
        )?;

        let (tracks, artists, albums, playlists, episodes) = (
            match track_result {
                rspotify::model::SearchResult::Tracks(p) => p
                    .items
                    .into_iter()
                    .filter_map(Track::try_from_full_track)
                    .collect(),
                _ => anyhow::bail!("expect a track search result"),
            },
            match artist_result {
                rspotify::model::SearchResult::Artists(p) => {
                    p.items.into_iter().map(std::convert::Into::into).collect()
                }
                _ => anyhow::bail!("expect an artist search result"),
            },
            match album_result {
                rspotify::model::SearchResult::Albums(p) => p
                    .items
                    .into_iter()
                    .filter_map(Album::try_from_simplified_album)
                    .collect(),
                _ => anyhow::bail!("expect an album search result"),
            },
            match playlist_result {
                rspotify::model::SearchResult::Playlists(p) => {
                    p.items.into_iter().map(std::convert::Into::into).collect()
                }
                _ => anyhow::bail!("expect a playlist search result"),
            },
            match episode_result {
                rspotify::model::SearchResult::Episodes(p) => {
                    p.items.into_iter().map(std::convert::Into::into).collect()
                }
                _ => anyhow::bail!("expect a episode search result"),
            },
        );

        Ok(SearchResults {
            tracks,
            artists,
            albums,
            playlists,
            shows,
            episodes,
        })
    }

    /// Search for items of a specific type matching a given query
    pub async fn search_specific_type(
        &self,
        query: &str,
        typ: rspotify::model::SearchType,
    ) -> Result<rspotify::model::SearchResult> {
        Ok(self
            .deref()
            .search(query, typ, None, None, None, None)
            .await?)
    }

    /// Search for shows matching a given query.
    ///
    /// Uses a raw request because rspotify's `SearchResult::Shows` deserializes into
    /// `SimplifiedShow`, which requires the `available_markets` field Spotify no longer sends.
    pub async fn search_shows(&self, query: &str) -> Result<Vec<Show>> {
        let result = self
            .http_get::<ShowSearchResult>(
                &format!("{SPOTIFY_API_ENDPOINT}/search"),
                &Query::from([("q", query), ("type", "show"), ("market", "from_token")]),
            )
            .await?
            .shows
            .items
            .into_iter()
            .map(|s| Show {
                id: s.id,
                name: s.name,
                publisher: s.publisher,
                #[cfg(feature = "image")]
                image_url: s.images.first().map(|i| i.url.clone()),
            })
            .collect::<Vec<Show>>();

        Ok(result)
    }

    /// Add a playable item to a playlist
    pub async fn add_item_to_playlist(
        &self,
        state: &SharedState,
        playlist_id: PlaylistId<'_>,
        playable_id: PlayableId<'_>,
    ) -> Result<()> {
        // remove all the occurrences of the track to ensure no duplication in the playlist
        self.playlist_remove_all_occurrences_of_items(
            playlist_id.as_ref(),
            [playable_id.as_ref()],
            None,
        )
        .await?;

        self.playlist_add_items(playlist_id.as_ref(), [playable_id.as_ref()], None)
            .await?;

        // After adding a new track to a playlist, remove the cache of that playlist to force refetching new data
        state.data.write().caches.context.remove(&playlist_id.uri());

        Ok(())
    }

    /// Remove a track from a playlist
    pub async fn delete_track_from_playlist(
        &self,
        state: &SharedState,
        playlist_id: PlaylistId<'_>,
        track_id: TrackId<'_>,
    ) -> Result<()> {
        // remove all the occurrences of the track to ensure no duplication in the playlist
        self.playlist_remove_all_occurrences_of_items(
            playlist_id.as_ref(),
            [PlayableId::Track(track_id.as_ref())],
            None,
        )
        .await?;

        // After making a delete request, update the playlist in-memory data stored inside the app caches.
        if let Some(Context::Playlist { tracks, .. }) = state
            .data
            .write()
            .caches
            .context
            .get_mut(&playlist_id.uri())
        {
            tracks.retain(|t| t.id != track_id);
        }

        Ok(())
    }

    /// Reorder items in a playlist
    async fn reorder_playlist_items(
        &self,
        state: &SharedState,
        playlist_id: PlaylistId<'_>,
        insert_index: usize,
        range_start: usize,
        range_length: Option<usize>,
        snapshot_id: Option<&str>,
    ) -> Result<()> {
        let insert_before = if insert_index > range_start {
            insert_index + 1
        } else {
            insert_index
        };

        self.playlist_reorder_items(
            playlist_id.clone(),
            Some(range_start as i32),
            Some(insert_before as i32),
            range_length.map(|range_length| range_length as u32),
            snapshot_id,
        )
        .await?;

        // After making a reorder request, update the playlist in-memory data stored inside the app caches.
        if let Some(Context::Playlist { tracks, .. }) = state
            .data
            .write()
            .caches
            .context
            .get_mut(&playlist_id.uri())
        {
            let track = tracks.remove(range_start);
            tracks.insert(insert_index, track);
        }

        Ok(())
    }

    /// Add a Spotify item to current user's library.
    async fn add_to_library(&self, state: &SharedState, item: Item) -> Result<()> {
        // Before adding new item, checks if that item already exists in the library to avoid adding a duplicated item.
        match item {
            Item::Track(track) => {
                let contains = self
                    .current_user_saved_tracks_contains([track.id.as_ref()])
                    .await?;
                if !contains[0] {
                    self.current_user_saved_tracks_add([track.id.as_ref()])
                        .await?;
                    // update the in-memory `user_data`
                    state
                        .data
                        .write()
                        .user_data
                        .saved_tracks
                        .insert(track.id.uri(), track);
                }
            }
            Item::Album(album) => {
                let contains = self
                    .current_user_saved_albums_contains([album.id.as_ref()])
                    .await?;
                if !contains[0] {
                    self.current_user_saved_albums_add([album.id.as_ref()])
                        .await?;
                    // update the in-memory `user_data`
                    state.data.write().user_data.saved_albums.insert(0, album);
                }
            }
            Item::Artist(artist) => {
                let follows = self.user_artist_check_follow([artist.id.as_ref()]).await?;
                if !follows[0] {
                    self.user_follow_artists([artist.id.as_ref()]).await?;
                    // update the in-memory `user_data`
                    state
                        .data
                        .write()
                        .user_data
                        .followed_artists
                        .insert(0, artist);
                }
            }
            Item::Playlist(playlist) => {
                let user_id = state
                    .data
                    .read()
                    .user_data
                    .user
                    .as_ref()
                    .map(|u| u.id.clone());

                if let Some(user_id) = user_id {
                    let follows = self
                        .playlist_check_follow(playlist.id.as_ref(), &[user_id])
                        .await?;
                    if !follows[0] {
                        self.playlist_follow(playlist.id.as_ref(), None).await?;
                        // update the in-memory `user_data`
                        state
                            .data
                            .write()
                            .user_data
                            .playlists
                            .insert(0, PlaylistFolderItem::Playlist(playlist));
                    }
                }
            }
            Item::Show(show) => {
                let follows = self.check_users_saved_shows([show.id.as_ref()]).await?;
                if !follows[0] {
                    self.save_shows([show.id.as_ref()]).await?;
                    // update the in-memory `user_data`
                    state.data.write().user_data.saved_shows.insert(0, show);
                }
            }
        }
        Ok(())
    }

    // Delete a Spotify item from user's library
    async fn delete_from_library(&self, state: &SharedState, id: ItemId) -> Result<()> {
        match id {
            ItemId::Track(id) => {
                let uri = id.uri();
                self.current_user_saved_tracks_delete([id]).await?;
                state.data.write().user_data.saved_tracks.remove(&uri);
            }
            ItemId::Album(id) => {
                state
                    .data
                    .write()
                    .user_data
                    .saved_albums
                    .retain(|a| a.id != id);
                self.current_user_saved_albums_delete([id]).await?;
            }
            ItemId::Artist(id) => {
                state
                    .data
                    .write()
                    .user_data
                    .followed_artists
                    .retain(|a| a.id != id);
                self.user_unfollow_artists([id]).await?;
            }
            ItemId::Playlist(id) => {
                state
                    .data
                    .write()
                    .user_data
                    .playlists
                    .retain(|item| match item {
                        PlaylistFolderItem::Playlist(p) => p.id != id,
                        PlaylistFolderItem::Folder(_) => true,
                    });
                self.playlist_unfollow(id).await?;
            }
            ItemId::Show(id) => {
                state
                    .data
                    .write()
                    .user_data
                    .saved_shows
                    .retain(|s| s.id != id);
                self.remove_users_saved_shows([id], Some(rspotify::model::Market::FromToken))
                    .await?;
            }
        }
        Ok(())
    }

    /// Get a track data
    pub async fn track(&self, track_id: TrackId<'_>) -> Result<Track> {
        Track::try_from_full_track(
            self.deref()
                .track(track_id, Some(rspotify::model::Market::FromToken))
                .await?,
        )
        .context("convert FullTrack into Track")
    }

    /// Get a playlist context data
    pub async fn playlist_context(&self, playlist_id: PlaylistId<'_>) -> Result<Context> {
        let playlist_uri = playlist_id.uri();
        tracing::info!("Get playlist context: {}", playlist_uri);

        let playlist = self
            .http_get_with::<TolerantPlaylistDetail>(
                &format!("{SPOTIFY_API_ENDPOINT}/playlists/{}", playlist_id.id()),
                &Query::from([("market", "from_token")]),
                rate_limit::Priority::Normal,
            )
            .await?;

        let total_tracks = playlist.track_total();
        let tracks = self
            .all_paging_items::<TolerantPlaylistItem>(
                &format!(
                    "{SPOTIFY_API_ENDPOINT}/playlists/{}/items",
                    playlist_id.id(),
                ),
                total_tracks,
                rate_limit::Priority::Normal,
            )
            .await?
            .into_iter()
            .filter_map(TolerantPlaylistItem::into_track)
            .collect::<Vec<_>>();

        Ok(Context::Playlist {
            playlist: playlist.into(),
            tracks,
        })
    }

    /// Get an album context data
    pub async fn album_context(&self, album_id: AlbumId<'_>) -> Result<Context> {
        let album_uri = album_id.uri();
        tracing::info!("Get album context: {}", album_uri);

        let album_detail = self
            .http_get_with::<TolerantAlbumDetail>(
                &format!("{SPOTIFY_API_ENDPOINT}/albums/{}", album_id.id()),
                &Query::from([("market", "from_token")]),
                rate_limit::Priority::Normal,
            )
            .await?;

        let total_tracks = album_detail.track_total();

        // converts the album payload into `state::Album`
        let album = album_detail
            .into_album()
            .context("album payload has no id")?;

        // get the album's tracks
        let tracks = self
            .all_paging_items::<TolerantTrack>(
                &format!("{SPOTIFY_API_ENDPOINT}/albums/{}/tracks", album_id.id()),
                total_tracks,
                rate_limit::Priority::Normal,
            )
            .await?
            .into_iter()
            .filter_map(|track| {
                // simplified track doesn't have album so
                // we need to manually include one during
                // converting into `state::Track`
                track.into_track(None).map(|mut track| {
                    track.album = Some(album.clone());
                    track
                })
            })
            .collect::<Vec<_>>();

        Ok(Context::Album { album, tracks })
    }

    /// Get a single artist's metadata.
    ///
    /// Inherent so it goes through the throttled [`Self::http_get_with`] layer instead of
    /// rspotify. Takes the ID by value to mirror rspotify's signature and keep existing call
    /// sites (`artist_id.as_ref()` / `id.clone()`) compiling.
    pub async fn artist(&self, id: ArtistId<'_>) -> Result<rspotify::model::FullArtist> {
        let artist = self
            .http_get_with::<TolerantFullArtist>(
                &format!("{SPOTIFY_API_ENDPOINT}/artists/{}", id.id()),
                &Query::new(),
                rate_limit::Priority::High,
            )
            .await?;
        Ok(artist.into())
    }

    /// Get an artist context data.
    ///
    /// The top tracks, albums and related artists are all best-effort: a reduced app
    /// registration is refused by `top-tracks` (403) and the `related-artists` endpoint no longer
    /// exists, and neither may take the page down with it. Each failure is warned about once per
    /// session.
    pub async fn artist_context(&self, artist_id: ArtistId<'_>) -> Result<Context> {
        let artist_uri = artist_id.uri();
        tracing::info!("Get artist context: {}", artist_uri);

        // get the artist's information, including top tracks, related artists, and albums
        let artist: Artist = self
            .artist(artist_id.as_ref())
            .await
            .context("get artist")?
            .into();

        let top_tracks = match self
            .artist_top_tracks(artist_id.as_ref(), Some(rspotify::model::Market::FromToken))
            .await
        {
            Ok(tracks) => tracks
                .into_iter()
                .filter_map(Track::try_from_full_track)
                .collect(),
            Err(err) => {
                warn_once_unavailable(
                    &ARTIST_TOP_TRACKS_UNAVAILABLE,
                    "top tracks",
                    &anyhow::Error::new(err),
                );
                Vec::new()
            }
        };

        #[allow(deprecated)]
        let related_artists = match self.artist_related_artists(artist_id.as_ref()).await {
            Ok(artists) => artists.into_iter().map(Into::into).collect(),
            Err(err) => {
                warn_once_unavailable(
                    &ARTIST_RELATED_ARTISTS_UNAVAILABLE,
                    "related artists",
                    &anyhow::Error::new(err),
                );
                Vec::new()
            }
        };

        let albums = match self.artist_albums(artist_id.as_ref()).await {
            Ok(albums) => albums,
            Err(err) => {
                warn_once_unavailable(&ARTIST_ALBUMS_UNAVAILABLE, "albums", &err);
                Vec::new()
            }
        };

        Ok(Context::Artist {
            artist,
            top_tracks,
            albums,
            related_artists,
        })
    }

    /// Get a show context data
    ///
    /// Only the first page of episodes is fetched eagerly; the rest load in the background so
    /// the table fills up without requiring the user to scroll.
    pub async fn show_context(&self, show_id: ShowId<'_>) -> Result<Context> {
        let show_uri = show_id.uri();
        tracing::info!("Get show context: {}", show_uri);

        let show = self
            .http_get::<FullShow>(
                &format!("{SPOTIFY_API_ENDPOINT}/shows/{}", show_id.id()),
                &Query::new(),
            )
            .await?;

        // Reuse the first page of episodes already bundled in the show payload instead of
        // issuing a second paginated request for the same data.
        let total_episodes = show.episodes.total as usize;
        let episodes: Vec<Episode> = show.episodes.items.into_iter().map(Episode::from).collect();

        let show = Show {
            id: show.id,
            name: show.name,
            publisher: show.publisher,
            #[cfg(feature = "image")]
            image_url: show.images.first().map(|i| i.url.clone()),
        };
        Ok(Context::Show {
            show,
            episodes,
            total_episodes,
        })
    }

    /// Fetch one page of a show's episodes starting at `offset`.
    ///
    /// Chunk size is the configured page of rows (so the first page fills the view), clamped to
    /// Spotify's practical 25–50 range.
    async fn show_episodes_page(
        &self,
        show_id: &ShowId<'_>,
        offset: usize,
    ) -> Result<Vec<Episode>> {
        let limit = config::get_config()
            .app_config
            .page_size_in_rows
            .clamp(25, 50);
        let limit_str = limit.to_string();
        let offset_str = offset.to_string();
        let params = Query::from([
            ("market", "from_token"),
            ("limit", &limit_str),
            ("offset", &offset_str),
        ]);
        let page = self
            .http_get::<rspotify::model::Page<SimplifiedEpisodeInfo>>(
                &format!("{SPOTIFY_API_ENDPOINT}/shows/{}/episodes", show_id.id()),
                &params,
            )
            .await?;

        Ok(page
            .items
            .into_iter()
            .map(Episode::from)
            .collect::<Vec<_>>())
    }

    /// Spawn a background task that appends the remaining pages of a show's episodes to the
    /// cached context as they arrive, so the episode table fills up without user interaction.
    fn spawn_show_episodes_prefetch(
        &self,
        state: SharedState,
        show_id: ShowId<'static>,
        uri: String,
        mut offset: usize,
        total_episodes: usize,
    ) {
        let client = self.clone();
        tokio::spawn(async move {
            while offset < total_episodes {
                match client.show_episodes_page(&show_id, offset).await {
                    Ok(episodes) => {
                        if episodes.is_empty() {
                            break;
                        }
                        offset += episodes.len();
                        let appended = {
                            let mut data = state.data.write();
                            match data.caches.context.get_mut(&uri) {
                                Some(Context::Show {
                                    episodes: loaded, ..
                                }) => {
                                    loaded.extend(episodes);
                                    true
                                }
                                _ => false,
                            }
                        };
                        if appended {
                            // These pages land *after* `handle_request` returned, so nothing else
                            // tells a frontend the episode table grew.
                            state.redraw.raise();
                        }
                    }
                    Err(err) => {
                        tracing::warn!("failed to prefetch show episodes: {err:#}");
                        break;
                    }
                }
            }
        });
    }

    /// Fetch an episode cover into the episode ring buffer (and the disk file cache when
    /// enabled). The in-memory copy is downscaled to a display-sized budget — full-resolution
    /// originals stay on disk only, keeping TUI memory footprint small.
    #[cfg(feature = "image")]
    async fn fetch_episode_image(&self, state: &SharedState, url: &str) -> Result<()> {
        // Downscale once: the largest render target is the episode footer strip (~15x8 cells),
        // so a 192px cap is beyond sufficient and shrinks each entry ~50x versus full-size.
        const MAX_DIMENSION: u32 = 192;
        if state.data.read().caches.episode_images.contains(url) {
            return Ok(());
        }

        let configs = config::get_config();
        let filename = format!(
            "episode-cover-{}.jpg",
            url.rsplit('/').next().unwrap_or("img")
        );
        let path = configs.cache_folder.join("image").join(filename);
        let bytes = self
            .retrieve_image(url, &path, configs.app_config.enable_cover_image_cache)
            .await?;

        #[allow(unused_mut)]
        let mut image =
            image::load_from_memory(&bytes).context("Failed to load image from memory")?;
        #[cfg(feature = "pixelate")]
        Self::pixelate_image(&mut image);

        if image.width() > MAX_DIMENSION || image.height() > MAX_DIMENSION {
            image = image.resize(
                MAX_DIMENSION,
                MAX_DIMENSION,
                image::imageops::FilterType::Triangle,
            );
        }

        state
            .data
            .write()
            .caches
            .episode_images
            .insert(url.to_owned(), image);
        Ok(())
    }
    /// Unwrap a possibly-deferred result of a request that can never be deferred (only
    /// [`rate_limit::Priority::Background`] requests defer).
    fn require_ready<T>(what: &str, value: Option<T>) -> Result<T> {
        value.ok_or_else(|| anyhow::anyhow!("{what} request was unexpectedly deferred"))
    }

    /// Make a GET HTTP request to the Spotify server at [`rate_limit::Priority::Normal`].
    async fn http_get<T>(&self, url: &str, payload: &Query<'_>) -> Result<T>
    where
        T: serde::de::DeserializeOwned,
    {
        self.http_get_with(url, payload, rate_limit::Priority::Normal)
            .await
    }

    /// Make a GET HTTP request to the Spotify server, paced according to `priority`.
    ///
    /// Only [`rate_limit::Priority::Background`] can defer; use [`Self::http_get_background`]
    /// for those call sites.
    async fn http_get_with<T>(
        &self,
        url: &str,
        payload: &Query<'_>,
        priority: rate_limit::Priority,
    ) -> Result<T>
    where
        T: serde::de::DeserializeOwned,
    {
        match self.http_get_optional(url, payload, priority).await? {
            Some(value) => Ok(value),
            // Only background acquisitions defer, and no caller of this method uses that class.
            None => anyhow::bail!("unexpected deferral of a {priority:?} request to {url}"),
        }
    }

    /// Paced GET shared by the wrappers above; `Ok(None)` means the request was deferred
    /// (possible only for [`rate_limit::Priority::Background`]).
    async fn http_get_optional<T>(
        &self,
        url: &str,
        payload: &Query<'_>,
        priority: rate_limit::Priority,
    ) -> Result<Option<T>>
    where
        T: serde::de::DeserializeOwned,
    {
        /// a helper function to process an API response from Spotify server
        ///
        /// This function is mainly used to patch upstream API bugs , resulting in
        /// a type error when a third-party library like `rspotify` parses the response
        fn process_spotify_api_response(text: &str) -> String {
            text.to_string()
        }

        // Respect Spotify's rate limit: on a 429, pause the *whole* client for the server's
        // `Retry-After` and retry. Each request waiting on the shared cooldown (instead of
        // sleeping and retrying on its own schedule) is what prevents retry storms.
        const MAX_RATE_LIMIT_RETRIES: usize = 5;

        let mut attempt = 0;
        loop {
            let access_token = self.token().await.context("get token")?;
            tracing::debug!("{access_token} {url}");

            // `High`/`Normal` may probe an active cooldown once per window; `Background` defers.
            // A retry (after the request's own 429) must not consume the probe it already spent.
            let outcome = if attempt == 0 {
                self.rate_limiter.acquire(priority).await
            } else {
                self.rate_limiter.acquire_after_rate_limit(priority).await
            };
            match outcome {
                rate_limit::AcquireOutcome::Ready => {}
                rate_limit::AcquireOutcome::Deferred => {
                    tracing::debug!(
                        "deferring {priority:?} request to {url}: client is backed off"
                    );
                    return Ok(None);
                }
            }

            let response = self
                .http
                .get(url)
                .query(payload)
                .header(
                    reqwest::header::AUTHORIZATION,
                    format!("Bearer {access_token}"),
                )
                .send()
                .await?;

            let status = response.status();

            if status == StatusCode::TOO_MANY_REQUESTS {
                let retry_after = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .map(std::time::Duration::from_secs);

                let applied_cooldown = self.rate_limiter.on_rate_limited(retry_after);

                if !rate_limit::should_retry_in_place(applied_cooldown) {
                    // Long pause: the quota window is exhausted for everyone sharing it, so
                    // holding this handler task for another tens of seconds per attempt wins
                    // nothing. Bail and let the caller's own cadence (the page-change re-issue,
                    // or the next user action) retry later - each such retry gets its own
                    // single probe. The cooldown above is still registered for everyone else.
                    let text = response.text().await.unwrap_or_default();
                    tracing::warn!(
                        "rate-limited (429) requesting {url}; server asked to pause {applied_cooldown:?}, \
                         longer than the {threshold:?} transient threshold, so not retrying in place",
                        threshold = rate_limit::TRANSIENT_COOLDOWN,
                    );
                    anyhow::bail!(
                        "rate-limited (429) requesting {url}: server asked to pause {applied_cooldown:?} \
                         (window exhausted), not retrying in place: {text}"
                    );
                }

                attempt += 1;
                if attempt >= MAX_RATE_LIMIT_RETRIES {
                    let text = response.text().await.unwrap_or_default();
                    anyhow::bail!(
                        "rate-limited (429) after {MAX_RATE_LIMIT_RETRIES} attempts requesting {url}: {text}"
                    );
                }

                tracing::warn!(
                    "rate-limited (429) requesting {url}; pausing the client for {retry_after:?} (attempt {attempt}/{MAX_RATE_LIMIT_RETRIES})"
                );
                // the next `acquire` waits out the shared cooldown
                continue;
            }

            let text = process_spotify_api_response(&response.text().await?);
            tracing::debug!("{text}");

            if status != StatusCode::OK && status != StatusCode::NO_CONTENT {
                anyhow::bail!("failed to send a Spotify API request {url}: {text}");
            }

            // The API answered, so end the client-wide pause early and reset the backoff.
            self.rate_limiter.on_success();

            if status == StatusCode::NO_CONTENT || text.is_empty() {
                // Bodyless success (e.g. `GET /me/player` with nothing playing): parse the
                // JSON `null` literal, so callers expecting `Option<T>` get `None`. rspotify
                // treated an empty body the same way, and some endpoints answer an empty body
                // with `200 OK` rather than `204 No Content`.
                return Ok(Some(serde_json::from_str("null")?));
            }

            return Ok(Some(serde_json::from_str(&text)?));
        }
    }

    /// Fetch all items of a paging endpoint at `priority`, returning `Ok(None)` without
    /// sending anything when a [`rate_limit::Priority::Background`] request was deferred (so a
    /// partial list is never cached).
    async fn all_paging_items_optional<T>(
        &self,
        base_url: &str,
        mut count: usize,
        priority: rate_limit::Priority,
    ) -> Result<Option<Vec<T>>>
    where
        T: serde::de::DeserializeOwned + std::fmt::Debug,
    {
        const PAGE_LIMIT: usize = 50;
        const MAX_PARALLEL: usize = 8;

        let mut all_items = Vec::new();
        let mut offset = 0;

        // if count is 0 (i.e., unknown), set it to usize::MAX to fetch until no more items
        if count == 0 {
            count = usize::MAX;
        }

        while offset < count {
            let n_jobs = std::cmp::min(MAX_PARALLEL, (count - offset).div_ceil(PAGE_LIMIT));

            let mut futures = Vec::with_capacity(n_jobs);

            for i in 0..n_jobs {
                let current_offset = offset + i * PAGE_LIMIT;
                let limit_str = PAGE_LIMIT.to_string();
                let offset_str = current_offset.to_string();

                futures.push(async move {
                    let params = Query::from([
                        ("market", "from_token"),
                        ("limit", &limit_str),
                        ("offset", &offset_str),
                    ]);
                    self.http_get_optional::<rspotify::model::Page<T>>(base_url, &params, priority)
                        .await
                });
            }

            let results = futures::future::try_join_all(futures).await?;

            let mut found_empty = false;
            for page in results {
                let Some(mut page) = page else {
                    // A page was deferred mid-pagination: skip the whole refresh.
                    return Ok(None);
                };
                if page.items.is_empty() {
                    found_empty = true;
                    break;
                }
                all_items.append(&mut page.items);
            }

            if found_empty {
                break;
            }

            offset += n_jobs * PAGE_LIMIT;
        }

        Ok(Some(all_items))
    }

    /// Like [`Self::all_paging_items_optional`], for a request that cannot be deferred.
    async fn all_paging_items<T>(
        &self,
        base_url: &str,
        count: usize,
        priority: rate_limit::Priority,
    ) -> Result<Vec<T>>
    where
        T: serde::de::DeserializeOwned + std::fmt::Debug,
    {
        self.all_paging_items_optional(base_url, count, priority)
            .await?
            .ok_or_else(|| anyhow::anyhow!("unexpected deferral while paging {base_url}"))
    }

    /// Get all cursor-based paging items starting from a pagination object of the first page.
    /// `Ok(None)` when a [`rate_limit::Priority::Background`] request was deferred.
    async fn all_cursor_based_paging_items_optional<T>(
        &self,
        first_page: TolerantCursorPage<T>,
        priority: rate_limit::Priority,
    ) -> Result<Option<Vec<T>>>
    where
        T: serde::de::DeserializeOwned,
    {
        let mut items = first_page.items;
        let mut maybe_next = first_page.next;
        while let Some(url) = maybe_next {
            let Some(mut next_page) = self
                .http_get_optional::<TolerantCursorPage<T>>(&url, &Query::new(), priority)
                .await?
            else {
                return Ok(None);
            };
            items.append(&mut next_page.items);
            maybe_next = next_page.next;
        }
        Ok(Some(items))
    }

    pub async fn current_playback2(
        &self,
    ) -> Result<Option<rspotify::model::CurrentPlaybackContext>> {
        // `GET /me/player` answers `204 No Content` when nothing is playing; `http_get_with`
        // maps that to `None`.
        let playback = self
            .http_get_with::<Option<TolerantPlayback>>(
                &format!("{SPOTIFY_API_ENDPOINT}/me/player"),
                &Query::from([("additional_types", "track,episode")]),
                rate_limit::Priority::High,
            )
            .await?;
        Ok(playback.map(TolerantPlayback::into_current_playback))
    }

    /// Retrieve the latest playback state
    pub async fn retrieve_current_playback(
        &self,
        state: &SharedState,
        reset_buffered_playback: bool,
    ) -> Result<()> {
        let new_playback = {
            // update the playback state
            let playback = self.current_playback2().await?;
            let mut player = state.player.write();

            let prev_item = player.currently_playing();

            let prev_name = match prev_item {
                Some(rspotify::model::PlayableItem::Track(track)) => track.name.clone(),
                Some(rspotify::model::PlayableItem::Episode(episode)) => episode.name.clone(),
                Some(rspotify::model::PlayableItem::Unknown(_)) | None => String::new(),
            };

            player.playback = playback;
            player.playback_last_updated_time = Some(std::time::Instant::now());

            let curr_item = player.currently_playing();

            let curr_name = match curr_item {
                Some(rspotify::model::PlayableItem::Track(track)) => track.name.clone(),
                Some(rspotify::model::PlayableItem::Episode(episode)) => episode.name.clone(),
                Some(rspotify::model::PlayableItem::Unknown(_)) | None => String::new(),
            };

            let new_playback = prev_name != curr_name && !curr_name.is_empty();
            // check if we need to update the buffered playback
            let needs_update = match (&player.buffered_playback, &player.playback) {
                (Some(bp), Some(p)) => bp.device_id != p.device.id || new_playback,
                (None, None) => false,
                _ => true,
            };

            if reset_buffered_playback || needs_update {
                player.buffered_playback = player.playback.as_ref().map(|p| {
                    let mut playback = PlaybackMetadata::from_playback(p);

                    // handle additional data from the previous buffered state
                    // that is not available in a standard Spotify playback's state
                    if let Some(bp) = &player.buffered_playback {
                        if let Some(volume) = bp.mute_state {
                            playback.volume = Some(volume);
                        }
                        playback.mute_state = bp.mute_state;
                    }
                    playback
                });
            }
            new_playback
        };

        // `player.playback` was just replaced (whether or not the new track differs): this is the
        // one place the Web API's playback state is written, from both the request handler and the
        // background `update_playback` poll, so a frontend redraws from here rather than from
        // every caller.
        if !new_playback {
            state.redraw.raise();
            return Ok(());
        }

        self.handle_new_playback_event(state).await?;
        // Raised after the new track's cover has been fetched and cached: raising before it would
        // let the frame that paints the new track miss the cover and wait for the next one (the
        // progress tick at best, the safety net at worst).
        state.redraw.raise();

        Ok(())
    }

    // Handle new track event
    async fn handle_new_playback_event(&self, state: &SharedState) -> Result<()> {
        let configs = config::get_config();

        let curr_item = {
            let player = state.player.read();
            let Some(track_or_episode) = player.currently_playing() else {
                return Ok(());
            };
            track_or_episode.clone()
        };

        // retrieve current artist for genres if not in cache
        let curr_artist = match &curr_item {
            rspotify::model::PlayableItem::Track(full_track) => {
                let cached = state
                    .data
                    .read()
                    .caches
                    .genres
                    .contains_key(&full_track.artists[0].name);

                if cached {
                    None
                } else {
                    match &full_track.artists[0].id {
                        Some(id) => self.artist(id.clone()).await.ok(),
                        None => None,
                    }
                }
            }
            rspotify::model::PlayableItem::Episode(_)
            | rspotify::model::PlayableItem::Unknown(_) => None,
        };

        if let Some(artist) = curr_artist {
            if !artist.genres.is_empty() {
                state.data.write().caches.genres.insert(
                    artist.name,
                    artist.genres,
                    *TTL_CACHE_DURATION,
                );
            }
        }

        let url = match curr_item {
            rspotify::model::PlayableItem::Track(ref track) => {
                crate::utils::get_track_album_image_url(track)
                    .ok_or(anyhow::anyhow!("missing image"))?
            }
            rspotify::model::PlayableItem::Episode(ref episode) => {
                crate::utils::get_episode_show_image_url(episode)
                    .ok_or(anyhow::anyhow!("missing image"))?
            }
            rspotify::model::PlayableItem::Unknown(_) => return Ok(()),
        };

        let filename = (match curr_item {
            rspotify::model::PlayableItem::Track(ref track) => {
                format!(
                    "{}-{}-cover-{}.jpg",
                    track.album.name,
                    track.album.artists.first().unwrap().name,
                    // first 6 characters of the album's id
                    &track.album.id.as_ref().unwrap().id()[..6]
                )
            }
            rspotify::model::PlayableItem::Episode(ref episode) => {
                format!(
                    "{}-{}-cover-{}.jpg",
                    episode.show.name,
                    episode.show.publisher,
                    // first 6 characters of the show's id
                    &episode.show.id.as_ref().id()[..6]
                )
            }
            rspotify::model::PlayableItem::Unknown(_) => return Ok(()),
        })
        .replace('/', ""); // remove invalid characters from the file's name
        let path = configs.cache_folder.join("image").join(filename);

        if configs.app_config.enable_cover_image_cache {
            self.retrieve_image(url, &path, true).await?;
        }

        #[cfg(feature = "image")]
        if !state.data.read().caches.images.contains_key(url) {
            let bytes = self.retrieve_image(url, &path, false).await?;

            #[cfg(not(feature = "pixelate"))]
            let image =
                image::load_from_memory(&bytes).context("Failed to load image from memory")?;
            #[cfg(feature = "pixelate")]
            let mut image =
                image::load_from_memory(&bytes).context("Failed to load image from memory")?;

            #[cfg(feature = "pixelate")]
            {
                Self::pixelate_image(&mut image);
            }

            state
                .data
                .write()
                .caches
                .images
                .insert(url.to_owned(), image, *TTL_CACHE_DURATION);
        }

        // notify user about the playback's change if any
        #[cfg(all(feature = "notify", feature = "streaming"))]
        if configs.app_config.enable_notify
            && (!configs.app_config.notify_streaming_only || self.stream_conn.lock().is_some())
        {
            Self::notify_new_playback(&curr_item, &path)?;
        }

        #[cfg(all(feature = "notify", not(feature = "streaming")))]
        if configs.app_config.enable_notify {
            Self::notify_new_playback(&curr_item, &path)?;
        }

        Ok(())
    }

    /// Create a new playlist
    async fn create_new_playlist(
        &self,
        state: &SharedState,
        user_id: UserId<'static>,
        playlist_name: &str,
        public: bool,
        collab: bool,
        desc: &str,
    ) -> Result<()> {
        let playlist: Playlist = self
            .user_playlist_create(
                user_id,
                playlist_name,
                Some(public),
                Some(collab),
                Some(desc),
            )
            .await?
            .into();
        tracing::info!(
            "new playlist (name={},id={}) was successfully created",
            playlist.name,
            playlist.id
        );
        state
            .data
            .write()
            .user_data
            .playlists
            .insert(0, PlaylistFolderItem::Playlist(playlist));
        Ok(())
    }

    #[cfg(feature = "notify")]
    /// Create a notification for a new playback
    fn notify_new_playback(
        playable: &rspotify::model::PlayableItem,
        cover_img_path: &std::path::Path,
    ) -> Result<()> {
        let mut n = notify_rust::Notification::new();

        let re = regex::Regex::new(r"\{.*?\}").unwrap();
        // Generate a text described a track from a format string.
        // For example, a format string "{track} - {artists}" will generate
        // a text consisting of the track's name followed by a dash then artists' names.
        let get_text_from_format_str = |format_str: &str| {
            let mut text = String::new();

            let mut ptr = 0;
            for m in re.find_iter(format_str) {
                let s = m.start();
                let e = m.end();

                if ptr < s {
                    text += &format_str[ptr..s];
                }
                ptr = e;
                match m.as_str() {
                    "{track}" => {
                        let name = match playable {
                            rspotify::model::PlayableItem::Track(ref track) => &track.name,
                            rspotify::model::PlayableItem::Episode(ref episode) => &episode.name,
                            rspotify::model::PlayableItem::Unknown(_) => continue,
                        };
                        text += name;
                    }
                    "{artists}" => {
                        if let rspotify::model::PlayableItem::Track(ref track) = playable {
                            text += &crate::utils::map_join(&track.artists, |a| &a.name, ", ");
                        }
                    }
                    "{album}" => match playable {
                        rspotify::model::PlayableItem::Track(ref track) => {
                            text += &track.album.name;
                        }
                        rspotify::model::PlayableItem::Episode(ref episode) => {
                            text += &episode.show.name;
                        }
                        rspotify::model::PlayableItem::Unknown(_) => {}
                    },
                    &_ => {}
                }
            }
            if ptr < format_str.len() {
                text += &format_str[ptr..];
            }

            text
        };

        let configs = config::get_config();

        n.appname("spotify_player")
            .summary(&get_text_from_format_str(
                &configs.app_config.notify_format.summary,
            ))
            .body(&get_text_from_format_str(
                &configs.app_config.notify_format.body,
            ));
        if cover_img_path.exists() {
            n.icon(cover_img_path.to_str().context("valid cover_img_path")?);
        }
        if configs.app_config.notify_timeout_in_secs > 0 {
            n.timeout(std::time::Duration::from_secs(
                configs.app_config.notify_timeout_in_secs,
            ));
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        if configs.app_config.notify_transient {
            use notify_rust::Hint;
            n.hint(Hint::Transient(true));
        }
        n.show()?;

        Ok(())
    }

    /// Retrieve an image from a `url` or a cached `path`.
    /// If `saved` is specified, the retrieved image is saved to the cached `path`.
    async fn retrieve_image(
        &self,
        url: &str,
        path: &std::path::Path,
        saved: bool,
    ) -> Result<Vec<u8>> {
        if path.exists() {
            tracing::debug!("Retrieving image from file: {}", path.display());
            return Ok(std::fs::read(path)?);
        }

        tracing::info!("Retrieving image from url: {url}");

        let bytes = self
            .http
            .get(url)
            .send()
            .await
            .with_context(|| format!("get image from url {url}"))?
            .bytes()
            .await?;

        if saved {
            tracing::info!("Saving the retrieved image into {}", path.display());
            let mut file = std::fs::File::create(path)?;
            file.write_all(&bytes)?;
        }

        Ok(bytes.to_vec())
    }

    #[cfg(feature = "pixelate")]
    fn pixelate_image(image: &mut image::DynamicImage) {
        let pixels = config::get_config().app_config.cover_img_pixels;
        let pixelated_image = image.resize(pixels, pixels, image::imageops::FilterType::Nearest);
        *image = pixelated_image.resize(
            image.width(),
            image.height(),
            image::imageops::FilterType::Nearest,
        );
    }

    /// Process a list of albums, which includes
    /// - sort albums by the release date
    /// - sort albums by the type if `sort_artist_albums_by_type` config is enabled
    fn process_artist_albums(mut albums: Vec<Album>) -> Vec<Album> {
        albums.sort_by(|x, y| y.release_date.partial_cmp(&x.release_date).unwrap());

        if config::get_config().app_config.sort_artist_albums_by_type {
            fn get_priority(album_type: &str) -> usize {
                match album_type {
                    "album" => 0,
                    "single" => 1,
                    "appears_on" => 2,
                    "compilation" => 3,
                    _ => 4,
                }
            }
            albums.sort_by_key(|a| get_priority(&a.album_type()));
        }

        albums
    }
}

/// A playback start prepared for `Spirc::load`, with the pane data to publish until the first
/// player event arrives.
#[cfg(feature = "streaming")]
struct LocalLoad {
    request: librespot_connect::LoadRequest,
    /// The item the pane shows until librespot reports the actual track.
    item_uri: Option<String>,
    /// The context the load addresses, for the context kinds that carry one.
    context: Option<(String, rspotify::model::Type)>,
}

/// Build the librespot load request that starts `playback` on the integrated player.
///
/// `None` when there is nothing to load: a `Tracks` context (liked/top/recently played) whose
/// tracks are not cached locally, or an empty URI list.
#[cfg(feature = "streaming")]
fn local_load(state: &SharedState, playback: &Playback, shuffle: bool) -> Option<LocalLoad> {
    let options = librespot_connect::LoadRequestOptions {
        start_playing: true,
        seek_to: 0,
        context_options: Some(librespot_connect::LoadContextOptions::Options(
            librespot_connect::Options {
                shuffle,
                repeat: false,
                repeat_track: false,
            },
        )),
        playing_track: None,
    };

    let (request, item_uri, context) = match playback {
        // The Web API refuses to start a `Tracks` context (it has no such playback context);
        // the local player takes the track list from the app's caches instead.
        Playback::Context(ContextId::Tracks(tracks_id), offset) => {
            let uris = context_track_uris(state, tracks_id);
            if uris.is_empty() {
                return None;
            }
            let playing_track = offset_uri(offset.as_ref());
            let item_uri = playing_track.clone().or_else(|| uris.first().cloned());
            (
                librespot_connect::LoadRequest::from_tracks(
                    uris,
                    librespot_connect::LoadRequestOptions {
                        playing_track: playing_track.map(librespot_connect::PlayingTrack::Uri),
                        ..options
                    },
                ),
                item_uri,
                None,
            )
        }
        Playback::Context(id, offset) => {
            let playing_track = offset_uri(offset.as_ref());
            let item_uri = playing_track
                .clone()
                .or_else(|| first_context_item_uri(state, id));
            (
                librespot_connect::LoadRequest::from_context_uri(
                    id.uri(),
                    librespot_connect::LoadRequestOptions {
                        playing_track: playing_track.map(librespot_connect::PlayingTrack::Uri),
                        ..options
                    },
                ),
                item_uri,
                Some((id.uri(), context_type(id))),
            )
        }
        Playback::URIs(ids, offset) => {
            let uris: Vec<String> = ids.iter().map(rspotify::prelude::Id::uri).collect();
            if uris.is_empty() {
                return None;
            }
            let playing_track = offset_uri(offset.as_ref());
            let item_uri = playing_track.clone().or_else(|| uris.first().cloned());
            (
                librespot_connect::LoadRequest::from_tracks(
                    uris,
                    librespot_connect::LoadRequestOptions {
                        playing_track: playing_track.map(librespot_connect::PlayingTrack::Uri),
                        ..options
                    },
                ),
                item_uri,
                None,
            )
        }
    };

    Some(LocalLoad {
        request,
        item_uri,
        context,
    })
}

/// Convert an app volume percentage (0-100) into librespot's 0-65535 range.
#[cfg(feature = "streaming")]
fn volume_to_librespot(volume_percent: u8) -> u16 {
    (f64::from(volume_percent) / 100.0 * 65_535.0).round() as u16
}

/// URI carried by a playback [`rspotify::model::Offset`], when it addresses an item by URI.
#[cfg(feature = "streaming")]
fn offset_uri(offset: Option<&rspotify::model::Offset>) -> Option<String> {
    match offset {
        Some(rspotify::model::Offset::Uri(uri)) => Some(uri.clone()),
        _ => None,
    }
}

/// The Web API context `Type` for a context id.
#[cfg(feature = "streaming")]
fn context_type(id: &ContextId) -> rspotify::model::Type {
    match id {
        ContextId::Playlist(_) => rspotify::model::Type::Playlist,
        ContextId::Album(_) => rspotify::model::Type::Album,
        ContextId::Artist(_) => rspotify::model::Type::Artist,
        ContextId::Show(_) => rspotify::model::Type::Show,
        ContextId::Tracks(_) => rspotify::model::Type::Collection,
    }
}

/// URI of the first playable item of a context, from the in-memory context cache. Used to fill
/// the playback pane when `StartPlayback` carries no explicit offset.
#[cfg(feature = "streaming")]
fn first_context_item_uri(state: &SharedState, id: &ContextId) -> Option<String> {
    let uri = id.uri();
    let data = state.data.read();
    match data.caches.context.get(&uri)? {
        Context::Playlist { tracks, .. } | Context::Album { tracks, .. } => {
            tracks.first().map(|t| t.id.uri())
        }
        Context::Artist { top_tracks, .. } => top_tracks.first().map(|t| t.id.uri()),
        Context::Tracks { tracks, .. } => tracks.first().map(|t| t.id.uri()),
        Context::Show { episodes, .. } => episodes.first().map(|e| e.id.uri()),
    }
}

/// Track URIs of a `Tracks` context (liked/top/recently played). The Web API path cannot start
/// these contexts, so the local player takes the track list directly.
#[cfg(feature = "streaming")]
fn context_track_uris(state: &SharedState, tracks_id: &TracksId) -> Vec<String> {
    if let Some(Context::Tracks { tracks, .. }) =
        state.data.read().caches.context.get(&tracks_id.uri)
    {
        return tracks.iter().map(|t| t.id.uri()).collect();
    }
    if tracks_id.uri == USER_LIKED_TRACKS_URI {
        // The liked list may not have been fetched into the context cache yet; the disk-seeded
        // library is the same source the page renders from.
        let mut uris: Vec<String> = state
            .data
            .read()
            .user_data
            .saved_tracks
            .values()
            .map(|t| t.id.uri())
            .collect();
        uris.sort();
        return uris;
    }
    Vec::new()
}

/// Resolve a playable item from the app's caches so the playback pane can show real metadata
/// while the Web API cannot refresh `/me/player`.
#[cfg(feature = "streaming")]
fn playback_item_for_uri(state: &SharedState, uri: &str) -> Option<rspotify::model::PlayableItem> {
    if let Some(track) = state.data.read().user_data.saved_tracks.get(uri) {
        return Some(app_track_to_playable(track));
    }

    // `TtlCache::iter` needs `&mut`, so this scan takes a write guard (no await while held).
    let mut data = state.data.write();
    for (_, context) in data.caches.context.iter() {
        match context {
            Context::Playlist { tracks, .. } | Context::Album { tracks, .. } => {
                if let Some(track) = tracks.iter().find(|t| t.id.uri() == uri) {
                    return Some(app_track_to_playable(track));
                }
            }
            Context::Artist { top_tracks, .. } => {
                if let Some(track) = top_tracks.iter().find(|t| t.id.uri() == uri) {
                    return Some(app_track_to_playable(track));
                }
            }
            Context::Tracks { tracks, .. } => {
                if let Some(track) = tracks.iter().find(|t| t.id.uri() == uri) {
                    return Some(app_track_to_playable(track));
                }
            }
            Context::Show { episodes, .. } => {
                if let Some(episode) = episodes.iter().find(|e| e.id.uri() == uri) {
                    return app_episode_to_playable(episode);
                }
            }
        }
    }
    None
}
fn app_artist_to_simplified(artist: &Artist) -> rspotify::model::SimplifiedArtist {
    rspotify::model::SimplifiedArtist {
        external_urls: HashMap::new(),
        href: None,
        id: Some(artist.id.clone()),
        name: artist.name.clone(),
    }
}

fn app_album_to_simplified(album: &Album) -> rspotify::model::SimplifiedAlbum {
    rspotify::model::SimplifiedAlbum {
        album_group: None,
        album_type: album
            .typ
            .and_then(|typ| serde_json::to_value(typ).ok())
            .and_then(|value| value.as_str().map(str::to_owned)),
        artists: album.artists.iter().map(app_artist_to_simplified).collect(),
        available_markets: Vec::new(),
        external_urls: HashMap::new(),
        href: None,
        id: Some(album.id.clone()),
        // The app's cached album has no cover images, so the pane shows no cover for a locally
        // resolved item (the Web API refresh, when available, replaces this with the real one).
        images: Vec::new(),
        name: album.name.clone(),
        release_date: Some(album.release_date.clone()),
        release_date_precision: None,
        restrictions: None,
    }
}

fn app_track_to_playable(track: &Track) -> rspotify::model::PlayableItem {
    let album = track.album.as_ref().map_or_else(
        || rspotify::model::SimplifiedAlbum {
            album_group: None,
            album_type: None,
            artists: Vec::new(),
            available_markets: Vec::new(),
            external_urls: HashMap::new(),
            href: None,
            id: None,
            images: Vec::new(),
            name: String::new(),
            release_date: None,
            release_date_precision: None,
            restrictions: None,
        },
        app_album_to_simplified,
    );

    rspotify::model::PlayableItem::Track(rspotify::model::FullTrack {
        album,
        artists: track.artists.iter().map(app_artist_to_simplified).collect(),
        available_markets: Vec::new(),
        disc_number: 1,
        duration: chrono::Duration::from_std(track.duration).unwrap_or_default(),
        explicit: track.explicit,
        external_ids: HashMap::new(),
        external_urls: HashMap::new(),
        href: None,
        id: Some(track.id.clone()),
        is_local: false,
        is_playable: None,
        linked_from: None,
        restrictions: None,
        name: track.name.clone(),
        popularity: 0,
        preview_url: None,
        track_number: 0,
        r#type: rspotify::model::Type::Track,
    })
}

/// Convert a cached episode into a playable item. `None` when the episode has no associated
/// show, since `FullEpisode` requires one.
#[allow(deprecated)] // `FullEpisode::language` is deprecated but still required by the struct.
fn app_episode_to_playable(episode: &Episode) -> Option<rspotify::model::PlayableItem> {
    let show = episode.show.as_ref()?;
    let show = rspotify::model::SimplifiedShow {
        available_markets: Vec::new(),
        copyrights: Vec::new(),
        description: String::new(),
        explicit: false,
        external_urls: HashMap::new(),
        href: String::new(),
        id: show.id.clone(),
        images: Vec::new(),
        is_externally_hosted: None,
        languages: Vec::new(),
        media_type: String::new(),
        name: show.name.clone(),
        publisher: show.publisher.clone(),
    };

    Some(rspotify::model::PlayableItem::Episode(
        rspotify::model::FullEpisode {
            audio_preview_url: None,
            description: episode.description.clone(),
            duration: chrono::Duration::from_std(episode.duration).unwrap_or_default(),
            explicit: false,
            external_urls: HashMap::new(),
            href: String::new(),
            id: episode.id.clone(),
            images: Vec::new(),
            is_externally_hosted: false,
            is_playable: true,
            language: String::new(),
            languages: Vec::new(),
            name: episode.name.clone(),
            release_date: episode.release_date.clone(),
            release_date_precision: rspotify::model::DatePrecision::Year,
            resume_point: None,
            show,
            r#type: rspotify::model::Type::Episode,
        },
    ))
}

fn move_seed_track_to_front(tracks: &mut Vec<Track>, seed_track: Track) {
    tracks.retain(|track| track.id != seed_track.id);
    tracks.insert(0, seed_track);
}

#[cfg(test)]
mod tests {
    use super::move_seed_track_to_front;
    #[cfg(feature = "streaming")]
    use super::{is_transient_api_failure, transport_for, PlayerRequest, Transport};
    use super::{
        TolerantAlbumDetail, TolerantFullArtist, TolerantPlayback, TolerantPlaylistDetail,
        TolerantPlaylistItem, TolerantQueue, TolerantSimplifiedPlaylist,
    };
    #[cfg(feature = "streaming")]
    use crate::state::{ContextId, Playback};
    use crate::state::{Playlist, Track};
    #[cfg(feature = "streaming")]
    use rspotify::model::PlaylistId;
    use rspotify::model::TrackId;
    use rspotify::prelude::Id;

    fn sample_track(id: &'static str, name: &str) -> Track {
        Track {
            id: TrackId::from_id(id).unwrap().into_static(),
            name: name.to_string(),
            artists: vec![],
            album: None,
            duration: std::time::Duration::default(),
            explicit: false,
            added_at: 0,
        }
    }

    #[test]
    fn move_seed_track_to_front_prepends_missing_seed() {
        let seed = sample_track("3n3Ppam7vgaVa1iaRUc9Lp", "seed");
        let second = sample_track("4uLU6hMCjMI75M1A2tKUQC", "second");
        let third = sample_track("1301WleyT98MSxVHPZCA6M", "third");
        let mut tracks = vec![second.clone(), third];

        move_seed_track_to_front(&mut tracks, seed.clone());

        assert_eq!(tracks.len(), 3);
        assert_eq!(tracks[0].id, seed.id);
        assert_eq!(tracks[1].id, second.id);
    }

    #[test]
    fn move_seed_track_to_front_reorders_existing_seed_without_duplication() {
        let seed = sample_track("3n3Ppam7vgaVa1iaRUc9Lp", "seed");
        let second = sample_track("4uLU6hMCjMI75M1A2tKUQC", "second");
        let mut tracks = vec![second.clone(), seed.clone()];

        move_seed_track_to_front(&mut tracks, seed.clone());

        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].id, seed.id);
        assert_eq!(tracks[1].id, second.id);
    }

    #[test]
    fn reduced_playlist_detail_reads_its_total_from_items() {
        // The reduced shape moves the track page from `tracks` to `items`.
        let json = r#"{
            "id": "4CUQQ1fJ2FCuiXRidjssjl",
            "name": "Reduced playlist",
            "collaborative": false,
            "description": "A &amp; B <b>bold</b>",
            "snapshot_id": "snap",
            "owner": {"id": "owner", "display_name": "Owner"},
            "items": {"href": "x", "total": 2, "items": []}
        }"#;

        let detail: TolerantPlaylistDetail = serde_json::from_str(json).unwrap();
        assert_eq!(detail.track_total(), 2);

        let playlist = Playlist::from(detail);
        assert_eq!(playlist.name, "Reduced playlist");
        assert_eq!(playlist.owner.0, "Owner");
        assert_eq!(playlist.owner.1.id(), "owner");
        assert_eq!(playlist.snapshot_id, "snap");
        // the description is stripped and unescaped like rspotify's conversion does
        assert_eq!(playlist.desc, "A & B bold");

        // the classic shape keeps working through `tracks`
        let classic = json.replace(
            "\"items\": {\"href\": \"x\", \"total\": 2, \"items\": []}",
            "\"tracks\": {\"total\": 3, \"items\": []}",
        );
        let detail: TolerantPlaylistDetail = serde_json::from_str(&classic).unwrap();
        assert_eq!(detail.track_total(), 3);
    }

    #[test]
    fn playlist_entries_convert_from_item_and_track() {
        // The reduced shape names the entry's playable `item`; the classic shape names it `track`.
        let reduced = r#"{
            "added_at": "2024-06-01T12:00:00Z",
            "item": {
                "type": "track",
                "id": "4uLU6hMCjMI75M1A2tKUQC",
                "name": "Reduced entry",
                "duration_ms": 60000,
                "explicit": false,
                "artists": [{"id": "2CaSnl81O3ZQGOy25LqoR7", "name": "Artist"}]
            }
        }"#;
        let classic = r#"{
            "track": {
                "type": "track",
                "id": "1301WleyT98MSxVHPZCA6M",
                "name": "Classic entry",
                "duration_ms": 1000,
                "explicit": true,
                "artists": []
            }
        }"#;
        // an episode in a playlist is not a track, as before
        let episode = r#"{"item": {"type": "episode", "in_playlist": true}}"#;

        let entry: TolerantPlaylistItem = serde_json::from_str(reduced).unwrap();
        let track = entry.into_track().expect("reduced entry converts");
        assert_eq!(track.name, "Reduced entry");
        assert_eq!(track.duration.as_secs(), 60);
        assert_eq!(track.added_at, 1_717_243_200);

        let entry: TolerantPlaylistItem = serde_json::from_str(classic).unwrap();
        let track = entry.into_track().expect("classic entry converts");
        assert_eq!(track.name, "Classic entry");
        assert!(track.explicit);

        let entry: TolerantPlaylistItem = serde_json::from_str(episode).unwrap();
        assert!(entry.into_track().is_none());
    }

    #[test]
    fn reduced_playback_item_is_a_real_track() {
        let json = r#"{
            "device": {"id": "dev", "is_active": true, "is_private_session": false,
                       "is_restricted": false, "name": "device", "type": "Computer",
                       "volume_percent": 42},
            "repeat_state": "off",
            "shuffle_state": true,
            "context": {"uri": "spotify:playlist:4CUQQ1fJ2FCuiXRidjssjl", "type": "playlist"},
            "timestamp": 1717243200000,
            "progress_ms": 5000,
            "is_playing": true,
            "currently_playing_type": "track",
            "item": {
                "type": "track",
                "id": "4uLU6hMCjMI75M1A2tKUQC",
                "name": "Reduced Track",
                "duration_ms": 200000,
                "explicit": false,
                "artists": [{"id": "2CaSnl81O3ZQGOy25LqoR7", "name": "Artist"}],
                "album": {"id": "1301WleyT98MSxVHPZCA6M", "name": "Album",
                          "release_date": "2020-01-01", "album_type": "album"}
            }
        }"#;

        // the reduced `item` withholds `popularity`/`external_ids`, which is what used to make
        // the whole response unparseable and the pane fall back to `Unknown`
        assert!(serde_json::from_str::<rspotify::model::CurrentPlaybackContext>(json).is_err());

        let playback: TolerantPlayback = serde_json::from_str(json).unwrap();
        let playback = playback.into_current_playback();

        let rspotify::model::PlayableItem::Track(track) =
            playback.item.expect("the payload has an item")
        else {
            panic!("the reduced item must convert into a real track");
        };
        assert_eq!(track.name, "Reduced Track");
        assert_eq!(track.album.name, "Album");
        assert_eq!(
            playback.currently_playing_type,
            rspotify::model::CurrentlyPlayingType::Track
        );
        assert_eq!(
            playback.progress,
            Some(chrono::Duration::milliseconds(5000))
        );
        assert_eq!(playback.timestamp.timestamp_millis(), 1_717_243_200_000);
        assert_eq!(playback.device.id.as_deref(), Some("dev"));
        assert_eq!(
            playback
                .context
                .as_ref()
                .map(|context| context.uri.as_str()),
            Some("spotify:playlist:4CUQQ1fJ2FCuiXRidjssjl")
        );
        assert!(playback.is_playing);
        assert!(playback.shuffle_state);
    }

    #[test]
    fn reduced_artist_parses_without_followers_genres_or_popularity() {
        let json = r#"{
            "external_urls": {"spotify": "https://open.spotify.com/artist/2CaSnl81O3ZQGOy25LqoR7"},
            "href": "https://api.spotify.com/v1/artists/2CaSnl81O3ZQGOy25LqoR7",
            "id": "2CaSnl81O3ZQGOy25LqoR7",
            "images": [{"url": "https://i.scdn.co/image/x", "height": 640, "width": 640}],
            "name": "Laura Canela",
            "type": "artist",
            "uri": "spotify:artist:2CaSnl81O3ZQGOy25LqoR7"
        }"#;

        assert!(serde_json::from_str::<rspotify::model::FullArtist>(json).is_err());

        let artist: TolerantFullArtist = serde_json::from_str(json).unwrap();
        let artist: rspotify::model::FullArtist = artist.into();
        assert_eq!(artist.name, "Laura Canela");
        assert_eq!(artist.id.id(), "2CaSnl81O3ZQGOy25LqoR7");
        // fields the reduced shape withholds (and the app never displays) are filled in
        assert!(artist.genres.is_empty());
        assert_eq!(artist.popularity, 0);
        assert_eq!(artist.followers.total, 0);

        // fields that are present keep their values
        let full = r#"{"id": "2CaSnl81O3ZQGOy25LqoR7", "name": "Full", "genres": ["pop"],
                       "popularity": 66, "followers": {"total": 5}}"#;
        let artist: TolerantFullArtist = serde_json::from_str(full).unwrap();
        let artist: rspotify::model::FullArtist = artist.into();
        assert_eq!(artist.genres, ["pop"]);
        assert_eq!(artist.popularity, 66);
        assert_eq!(artist.followers.total, 5);
    }

    #[test]
    fn reduced_playlist_entry_without_tracks_parses() {
        let json = r#"{
            "id": "4CUQQ1fJ2FCuiXRidjssjl",
            "name": "Reduced playlist",
            "collaborative": false,
            "snapshot_id": "snap",
            "owner": {"id": "owner", "display_name": "Owner"},
            "items": {"href": "x", "total": 2, "items": []}
        }"#;

        assert!(serde_json::from_str::<rspotify::model::SimplifiedPlaylist>(json).is_err());

        let playlist: TolerantSimplifiedPlaylist = serde_json::from_str(json).unwrap();
        let playlist = Playlist::from(playlist);
        assert_eq!(playlist.name, "Reduced playlist");
        assert_eq!(playlist.owner.1.id(), "owner");
        assert_eq!(playlist.snapshot_id, "snap");
        // the library list carries no description; the playlist detail fetch fills it in
        assert!(playlist.desc.is_empty());
    }

    #[test]
    fn reduced_album_detail_parses_without_popularity_or_label() {
        let json = r#"{
            "id": "1301WleyT98MSxVHPZCA6M",
            "name": "Reduced Album",
            "album_type": "single",
            "release_date": "2021-05-05",
            "total_tracks": 2,
            "artists": [{"id": "2CaSnl81O3ZQGOy25LqoR7", "name": "Artist"}],
            "tracks": {"total": 2, "items": []}
        }"#;

        assert!(serde_json::from_str::<rspotify::model::FullAlbum>(json).is_err());

        let detail: TolerantAlbumDetail = serde_json::from_str(json).unwrap();
        assert_eq!(detail.track_total(), 2);

        let album = detail.into_album().expect("the payload has an id");
        assert_eq!(album.name, "Reduced Album");
        assert_eq!(album.release_date, "2021-05-05");
        assert_eq!(album.typ, Some(rspotify::model::AlbumType::Single));
        assert_eq!(album.artists[0].name, "Artist");
    }

    #[test]
    fn queue_keeps_unparseable_entries_as_unknown() {
        let json = r#"{
            "currently_playing": {
                "type": "track", "id": "4uLU6hMCjMI75M1A2tKUQC", "name": "Playing",
                "duration_ms": 1000, "explicit": false, "artists": []
            },
            "queue": [
                {"type": "track", "id": "1301WleyT98MSxVHPZCA6M", "name": "Next",
                 "duration_ms": 1000, "explicit": false, "artists": []},
                {"id": "local-file", "is_local": true, "name": "Local"}
            ]
        }"#;

        let queue: TolerantQueue = serde_json::from_str(json).unwrap();
        let queue = queue.into_current_user_queue();

        assert!(matches!(
            queue.currently_playing,
            Some(rspotify::model::PlayableItem::Track(_))
        ));
        assert_eq!(queue.queue.len(), 2);
        assert!(matches!(
            queue.queue[0],
            rspotify::model::PlayableItem::Track(_)
        ));
        // like rspotify, an entry that is neither track nor episode is kept as raw `Unknown`
        assert!(matches!(
            queue.queue[1],
            rspotify::model::PlayableItem::Unknown(_)
        ));
    }

    /// One request per transport-relevant kind, as the local transport sees them.
    #[cfg(feature = "streaming")]
    fn player_requests() -> Vec<(PlayerRequest, &'static str)> {
        let playback = Playback::URIs(
            vec![rspotify::model::PlayableId::Track(
                TrackId::from_id("4uLU6hMCjMI75M1A2tKUQC")
                    .unwrap()
                    .into_static(),
            )],
            None,
        );
        vec![
            (PlayerRequest::StartPlayback(playback, Some(true)), "start"),
            (PlayerRequest::Pause, "pause"),
            (PlayerRequest::Resume, "resume"),
            (PlayerRequest::ResumePause, "resume/pause"),
            (PlayerRequest::NextTrack, "next"),
            (PlayerRequest::PreviousTrack, "previous"),
            (
                PlayerRequest::SeekTrack(chrono::Duration::seconds(30)),
                "seek",
            ),
            (PlayerRequest::Repeat, "repeat"),
            (PlayerRequest::Shuffle, "shuffle"),
            (PlayerRequest::Volume(50), "volume"),
            (PlayerRequest::ToggleMute, "mute"),
            (
                PlayerRequest::TransferPlayback("device".to_string(), true),
                "transfer",
            ),
        ]
    }

    #[cfg(feature = "streaming")]
    #[test]
    fn integrated_active_device_serves_every_request_locally() {
        for (request, what) in player_requests() {
            // our own device is active: the request never reaches the Web API ...
            assert_eq!(
                transport_for(&request, true, false),
                Transport::Integrated,
                "{what} must run on the integrated player"
            );
            // ... and a failed Web API attempt cannot change that, since none is made
            assert_eq!(
                transport_for(&request, true, true),
                Transport::Integrated,
                "{what} must not be retried on the Web API"
            );
        }
    }

    #[cfg(feature = "streaming")]
    #[test]
    fn other_active_device_keeps_the_web_api() {
        for (request, what) in player_requests() {
            if matches!(request, PlayerRequest::StartPlayback(..)) {
                continue; // covered by the fallback test
            }
            assert_eq!(
                transport_for(&request, false, false),
                Transport::WebApi,
                "{what} must stay on the Web API"
            );
            assert_eq!(
                transport_for(&request, false, true),
                Transport::WebApi,
                "{what} must not fall back to the local player"
            );
        }
    }

    #[cfg(feature = "streaming")]
    #[test]
    fn playback_start_falls_back_to_local_only_after_a_failed_api_call() {
        let start = PlayerRequest::StartPlayback(
            Playback::Context(
                ContextId::Playlist(
                    PlaylistId::from_id("4CUQQ1fJ2FCuiXRidjssjl")
                        .unwrap()
                        .into_static(),
                ),
                None,
            ),
            None,
        );

        // another device is active: the Web API is asked first ...
        assert_eq!(transport_for(&start, false, false), Transport::WebApi);
        // ... and only its failure lets the local player claim the device
        assert_eq!(
            transport_for(&start, false, true),
            Transport::WebApiWithLocalFallback
        );
    }

    #[cfg(feature = "streaming")]
    #[test]
    fn rate_limit_and_connectivity_failures_are_transient() {
        assert!(is_transient_api_failure(&anyhow::anyhow!(
            "rate-limited (429) requesting https://api.spotify.com/v1/me/player"
        )));
        assert!(!is_transient_api_failure(&anyhow::anyhow!(
            "failed to send a Spotify API request https://api.spotify.com/v1/me/player: Forbidden"
        )));
    }
}
