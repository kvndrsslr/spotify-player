//! The frontend-agnostic redraw signal.
//!
//! A frontend redraws what it draws from [`State`](super::State): pages, popups, the playback
//! window, cover art, logs. Everything that changes any of that raises [`RedrawNotify`], and the
//! frontend awaits it instead of polling, so an idle application renders nothing at all.

use std::{cell::Cell, sync::Arc, time::Duration};

use parking_lot::{Condvar, Mutex};

/// A cheap, coalescing "something a frontend draws has changed" notification.
///
/// Whoever makes new state for a frontend to draw — the client task, the player-event watcher, the
/// CLI socket, os media control, the log buffer, a frontend's own input thread —
/// [`raise`](Self::raise)s it; a frontend [`wait_timeout`](Self::wait_timeout)s for the next raise
/// or its own deadline and redraws. It names no rendering framework: the TUI is not the only
/// frontend, and a frontend that never awaits it is unaffected.
///
/// Raises coalesce: however many arrive before a frontend waits, exactly one wait returns, so a
/// burst of changes costs one redraw. [`raise`](Self::raise) never blocks and never panics, and it
/// may be called while any application-state guard is held: the internal mutex is a *leaf* — no
/// path acquires a `view`/`data`/`player` guard while holding it — so it adds no edge to the
/// acquisition order documented on [`State`](super::State).
#[derive(Clone, Default)]
pub struct RedrawNotify {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    changed: Mutex<bool>,
    cv: Condvar,
}

impl RedrawNotify {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark that a frontend has something new to draw.
    ///
    /// Ignored when raised on the thread currently inside a [`frame`](Self::frame): a frontend's
    /// own render path emitting a log line must not schedule the frame after it, and a draw that
    /// keeps failing must be retried on the retry policy, not on its own error output.
    pub fn raise(&self) {
        if IN_FRAME.with(Cell::get) {
            return;
        }
        *self.inner.changed.lock() = true;
        self.inner.cv.notify_one();
    }

    /// Block until a raise or `timeout` elapses. `true` when a raise was seen.
    ///
    /// The pending flag is consumed either way, so a frontend that then draws owes the state as
    /// of that draw; the next wait blocks afresh.
    #[must_use]
    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        let mut changed = self.inner.changed.lock();
        self.inner
            .cv
            .wait_while_for(&mut changed, |changed| !*changed, timeout);
        std::mem::take(&mut *changed)
    }

    /// Mark this thread as drawing a frame until the returned guard drops.
    ///
    /// Per-thread on purpose: a raise from any *other* thread while a frame draws is a real
    /// change and still schedules the next frame.
    #[must_use]
    pub fn frame(&self) -> FrameGuard {
        IN_FRAME.with(|in_frame| in_frame.set(true));
        FrameGuard
    }
}

/// Clears the drawing flag on drop. See [`RedrawNotify::frame`].
pub struct FrameGuard;

impl Drop for FrameGuard {
    fn drop(&mut self) {
        IN_FRAME.with(|in_frame| in_frame.set(false));
    }
}

thread_local! {
    /// Whether the current thread is inside a frame. See [`RedrawNotify::frame`].
    static IN_FRAME: Cell<bool> = const { Cell::new(false) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wait_without_a_raise_times_out() {
        let notify = RedrawNotify::new();
        assert!(!notify.wait_timeout(Duration::from_millis(10)));
    }

    #[test]
    fn a_raise_returns_from_the_wait_and_is_consumed_once() {
        let notify = RedrawNotify::new();
        notify.raise();
        assert!(notify.wait_timeout(Duration::from_millis(10)));
        assert!(!notify.wait_timeout(Duration::from_millis(10)));
    }

    #[test]
    fn raises_coalesce_into_a_single_wait() {
        let notify = RedrawNotify::new();
        for _ in 0..10 {
            notify.raise();
        }
        assert!(notify.wait_timeout(Duration::from_millis(10)));
        assert!(!notify.wait_timeout(Duration::from_millis(10)));
    }

    #[test]
    fn a_raise_from_inside_a_frame_is_ignored() {
        let notify = RedrawNotify::new();
        {
            let _frame = notify.frame();
            notify.raise();
        }
        assert!(!notify.wait_timeout(Duration::from_millis(10)));
        notify.raise();
        assert!(notify.wait_timeout(Duration::from_millis(10)));
    }

    #[test]
    fn a_raise_from_another_thread_reaches_a_waiting_frontend() {
        let notify = RedrawNotify::new();
        let raiser = notify.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            raiser.raise();
        });
        assert!(notify.wait_timeout(Duration::from_secs(5)));
        handle.join().expect("the raiser thread");
    }
}
