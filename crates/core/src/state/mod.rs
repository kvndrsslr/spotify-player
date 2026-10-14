mod constant;
mod data;
mod model;
mod player;
mod queue;
mod redraw;
mod view;

use std::{collections::VecDeque, sync::Arc};

pub use constant::*;
pub use data::*;
pub use model::*;
pub use player::*;
#[allow(unused_imports)]
pub use queue::*;
pub use redraw::*;
pub use view::*;

use crate::config;

pub use parking_lot::{Mutex, RwLock};

/// Application's shared state
pub type SharedState = Arc<State>;

/// Application's state.
///
/// The guards `view`, `data` and `player` are taken in that order by every path that holds more
/// than one at a time, and no path may hold two guards of the *same* lock: the client task, the
/// player-event watcher, the media-control thread and a frontend's render loop run concurrently, so
/// a reverse acquisition (a `data` guard taken under `player`, say) closes a cycle with the client
/// task's writes and parks both threads.
///
/// [`State::redraw`] is not part of that order: it is a leaf, taken only inside
/// [`RedrawNotify::raise`]/[`RedrawNotify::wait_timeout`] themselves, never around a state guard.
pub struct State {
    pub view: Mutex<ViewState>,
    pub player: RwLock<PlayerState>,
    pub data: RwLock<AppData>,

    /// The frontend-agnostic "something to draw changed" signal. See [`RedrawNotify`]: every path
    /// that changes what a frontend draws raises it, and a frontend awaits it instead of polling.
    pub redraw: RedrawNotify,

    /// The last locally commanded volume target. Volume events compute against this instead
    /// of the racy shared `buffered_playback`, so rapid events (scroll wheel) can't step on
    /// each other; the client task clears it once the change is applied.
    pub volume_target: Mutex<Option<u8>>,

    /// Image urls awaiting a background fetch, with retry attempt counts. Render paths enqueue
    /// cache misses here plus the moving prefetch window around the selected episode; the
    /// client task drains them.
    #[cfg(feature = "image")]
    pub image_fetch_queue: Mutex<Vec<(String, u8)>>,
    /// Endpoints of the last enqueued episode-image window, used to detect selection moves
    /// without re-enqueueing every frame.
    #[cfg(feature = "image")]
    pub image_window_signature: Mutex<Option<(String, String)>>,
    /// The encoded cover art every frontend names by id, keyed by image url (see
    /// [`State::image_blob`]).
    ///
    /// Deliberately not in [`AppData`]: a frontend draws cover art while it *holds* a guard on the
    /// data cache, and filling this cache must not need another `data` lock.
    #[cfg(feature = "image")]
    encoded_images: Mutex<ttl_cache::TtlCache<String, ImageBlob>>,

    pub is_daemon: bool,

    /// Shared FFT frequency-band data written by the audio sink and read by the UI.
    /// `Some` only when `enable_audio_visualization` is `true`; avoids allocating
    /// the mutex/state entirely when the feature is not in use.
    #[cfg(feature = "streaming")]
    pub vis_bands: Option<Arc<Mutex<crate::visualization::VisBands>>>,

    pub logs: Arc<Mutex<VecDeque<String>>>,
}

impl State {
    /// `redraw` is the shared frontend signal; the logging layer holds a clone of it, so it is
    /// built by the caller and passed to both.
    pub fn new(
        is_daemon: bool,
        log_buffer: Arc<Mutex<VecDeque<String>>>,
        redraw: RedrawNotify,
    ) -> Self {
        let configs = config::get_config();
        let app_data = AppData::new(&configs.cache_folder);

        Self {
            view: Mutex::new(ViewState::default()),
            player: RwLock::new(PlayerState::default()),
            redraw,
            volume_target: Mutex::new(None),
            #[cfg(feature = "image")]
            image_fetch_queue: Mutex::new(Vec::new()),
            #[cfg(feature = "image")]
            image_window_signature: Mutex::new(None),
            #[cfg(feature = "image")]
            encoded_images: Mutex::new(ttl_cache::TtlCache::new(16)),
            data: RwLock::new(app_data),
            is_daemon,
            #[cfg(feature = "streaming")]
            vis_bands: if configs.app_config.enable_audio_visualization {
                Some(Arc::new(Mutex::new(
                    crate::visualization::VisBands::default(),
                )))
            } else {
                None
            },

            logs: log_buffer,
        }
    }

    /// The encoded, content-addressed bytes of a cached cover image, or `None` when the url's
    /// image is not in `data` yet — queue it with [`State::queue_image_fetch`] and retry.
    ///
    /// Encodes once per url, into the cache behind [`State::encoded_images`].
    ///
    /// Takes the data guard the caller already holds instead of locking `data` itself: cover art is
    /// drawn *while* a guard on that cache is alive, and a nested acquisition would block forever on
    /// the guard the caller's own thread holds (a write lock can never be granted then, and a read
    /// lock waits for the queued writer that our read is blocking). Reading the image through
    /// `data` and encoding it here keeps this call lock-free with respect to the data cache.
    #[cfg(feature = "image")]
    pub fn image_blob(&self, data: &AppData, url: &str) -> Option<ImageBlob> {
        if let Some(blob) = self.encoded_images.lock().get(url) {
            return Some(blob.clone());
        }

        // Whichever cache holds the decoded image, mirroring the render paths.
        let image = data
            .caches
            .images
            .get(url)
            .or_else(|| data.caches.episode_images.get(url))?;

        match ImageBlob::encode(image) {
            Ok(blob) => {
                self.encoded_images.lock().insert(
                    url.to_owned(),
                    blob.clone(),
                    *TTL_CACHE_DURATION,
                );
                Some(blob)
            }
            Err(err) => {
                tracing::warn!("Failed to encode the cover image for {url}: {err:#}");
                None
            }
        }
    }

    /// Queue an image url for background fetching; retried url bumps its attempt count.
    #[cfg(feature = "image")]
    pub fn queue_image_fetch(&self, url: &str) {
        const MAX_PENDING: usize = 32;
        let mut queue = self.image_fetch_queue.lock();
        if let Some(entry) = queue.iter_mut().find(|(u, _)| u == url) {
            entry.1 = entry.1.saturating_add(1);
        } else if queue.len() < MAX_PENDING {
            queue.push((url.to_owned(), 0));
        }
    }

    /// Record the episode-image window that was last enqueued, returning whether it changed.
    ///
    /// Render paths use this to detect selection moves without re-enqueueing every frame.
    #[cfg(feature = "image")]
    pub fn update_image_window(&self, first: String, last: String) -> bool {
        let mut signature = self.image_window_signature.lock();
        if signature.as_ref() == Some(&(first.clone(), last.clone())) {
            return false;
        }
        *signature = Some((first, last));
        true
    }

    #[cfg(feature = "streaming")]
    pub fn is_streaming_enabled(&self) -> bool {
        let configs = config::get_config();
        configs.app_config.enable_streaming == config::StreamingType::Always
            || (configs.app_config.enable_streaming == config::StreamingType::DaemonOnly
                && self.is_daemon)
    }

    /// Returns `true` when the custom queue system should be used for new playback.
    ///
    /// Requires streaming to be enabled and the `custom_queue` config option
    /// to be `true`.
    #[cfg(feature = "streaming")]
    #[allow(dead_code)]
    pub fn should_use_custom_queue(&self) -> bool {
        self.is_streaming_enabled() && config::get_config().app_config.custom_queue
    }

    /// Returns `true` when the local librespot player is actively streaming
    /// audio (i.e. a `Playing` event has been received and no `Paused` / `stop`
    /// has occurred since).  Used by the UI to decide whether to allocate and
    /// render the audio-visualization area.
    #[cfg(feature = "streaming")]
    pub fn is_local_streaming_active(&self) -> bool {
        self.vis_bands.as_ref().is_some_and(|b| b.lock().is_active)
    }
}
