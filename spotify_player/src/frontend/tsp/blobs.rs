//! Cover art as Tern blobs.
//!
//! Tern addresses an image by the SHA-256 of its bytes, so the frontend encodes each cover url
//! once (core does that, see `State::image_blob`) and names the blob in the `image` node.
//!
//! A document is built while its thread holds guards on the shared state, so a build neither
//! encodes nor writes anything: it records the covers it wants ([`Blobs::want`]), and
//! [`Blobs::resolve`] sends their blobs once the build and its guards are over. Nothing under those
//! guards may take a state lock or touch the terminal.

#[cfg(feature = "image")]
use std::collections::HashMap;

#[cfg(feature = "image")]
use spotify_player_core::state::ImageBlob;
use spotify_player_core::state::SharedState;
use tern_sdk::Session;

/// The blob ids this session has named, and the covers the last build asked for.
///
/// Without the `image` feature there is no cover art to name, so this holds nothing and the
/// session still carries it around: the type is what `build` takes.
#[derive(Default)]
pub struct Blobs {
    /// The blob id of each url whose bytes this session sent.
    #[cfg(feature = "image")]
    ids: HashMap<String, String>,
    /// The urls the last build asked for a cover of, not sent yet.
    #[cfg(feature = "image")]
    wanted: Vec<String>,
}

impl Blobs {
    /// The blob id of `url`, when this session already sent its bytes.
    #[cfg(feature = "image")]
    pub fn known(&self, url: &str) -> Option<String> {
        self.ids.get(url).cloned()
    }

    /// Records that the document wants the cover of `url`, for [`resolve`](Self::resolve).
    ///
    /// A build may not send the blob itself: it runs under the shared state's guards.
    #[cfg(feature = "image")]
    pub fn want(&mut self, url: &str) {
        if !self.wanted.iter().any(|wanted| wanted == url) {
            self.wanted.push(url.to_owned());
        }
    }

    /// Sends the blobs the builds since the last call asked for, and remembers their ids.
    ///
    /// Must run with no `state` lock held: it is what lets a document build stay lock-free while it
    /// draws, and it encodes under the data guard it takes here instead.
    #[cfg(feature = "image")]
    pub fn resolve<M>(&mut self, session: &mut Session<M>, state: &SharedState) {
        let wanted = std::mem::take(&mut self.wanted);

        // Encode (and take the guard the encoding reads through) before any I/O, so no lock is held
        // across a write to the terminal.
        let blobs: Vec<(String, ImageBlob)> = {
            let data = state.data.read();
            wanted
                .into_iter()
                .filter(|url| !self.ids.contains_key(url))
                // `None` when the image is not cached yet: the build queued a fetch, and a later
                // build asks for this url again.
                .filter_map(|url| state.image_blob(&data, &url).map(|blob| (url, blob)))
                .collect()
        };

        for (url, blob) in blobs {
            match session.blob(blob.data.as_ref().clone(), Some(blob.mime)) {
                Ok(id) => {
                    self.ids.insert(url, id);
                }
                Err(err) => tracing::warn!("Failed to send the cover blob for {url}: {err}"),
            }
        }
    }

    /// Without the `image` feature no cover art is named, so there is nothing to resolve.
    #[cfg(not(feature = "image"))]
    pub fn resolve<M>(&mut self, _session: &mut Session<M>, _state: &SharedState) {}
}
