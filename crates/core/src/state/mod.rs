mod constant;
mod data;
mod model;
mod player;
mod queue;
mod view;

use std::{collections::VecDeque, sync::Arc};

pub use constant::*;
pub use data::*;
pub use model::*;
pub use player::*;
#[allow(unused_imports)]
pub use queue::*;
pub use view::*;

use crate::config;

pub use parking_lot::{Mutex, RwLock};

/// Application's shared state
pub type SharedState = Arc<State>;

/// Application's state
pub struct State {
    pub view: Mutex<ViewState>,
    pub player: RwLock<PlayerState>,
    pub data: RwLock<AppData>,

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

    pub is_daemon: bool,

    /// Shared FFT frequency-band data written by the audio sink and read by the UI.
    /// `Some` only when `enable_audio_visualization` is `true`; avoids allocating
    /// the mutex/state entirely when the feature is not in use.
    #[cfg(feature = "streaming")]
    pub vis_bands: Option<Arc<Mutex<crate::visualization::VisBands>>>,

    pub logs: Arc<Mutex<VecDeque<String>>>,
}

impl State {
    pub fn new(is_daemon: bool, log_buffer: Arc<Mutex<VecDeque<String>>>) -> Self {
        let configs = config::get_config();
        let app_data = AppData::new(&configs.cache_folder);

        Self {
            view: Mutex::new(ViewState::default()),
            player: RwLock::new(PlayerState::default()),
            volume_target: Mutex::new(None),
            #[cfg(feature = "image")]
            image_fetch_queue: Mutex::new(Vec::new()),
            #[cfg(feature = "image")]
            image_window_signature: Mutex::new(None),
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
