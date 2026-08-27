//! Debounced application of remote volume changes.
//!
//! Volume adjustments targeting devices outside this process must travel through the Spotify
//! Web API, which rate-limits aggressively. Key repeats and mouse scrolling produce bursts of
//! dozens of updates per second; every burst collapses to a single request carrying the latest
//! value once the input stream goes quiet for [`SETTLE_WINDOW`].
//!
//! Assumption: bursts target one device at a time. A transfer mid-burst can coalesce across
//! devices into the head device's cycle; server state reconciles within seconds via the
//! periodic playback poll, which vastly outvalues protecting against that rare case.

use std::{sync::Arc, time::Duration};

use futures::future::BoxFuture;
use tokio::{select, sync::mpsc, time};

/// Quiet period a volume-update stream must reach before exactly one Web API call fires.
const SETTLE_WINDOW: Duration = Duration::from_millis(150);

type ApplyFn = Arc<dyn Fn(Option<String>, u8) -> BoxFuture<'static, ()> + Send + Sync>;

/// Coalesces bursts of absolute-volume updates into one application per quiet window.
#[derive(Clone)]
pub struct RemoteVolume {
    tx: mpsc::Sender<(Option<String>, u8)>,
}

impl RemoteVolume {
    /// Spawns the debounce worker; must be called inside the tokio runtime.
    pub fn new<F>(apply: F) -> Self
    where
        F: Fn(Option<String>, u8) -> BoxFuture<'static, ()> + Send + Sync + 'static,
    {
        let apply: ApplyFn = Arc::new(apply);
        // Bounded so a wedged apply cannot grow memory unboundedly; blocking pushes are fine
        // because the consumer always drains.
        let (tx, mut rx) = mpsc::channel::<(Option<String>, u8)>(64);

        tokio::spawn(async move {
            while let Some((head_device, mut latest)) = rx.recv().await {
                // Restart the settle window until no new value arrives within it.
                loop {
                    select! {
                        () = time::sleep(SETTLE_WINDOW) => break,
                        arrived = rx.recv() => {
                            match arrived {
                                None => return,
                                Some((_, volume)) => latest = volume,
                            }
                        }
                    }
                }
                apply(head_device, latest).await;
            }
        });

        Self { tx }
    }

    /// Enqueue an absolute volume percent for eventual coalesced application.
    pub async fn push(&self, device: Option<String>, volume_percent: u8) {
        if let Err(err) = self.tx.send((device, volume_percent)).await {
            tracing::warn!("Remote volume update dropped, debounce worker gone: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

    use std::sync::LazyLock;

    static BURST_LOG: LazyLock<Mutex<Vec<u8>>> = LazyLock::new(|| Mutex::new(Vec::new()));
    static SPREAD_LOG: LazyLock<Mutex<Vec<u8>>> = LazyLock::new(|| Mutex::new(Vec::new()));

    fn recording_apply(
        log: &'static LazyLock<Mutex<Vec<u8>>>,
    ) -> impl Fn(Option<String>, u8) -> BoxFuture<'static, ()> {
        move |_device, volume| {
            log.lock().push(volume);
            Box::pin(async {})
        }
    }

    #[tokio::test]
    async fn collapses_bursts_into_latest_value() {
        let queue = RemoteVolume::new(recording_apply(&BURST_LOG));

        for volume in [40_u8, 45, 50, 60] {
            queue.push(None, volume).await;
            time::sleep(Duration::from_millis(10)).await;
        }
        // Enough time for the settle window after the last update to elapse and apply.
        time::sleep(SETTLE_WINDOW * 4).await;

        assert_eq!(
            *BURST_LOG.lock(),
            vec![60],
            "burst should collapse to the last value"
        );
    }

    #[tokio::test]
    async fn applies_each_value_when_updates_are_spread_out() {
        let queue = RemoteVolume::new(recording_apply(&SPREAD_LOG));

        queue.push(None, 30).await;
        time::sleep(SETTLE_WINDOW * 4).await;
        queue.push(None, 80).await;
        time::sleep(SETTLE_WINDOW * 4).await;

        assert_eq!(
            *SPREAD_LOG.lock(),
            vec![30, 80],
            "isolated updates each apply once"
        );
    }
}
