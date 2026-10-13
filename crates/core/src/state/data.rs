use std::io::{BufReader, BufWriter};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::sync::LazyLock;

use super::model::{
    Album, Artist, Category, Context, ContextId, Id, Playlist, PlaylistFolderItem,
    PlaylistFolderNode, SearchResults, Show, Track,
};
use super::Lyrics;

pub type DataReadGuard<'a> = parking_lot::RwLockReadGuard<'a, AppData>;

#[derive(Debug, Copy, Clone)]
pub enum FileCacheKey {
    Playlists,
    PlaylistFolders,
    FollowedArtists,
    SavedShows,
    SavedAlbums,
    SavedTracks,
}

/// default time-to-live cache duration
pub static TTL_CACHE_DURATION: LazyLock<std::time::Duration> =
    LazyLock::new(|| std::time::Duration::from_hours(1));

/// the application's data
pub struct AppData {
    pub user_data: UserData,
    pub caches: MemoryCaches,
    pub browse: BrowseData,
}

#[derive(Debug)]
/// current user's data
pub struct UserData {
    pub user: Option<rspotify::model::PrivateUser>,
    pub playlists: Vec<PlaylistFolderItem>,
    pub playlist_folder_node: Option<PlaylistFolderNode>,
    pub followed_artists: Vec<Artist>,
    pub saved_shows: Vec<Show>,
    pub saved_albums: Vec<Album>,
    pub saved_tracks: HashMap<String, Track>,
}

/// the application's in-memory caches
pub struct MemoryCaches {
    pub context: ttl_cache::TtlCache<String, Context>,
    pub search: ttl_cache::TtlCache<String, SearchResults>,
    pub lyrics: ttl_cache::TtlCache<String, Option<Lyrics>>,
    pub genres: ttl_cache::TtlCache<String, Vec<String>>,
    /// Album / show / artist artwork for the playback panel; modest cap — the playback
    /// surface only ever needs one url at a time.
    #[cfg(feature = "image")]
    pub images: ttl_cache::TtlCache<String, image::DynamicImage>,
    /// Episode cover ring buffer. Kept strictly separate from [`Self::images`] so scrolling
    /// a long show can never evict displayed album/show art (or vice versa). Insertion moves
    /// entries to the back, so eviction hits the least-recently-*touched* episode; the moving
    /// prefetch window re-inserts near entries on every selection move, making an evicted
    /// item always one that is far from the pointer. Cap >> window radius.
    #[cfg(feature = "image")]
    pub episode_images: EpisodeImageRing,
}

/// Fixed-capacity LRU keyed by image url, storing small decoded covers.
#[cfg(feature = "image")]
#[derive(Default)]
pub struct EpisodeImageRing {
    buf: std::collections::VecDeque<(String, image::DynamicImage)>,
}

#[cfg(feature = "image")]
impl EpisodeImageRing {
    pub const CAP: usize = 64;

    #[must_use]
    pub fn get(&self, url: &str) -> Option<&image::DynamicImage> {
        self.buf.iter().find(|(u, _)| u == url).map(|(_, img)| img)
    }

    #[must_use]
    pub fn contains(&self, url: &str) -> bool {
        self.buf.iter().any(|(u, _)| u == url)
    }

    /// Insert (or move to back), evicting the least-recently-touched entry when full.
    pub fn insert(&mut self, url: String, img: image::DynamicImage) {
        if let Some(pos) = self.buf.iter().position(|(u, _)| *u == url) {
            self.buf.remove(pos);
        }
        while self.buf.len() >= Self::CAP {
            self.buf.pop_front();
        }
        self.buf.push_back((url, img));
    }
}

impl Default for MemoryCaches {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryCaches {
    #[must_use]
    pub fn new() -> Self {
        Self {
            context: ttl_cache::TtlCache::new(64),
            search: ttl_cache::TtlCache::new(64),
            lyrics: ttl_cache::TtlCache::new(64),
            genres: ttl_cache::TtlCache::new(64),
            #[cfg(feature = "image")]
            images: ttl_cache::TtlCache::new(64),
            #[cfg(feature = "image")]
            episode_images: EpisodeImageRing::default(),
        }
    }
}

#[derive(Default, Debug)]
/// Spotify browse data
pub struct BrowseData {
    pub categories: Vec<Category>,
    pub category_playlists: HashMap<String, Vec<Playlist>>,
}

impl AppData {
    #[must_use]
    pub fn new(cache_folder: &Path) -> Self {
        Self {
            user_data: UserData::new_from_file_caches(cache_folder),
            caches: MemoryCaches::new(),
            browse: BrowseData::default(),
        }
    }

    /// Get a list of tracks inside a given context
    pub fn context_tracks_mut(&mut self, id: &ContextId) -> Option<&mut Vec<Track>> {
        let c = self.caches.context.get_mut(&id.uri())?;

        Some(match c {
            Context::Album { tracks, .. }
            | Context::Playlist { tracks, .. }
            | Context::Tracks { tracks, .. }
            | Context::Artist {
                top_tracks: tracks, ..
            } => tracks,
            Context::Show { .. } => {
                return None;
            }
        })
    }

    #[must_use]
    pub fn context_tracks(&self, id: &ContextId) -> Option<&Vec<Track>> {
        let c = self.caches.context.get(&id.uri())?;
        Some(match c {
            Context::Album { tracks, .. }
            | Context::Playlist { tracks, .. }
            | Context::Tracks { tracks, .. }
            | Context::Artist {
                top_tracks: tracks, ..
            } => tracks,
            Context::Show { .. } => {
                return None;
            }
        })
    }
}

impl UserData {
    /// Construct a new user data based on file caches
    #[must_use]
    pub fn new_from_file_caches(cache_folder: &Path) -> Self {
        Self {
            user: None,
            playlists: load_data_from_file_cache(FileCacheKey::Playlists, cache_folder)
                .unwrap_or_default(),
            playlist_folder_node: load_data_from_file_cache(
                FileCacheKey::PlaylistFolders,
                cache_folder,
            ),
            followed_artists: load_data_from_file_cache(
                FileCacheKey::FollowedArtists,
                cache_folder,
            )
            .unwrap_or_default(),
            saved_shows: load_data_from_file_cache(FileCacheKey::SavedShows, cache_folder)
                .unwrap_or_default(),
            saved_albums: load_data_from_file_cache(FileCacheKey::SavedAlbums, cache_folder)
                .unwrap_or_default(),
            saved_tracks: load_data_from_file_cache(FileCacheKey::SavedTracks, cache_folder)
                .unwrap_or_default(),
        }
    }

    /// Get a list of playlist items that are **possibly** modifiable by user
    ///
    /// If `folder_id` is provided, returns items in the given folder id.
    /// Otherwise, returns the all items.
    #[must_use]
    pub fn modifiable_playlist_items(&self, folder_id: Option<usize>) -> Vec<&PlaylistFolderItem> {
        match self.user {
            None => vec![],
            Some(ref u) => self
                .playlists
                .iter()
                // filter items in a folder (if specified)
                .filter(|item| {
                    if let Some(folder_id) = folder_id {
                        match item {
                            PlaylistFolderItem::Playlist(p) => p.current_folder_id == folder_id,
                            PlaylistFolderItem::Folder(f) => f.current_id == folder_id,
                        }
                    } else {
                        true
                    }
                })
                // filter modifiable items
                .filter(|item| match item {
                    PlaylistFolderItem::Playlist(p) => p.owner.1 == u.id || p.collaborative,
                    PlaylistFolderItem::Folder(_) => true,
                })
                .collect(),
        }
    }

    /// Get playlists items for the given folder id
    #[must_use]
    pub fn folder_playlists_items(&self, folder_id: usize) -> Vec<&PlaylistFolderItem> {
        self.playlists
            .iter()
            .filter(|item| match item {
                PlaylistFolderItem::Playlist(p) => p.current_folder_id == folder_id,
                PlaylistFolderItem::Folder(f) => f.current_id == folder_id,
            })
            .collect()
    }

    /// Check if a track is a liked track
    #[must_use]
    pub fn is_liked_track(&self, track: &Track) -> bool {
        self.saved_tracks.contains_key(&track.id.uri())
    }

    /// Get the user's liked tracks by the given artist, sorted by liked date (newest first)
    #[must_use]
    pub fn liked_tracks_by_artist(&self, artist: &Artist) -> Vec<Track> {
        let mut tracks: Vec<Track> = self
            .saved_tracks
            .values()
            .filter(|t| t.artists.iter().any(|a| a.id == artist.id))
            .cloned()
            .collect();
        tracks.sort_by_key(|t| std::cmp::Reverse(t.added_at));
        tracks
    }

    /// Check if a playlist is followed
    #[must_use]
    pub fn is_followed_playlist(&self, playlist: &Playlist) -> bool {
        self.playlists.iter().any(|x| match x {
            PlaylistFolderItem::Playlist(p) => p.id == playlist.id,
            PlaylistFolderItem::Folder(_) => false,
        })
    }
}

pub fn store_data_into_file_cache<T: Serialize>(
    key: FileCacheKey,
    cache_folder: &Path,
    data: &T,
) -> std::io::Result<()> {
    let path = cache_folder.join(format!("{key:?}_cache.json"));
    let f = BufWriter::new(std::fs::File::create(path)?);
    serde_json::to_writer(f, data)?;
    Ok(())
}

/// # Panics
///
/// Panics if the cache file disappears between the existence check and the open call.
pub fn load_data_from_file_cache<T>(key: FileCacheKey, cache_folder: &Path) -> Option<T>
where
    T: DeserializeOwned,
{
    let path = cache_folder.join(format!("{key:?}_cache.json"));
    if path.exists() {
        tracing::info!("Loading {key:?} data from {}...", path.display());
        let f = BufReader::new(std::fs::File::open(path).expect("path exists"));
        match serde_json::from_reader(f) {
            Ok(data) => {
                tracing::info!("Successfully loaded {key:?} data!");
                Some(data)
            }
            Err(err) => {
                tracing::error!("Failed to load {key:?} data: {err:#}");
                None
            }
        }
    } else {
        None
    }
}

/// Current wall-clock time in milliseconds since the UNIX epoch, saturating to 0.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Path of the on-disk cache entry for a context URI: `<cache>/context/<uri sanitized>.json`.
fn context_cache_path(cache_folder: &Path, uri: &str) -> PathBuf {
    cache_folder
        .join("context")
        .join(format!("{}.json", uri.replace([':', '/'], "-")))
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CachedContext {
    pub fetched_at_ms: u64,
    pub context: Context,
}

impl CachedContext {
    /// Whether this entry is still within [`TTL_CACHE_DURATION`].
    pub fn is_fresh(&self) -> bool {
        now_ms().saturating_sub(self.fetched_at_ms) < TTL_CACHE_DURATION.as_millis() as u64
    }
}

/// Write a context to the on-disk cache under `<cache>/context/`, one file per URI.
pub fn store_context_in_file_cache(
    cache_folder: &Path,
    uri: &str,
    context: &Context,
) -> std::io::Result<()> {
    std::fs::create_dir_all(cache_folder.join("context"))?;
    let path = context_cache_path(cache_folder, uri);
    let cached = CachedContext {
        fetched_at_ms: now_ms(),
        context: context.clone(),
    };
    let f = BufWriter::new(std::fs::File::create(path)?);
    serde_json::to_writer(f, &cached)?;
    Ok(())
}

/// Read a context from the on-disk cache; `None` when the file is missing or unparsable.
pub fn load_context_from_file_cache(cache_folder: &Path, uri: &str) -> Option<CachedContext> {
    let path = context_cache_path(cache_folder, uri);
    if path.exists() {
        let f = BufReader::new(std::fs::File::open(path).ok()?);
        match serde_json::from_reader(f) {
            Ok(data) => Some(data),
            Err(err) => {
                tracing::warn!("Failed to load context {uri} from the file cache: {err:#}");
                None
            }
        }
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::model::{AlbumId, ArtistId, PlaylistId, TrackId, UserId};

    /// A scratch directory unique to this process and test, removed at creation to start clean.
    fn unique_temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "spotify-player-context-cache-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn sample_context() -> Context {
        Context::Playlist {
            playlist: Playlist {
                id: PlaylistId::from_id("37i9dQZF1DXcBWIGoYBM5M").expect("valid playlist id"),
                collaborative: false,
                name: "Daily Mix".to_string(),
                owner: (
                    "me".to_string(),
                    UserId::from_id("spotify").expect("valid user id"),
                ),
                desc: "A test playlist".to_string(),
                current_folder_id: 0,
                snapshot_id: "snap123".to_string(),
            },
            tracks: vec![Track {
                id: TrackId::from_id("1eX57c7iU9Q2b0t6s0kJ2vW").expect("valid track id"),
                name: "Some Song".to_string(),
                artists: vec![Artist {
                    id: ArtistId::from_id("0hXJ7TZ9a5p0MZf2s2X1vJ").expect("valid artist id"),
                    name: "Some Artist".to_string(),
                }],
                album: Some(Album {
                    id: AlbumId::from_id("4aawyAB9vmqN3uQ7FjRGTy").expect("valid album id"),
                    release_date: "2024-01-01".to_string(),
                    name: "Some Album".to_string(),
                    artists: vec![],
                    typ: None,
                    added_at: 0,
                }),
                duration: std::time::Duration::from_secs(180),
                explicit: false,
                added_at: 0,
            }],
        }
    }

    #[test]
    fn context_round_trips_through_file_cache() {
        let dir = unique_temp_dir("roundtrip");
        let uri = "spotify:playlist:37i9dQZF1DXcBWIGoYBM5M";
        let ctx = sample_context();

        store_context_in_file_cache(&dir, uri, &ctx).expect("store should succeed");
        let cached = load_context_from_file_cache(&dir, uri).expect("load should succeed");

        assert!(cached.is_fresh(), "a freshly stored entry must be fresh");
        assert_eq!(
            serde_json::to_value(&cached.context).expect("serialize loaded context"),
            serde_json::to_value(&ctx).expect("serialize original context"),
            "context must survive the file-cache round trip unchanged"
        );

        std::fs::remove_dir_all(&dir).expect("cleanup temp dir");
    }

    #[test]
    fn missing_or_truncated_context_file_yields_none() {
        let dir = unique_temp_dir("truncated");
        let uri = "spotify:album:4aawyAB9vmqN3uQ7FjRGTy";
        let path = context_cache_path(&dir, uri);

        // No file yet: must be a cache miss, not an error.
        assert!(load_context_from_file_cache(&dir, uri).is_none());

        // Garbage / truncated JSON must also yield None rather than panic.
        std::fs::create_dir_all(path.parent().expect("cache path has a parent"))
            .expect("create cache dir");
        std::fs::write(
            &path,
            b"{ \"fetched_at_ms\": 123, \"context\": { \"tracks\": [ ",
        )
        .expect("write truncated file");
        assert!(load_context_from_file_cache(&dir, uri).is_none());

        std::fs::remove_dir_all(&dir).expect("cleanup temp dir");
    }
}
