//! Client-wide pacing for Spotify Web API requests.
//!
//! The Web API attributes every request of an application to one quota. Firing the startup
//! burst (library pagination) and the UI-gating reads (playback, devices, queue) concurrently
//! exhausts that quota within seconds, and every `429` then retried independently turns into a
//! second, self-sustaining burst. [`RateLimiter`] spreads normal traffic out and turns any
//! observed `429` into a single client-wide pause, so the UI-gating reads wait for one shared
//! cooldown instead of each backing off and retrying on its own schedule.
//!
//! On top of that shared pause: [`Priority::High`]/[`Priority::Normal`] work may spend a single
//! *probe* per cooldown window so a user action is never stuck behind a pause armed by
//! unrelated traffic, [`Priority::Background`] maintenance is *deferred* (skipped, not queued)
//! while the client is cooling down or backed off, missing `Retry-After`s escalate across
//! consecutive failures, and the `Normal` spacing adapts so a post-cooldown queue cannot resume
//! as one instantaneous burst.

use std::time::Duration;

use tokio::time::Instant;

/// Minimum spacing between two paced ([`Priority::Normal`]) requests.
const REQUEST_SPACING: Duration = Duration::from_millis(200);
/// Upper bound on the adaptive [`Priority::Normal`] spacing reached after repeated 429s.
const MAX_SPACING: Duration = Duration::from_secs(10);
/// Bounds applied to a server-supplied `Retry-After` before use as a client-wide cooldown.
const MIN_COOLDOWN: Duration = Duration::from_secs(1);
const MAX_COOLDOWN: Duration = Duration::from_secs(60);
/// Cooldowns at or below this are transient hiccups worth retrying inside the same request;
/// a longer one means the quota window is exhausted for every request sharing it.
pub const TRANSIENT_COOLDOWN: Duration = Duration::from_secs(5);

/// Request class.
///
/// * `High` — UI-gating reads (`/me/player`, devices, queue, current user).
/// * `Normal` — anything the user explicitly asked for (`GetContext` fetches, search, browse).
/// * `Background` — library refreshes and other maintenance; deferred rather than queued while
///   the client is cooling down or backed off.
///
/// `High` and `Normal` may probe an active cooldown once per window; `Background` never probes
/// and never delays the other classes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Priority {
    Normal,
    High,
    Background,
}

/// Result of a paced [`RateLimiter::acquire`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcquireOutcome {
    /// The request may be sent now.
    Ready,
    /// A [`Priority::Background`] request was skipped because the client is cooling down or
    /// backed off. The caller must not send anything and should retry later.
    Deferred,
}

#[derive(Debug)]
pub struct RateLimiter {
    state: parking_lot::Mutex<State>,
}

#[derive(Debug)]
struct State {
    /// Earliest instant a new [`Priority::Normal`] request may be sent.
    next_slot: Instant,
    /// Earliest instant a new [`Priority::Background`] request may be sent. Kept separate from
    /// `next_slot` so background work can never delay `High`/`Normal` requests.
    background_next_slot: Instant,
    /// Instant until which no request of any priority may be sent (except a single probe).
    cooldown_until: Instant,
    /// Whether the current cooldown window has already been probed by a `High`/`Normal`
    /// request. Reset when a window ends (success) or a new one begins (429).
    probed: bool,
    /// Current spacing between [`Priority::Normal`] requests; grows on each 429 and decays
    /// back toward [`REQUEST_SPACING`] as requests succeed.
    spacing: Duration,
    /// Consecutive 429s since the last successful request, driving the cooldown/spacing
    /// escalation.
    consecutive_429s: u32,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl RateLimiter {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            state: parking_lot::Mutex::new(State {
                next_slot: now,
                background_next_slot: now,
                cooldown_until: now,
                probed: false,
                spacing: REQUEST_SPACING,
                consecutive_429s: 0,
            }),
        }
    }

    /// Block until this request may be sent, returning [`AcquireOutcome::Deferred`] instead of
    /// waiting for a [`Priority::Background`] request that must be skipped.
    ///
    /// A `High`/`Normal` request may consume the current cooldown window's single probe (see
    /// [`Self::acquire_after_rate_limit`]). Reservation happens under the lock; the wait
    /// happens outside it and re-reads the state, so a cooldown set while sleeping is honored.
    pub async fn acquire(&self, priority: Priority) -> AcquireOutcome {
        self.acquire_inner(priority, true).await
    }

    /// Like [`Self::acquire`], but never consumes the cooldown window's probe.
    ///
    /// The retry loop after a 429 uses this: the request's own attempt already probed the
    /// window it armed, so it must wait the cooldown out rather than re-probing immediately.
    pub async fn acquire_after_rate_limit(&self, priority: Priority) -> AcquireOutcome {
        self.acquire_inner(priority, false).await
    }

    async fn acquire_inner(&self, priority: Priority, allow_probe: bool) -> AcquireOutcome {
        loop {
            let now = Instant::now();
            // The guard must not be live across the `await` below: `parking_lot::MutexGuard`
            // is not `Send`, and holding it would make `acquire`'s future non-`Send`, which in
            // turn breaks every `tokio::task::spawn` that awaits it. Scope it to a block.
            let wait = {
                let mut state = self.state.lock();
                let cooling_down = state.cooldown_until > now;
                if !cooling_down {
                    // The previous window has ended, so a later one may be probed again.
                    state.probed = false;
                }

                if priority == Priority::Background {
                    // Defer rather than queue: never wait out (or extend) a cooldown for
                    // maintenance work. The refresh trigger retries it later.
                    if cooling_down || state.consecutive_429s > 0 {
                        return AcquireOutcome::Deferred;
                    }
                    if state.background_next_slot <= now {
                        state.background_next_slot = now + state.spacing;
                        return AcquireOutcome::Ready;
                    }
                    state.background_next_slot.saturating_duration_since(now)
                } else {
                    // One probe per cooldown window: the first `High`/`Normal` request may
                    // ignore an active cooldown to find out whether the API has recovered, so a
                    // user action is never stuck behind a pause armed by unrelated traffic.
                    if cooling_down && allow_probe && !state.probed {
                        state.probed = true;
                        return AcquireOutcome::Ready;
                    }
                    let paced_at = match priority {
                        Priority::Normal => state.next_slot,
                        // `High` never waits for the pacing clock.
                        Priority::High => now,
                        Priority::Background => unreachable!("handled above"),
                    };
                    let ready_at = state.cooldown_until.max(paced_at);
                    if ready_at <= now {
                        if priority == Priority::Normal {
                            state.next_slot = now + state.spacing;
                        }
                        return AcquireOutcome::Ready;
                    }
                    ready_at.saturating_duration_since(now)
                }
            };
            tokio::time::sleep(wait).await;
        }
    }

    /// Register a 429 and return the cooldown it applied: every later request waits out
    /// `retry_after`, clamped to `MIN_COOLDOWN..=MAX_COOLDOWN`. When the server omits it, the
    /// cooldown escalates across consecutive failures (`MIN_COOLDOWN` doubled each time,
    /// capped) instead of re-429ing on the flat minimum. Also raises the adaptive `Normal`
    /// spacing.
    ///
    /// The returned value is the pause *this* response asked for (not the possibly-longer
    /// remaining cooldown), so the caller can decide whether to retry in place; see
    /// [`should_retry_in_place`].
    pub fn on_rate_limited(&self, retry_after: Option<Duration>) -> Duration {
        let now = Instant::now();
        let mut state = self.state.lock();
        state.consecutive_429s = state.consecutive_429s.saturating_add(1);
        if state.cooldown_until <= now {
            // A fresh window begins, so it may be probed. If a window is still active it keeps
            // its probe state: a probe that just failed must not be repeated by its own retry.
            state.probed = false;
        }

        let cooldown = retry_after.map_or_else(
            || escalated_cooldown(state.consecutive_429s),
            |retry_after| retry_after.clamp(MIN_COOLDOWN, MAX_COOLDOWN),
        );

        // Spread `Normal` traffic further apart too: an instantaneous post-cooldown burst is
        // what re-arms the limit. Raise the spacing to at least `cooldown / 8`, doubling per 429.
        let floor = REQUEST_SPACING.max(cooldown / 8);
        state.spacing = state.spacing.saturating_mul(2).max(floor).min(MAX_SPACING);

        let until = now + cooldown;
        if until > state.cooldown_until {
            state.cooldown_until = until;
        }

        cooldown
    }

    /// Register a successful request: the API answered, so clear the cooldown early (queued
    /// requests no longer wait out a pause that no longer applies), reset the 429 escalation,
    /// and decay the adaptive spacing back toward [`REQUEST_SPACING`].
    pub fn on_success(&self) {
        let now = Instant::now();
        let mut state = self.state.lock();
        state.consecutive_429s = 0;
        state.probed = false;
        if state.cooldown_until > now {
            state.cooldown_until = now;
        }
        state.spacing = REQUEST_SPACING.max(state.spacing / 2);
    }
}

/// Cooldown for a 429 without `Retry-After`: [`MIN_COOLDOWN`] doubled per consecutive failure,
/// capped at [`MAX_COOLDOWN`].
fn escalated_cooldown(consecutive_429s: u32) -> Duration {
    let shift = consecutive_429s.saturating_sub(1).min(6);
    MIN_COOLDOWN
        .saturating_mul(1u32 << shift)
        .clamp(MIN_COOLDOWN, MAX_COOLDOWN)
}

/// Whether a 429 whose applied cooldown was `applied` is worth retrying inside the same
/// request.
///
/// A short cooldown is a transient hiccup. A long one means the quota window is exhausted for
/// everyone sharing it, so holding the request's task for tens of seconds per attempt wins
/// nothing: the caller's own cadence (the page-change re-issue, or the next user action)
/// retries far more cheaply, and each of those retries gets its own single probe.
pub fn should_retry_in_place(applied: Duration) -> bool {
    applied <= TRANSIENT_COOLDOWN
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test(start_paused = true)]
    async fn normal_requests_are_spaced_out() {
        let limiter = RateLimiter::new();
        let start = Instant::now();

        for _ in 0..3 {
            limiter.acquire(Priority::Normal).await;
        }

        assert!(start.elapsed() >= 2 * REQUEST_SPACING);
    }

    #[tokio::test(start_paused = true)]
    async fn high_priority_skips_the_pacing_queue() {
        let limiter = Arc::new(RateLimiter::new());
        // Seed the pacing clock, so a later `Normal` request has to wait for its slot.
        limiter.acquire(Priority::Normal).await;

        let queued = {
            let limiter = limiter.clone();
            tokio::spawn(async move {
                let start = Instant::now();
                limiter.acquire(Priority::Normal).await;
                start.elapsed()
            })
        };
        tokio::task::yield_now().await;

        let start = Instant::now();
        limiter.acquire(Priority::High).await;
        assert!(start.elapsed() < REQUEST_SPACING);

        assert!(queued.await.unwrap() >= REQUEST_SPACING);
    }

    #[tokio::test(start_paused = true)]
    async fn one_probe_per_cooldown_window() {
        let limiter = RateLimiter::new();
        limiter.on_rate_limited(Some(Duration::from_secs(30)));

        // The first `High`/`Normal` request probes the cooldown and returns immediately.
        let start = Instant::now();
        assert_eq!(
            limiter.acquire(Priority::Normal).await,
            AcquireOutcome::Ready
        );
        assert_eq!(start.elapsed(), Duration::ZERO);

        // A second request in the same window waits the cooldown out.
        let start = Instant::now();
        assert_eq!(
            limiter.acquire(Priority::Normal).await,
            AcquireOutcome::Ready
        );
        assert_eq!(start.elapsed(), Duration::from_secs(30));

        // A successful probe clears the cooldown early: later requests are unpaced.
        let limiter = RateLimiter::new();
        limiter.on_rate_limited(Some(Duration::from_secs(30)));
        limiter.acquire(Priority::Normal).await; // probe
        limiter.on_success(); // the probe answered
        let start = Instant::now();
        assert_eq!(limiter.acquire(Priority::High).await, AcquireOutcome::Ready);
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn background_requests_defer_during_cooldown() {
        let limiter = RateLimiter::new();
        limiter.on_rate_limited(Some(Duration::from_secs(30)));

        let snapshot = {
            let state = limiter.state.lock();
            (
                state.next_slot,
                state.background_next_slot,
                state.cooldown_until,
                state.consecutive_429s,
            )
        };

        // Deferred immediately, costing no wait...
        let start = Instant::now();
        assert_eq!(
            limiter.acquire(Priority::Background).await,
            AcquireOutcome::Deferred
        );
        assert_eq!(start.elapsed(), Duration::ZERO);

        // ...and neither advancing the background clock nor extending the cooldown.
        let after = {
            let state = limiter.state.lock();
            (
                state.next_slot,
                state.background_next_slot,
                state.cooldown_until,
                state.consecutive_429s,
            )
        };
        assert_eq!(snapshot, after);

        // Even once the cooldown elapses, background work stays deferred until the client is
        // provably healthy again (a successful request clears the backs-off state).
        tokio::time::sleep(Duration::from_secs(30)).await;
        assert_eq!(
            limiter.acquire(Priority::Background).await,
            AcquireOutcome::Deferred
        );
        limiter.on_success();
        assert_eq!(
            limiter.acquire(Priority::Background).await,
            AcquireOutcome::Ready
        );
    }

    #[tokio::test(start_paused = true)]
    async fn missing_retry_after_escalates_and_success_resets() {
        let limiter = RateLimiter::new();

        // 1s, 2s, 4s ... across consecutive 429s without `Retry-After`.
        for expected in [1u64, 2, 4] {
            limiter.on_rate_limited(None);
            // Spend the probe, then measure the cooldown the next request waits out.
            limiter.acquire(Priority::High).await;
            let start = Instant::now();
            limiter.acquire(Priority::High).await;
            assert_eq!(start.elapsed(), Duration::from_secs(expected));
        }

        // A success resets the escalation, so the next 429 starts at the minimum again.
        limiter.on_success();
        limiter.on_rate_limited(None);
        limiter.acquire(Priority::High).await; // probe
        let start = Instant::now();
        limiter.acquire(Priority::High).await;
        assert_eq!(start.elapsed(), MIN_COOLDOWN);
    }

    #[tokio::test(start_paused = true)]
    async fn queued_normal_requests_do_not_burst_after_cooldown() {
        let limiter = Arc::new(RateLimiter::new());
        // An 8s `Retry-After` raises the spacing floor to `max(REQUEST_SPACING, 8s / 8) = 1s`.
        limiter.on_rate_limited(Some(Duration::from_secs(8)));

        let mut handles = Vec::new();
        for _ in 0..5 {
            let limiter = limiter.clone();
            handles.push(tokio::spawn(async move {
                limiter.acquire(Priority::Normal).await;
                Instant::now()
            }));
        }
        let mut times = Vec::new();
        for handle in handles {
            times.push(handle.await.unwrap());
        }
        times.sort();

        // One request probes the cooldown; the rest wait it out and are then spread by the
        // raised spacing instead of all resuming in one instant.
        let spacing = Duration::from_secs(1);
        let span = times.last().unwrap().duration_since(times[0]);
        assert!(span >= spacing * 4, "post-cooldown burst span {span:?}");
        for pair in times.windows(2) {
            let gap = pair[1].duration_since(pair[0]);
            assert!(gap >= spacing, "requests not spread out: {gap:?}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn high_priority_is_not_delayed_by_normal_reservations() {
        let limiter = RateLimiter::new();
        // Raise the `Normal` spacing via a 429, probe the window, then reserve a `Normal` slot.
        limiter.on_rate_limited(Some(Duration::from_secs(8)));
        limiter.acquire(Priority::High).await; // probe
        limiter.acquire(Priority::Normal).await; // waits the cooldown, reserves the next slot

        let start = Instant::now();
        limiter.acquire(Priority::High).await;
        assert_eq!(start.elapsed(), Duration::ZERO);

        // ...while a following `Normal` request still has to wait for its reserved slot.
        let start = Instant::now();
        limiter.acquire(Priority::Normal).await;
        assert!(start.elapsed() >= Duration::from_secs(1));
    }

    #[test]
    fn on_rate_limited_reports_the_applied_cooldown() {
        let limiter = RateLimiter::new();

        // A server-supplied `Retry-After` is clamped and applied verbatim.
        assert_eq!(
            limiter.on_rate_limited(Some(Duration::from_secs(29))),
            Duration::from_secs(29)
        );
        // A missing one escalates: second consecutive failure -> 2s.
        assert_eq!(
            limiter.on_rate_limited(None),
            Duration::from_secs(2),
            "escalation continues across consecutive failures"
        );
    }

    #[test]
    fn transient_cooldowns_retry_in_place() {
        // `Retry-After: 1s` (the transient case) and anything up to the threshold retry.
        assert!(should_retry_in_place(Duration::from_secs(1)));
        assert!(should_retry_in_place(TRANSIENT_COOLDOWN));
        // `Retry-After: 27-29s` (the exhausted-window case) must NOT hold the request.
        assert!(!should_retry_in_place(Duration::from_secs(27)));
        assert!(!should_retry_in_place(Duration::from_secs(29)));
        assert!(!should_retry_in_place(MAX_COOLDOWN));
    }

    #[test]
    fn escalation_without_retry_after_eventually_exceeds_the_threshold() {
        // 1s, 2s, 4s retry in place; 8s (and up) bail, so a run of missing-`Retry-After`
        // 429s cannot pin the request for minutes.
        assert!(should_retry_in_place(escalated_cooldown(1)));
        assert!(should_retry_in_place(escalated_cooldown(2)));
        assert!(should_retry_in_place(escalated_cooldown(3)));
        assert!(!should_retry_in_place(escalated_cooldown(4)));
        assert!(!should_retry_in_place(escalated_cooldown(8)));
    }
}
