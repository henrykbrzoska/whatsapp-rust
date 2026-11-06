// Ported from the Go client `keepalive.go`, © Tulir Asokan, MPL-2.0 —
// see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! The WebSocket keep-alive loop (`keepAliveLoop` / `sendKeepAlive`).
//!
//! Every [`KeepAliveConfig::interval_min`]..=[`KeepAliveConfig::interval_max`]
//! (Go: 20–30 s, randomized) an `<iq type="get" xmlns="w:p">` ping is sent to the
//! server JID and we wait up to [`KeepAliveConfig::response_deadline`] (10 s) for
//! the response:
//!
//! * response → [`WaEvent::KeepAliveRestored`] if the previous ping had failed;
//! * no response → [`WaEvent::KeepAliveTimeout`] with a running error count, and
//!   once nothing has succeeded for [`KeepAliveConfig::max_fail_time`] (3 min) the
//!   reconnect hook is invoked.
//!
//! The loop is transport-agnostic: the ping and the force-reconnect action are
//! injected, so this module needs neither a socket nor the IQ plumbing and can be
//! unit-tested with a fake clock-free ping.

use std::future::Future;
use std::time::{Duration, Instant};

use rand::Rng;

use crate::types::events::WaEvent;

/// Go: `KeepAliveIntervalMin` — 20 s.
pub const KEEPALIVE_INTERVAL_MIN: Duration = Duration::from_secs(20);
/// Go: `KeepAliveIntervalMax` — 30 s.
pub const KEEPALIVE_INTERVAL_MAX: Duration = Duration::from_secs(30);
/// Go: `KeepAliveResponseDeadline` — 10 s.
pub const KEEPALIVE_RESPONSE_DEADLINE: Duration = Duration::from_secs(10);
/// Go: `KeepAliveMaxFailTime` — 3 min without a successful ping forces a reconnect.
pub const KEEPALIVE_MAX_FAIL_TIME: Duration = Duration::from_secs(180);

/// Result of one ping round-trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PingOutcome {
    /// The server answered in time.
    Answered,
    /// The response deadline elapsed.
    TimedOut,
    /// The connection went away while waiting; the loop must stop.
    Disconnected,
}

/// Timing knobs of the loop (the Go package-level variables).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeepAliveConfig {
    pub interval_min: Duration,
    pub interval_max: Duration,
    pub response_deadline: Duration,
    pub max_fail_time: Duration,
    /// When false, failing keep-alives never trigger a reconnect (Go's
    /// `EnableAutoReconnect`).
    pub auto_reconnect: bool,
    /// Randomize the interval inside `[interval_min, interval_max]` like Go does.
    /// Disable for deterministic tests.
    pub jitter: bool,
}

impl Default for KeepAliveConfig {
    fn default() -> Self {
        Self {
            interval_min: KEEPALIVE_INTERVAL_MIN,
            interval_max: KEEPALIVE_INTERVAL_MAX,
            response_deadline: KEEPALIVE_RESPONSE_DEADLINE,
            max_fail_time: KEEPALIVE_MAX_FAIL_TIME,
            auto_reconnect: true,
            jitter: true,
        }
    }
}

impl KeepAliveConfig {
    /// The delay before the next ping (Go: `rand.Int64N(max-min) + min`).
    pub fn next_interval(&self) -> Duration {
        if !self.jitter || self.interval_max <= self.interval_min {
            return self.interval_min;
        }
        let min = self.interval_min.as_millis() as u64;
        let max = self.interval_max.as_millis() as u64;
        Duration::from_millis(rand::thread_rng().gen_range(min..max))
    }
}

/// Why the loop stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepAliveExit {
    /// The shutdown signal fired (Go: `connCtx.Done()`).
    Shutdown,
    /// A ping found the connection gone (`shouldContinue == false`).
    Disconnected,
}

/// The keep-alive loop itself.
///
/// `ping` performs one `<iq>` round-trip; `emit` publishes events to the UI;
/// `force_reconnect` is invoked when keep-alives have been failing for
/// [`KeepAliveConfig::max_fail_time`].
pub struct KeepAlive<P, E, R> {
    config: KeepAliveConfig,
    ping: P,
    emit: E,
    force_reconnect: R,
    last_success: Instant,
    error_count: u32,
}

impl<P, E, R, F> KeepAlive<P, E, R>
where
    P: Fn() -> F,
    F: Future<Output = PingOutcome>,
    E: Fn(WaEvent),
    R: Fn(),
{
    /// Build a loop. `ping` must not block longer than the response deadline —
    /// the deadline is applied here.
    pub fn new(config: KeepAliveConfig, ping: P, emit: E, force_reconnect: R) -> Self {
        Self { config, ping, emit, force_reconnect, last_success: Instant::now(), error_count: 0 }
    }

    /// Go: `sendKeepAlive` — one ping bounded by the response deadline.
    async fn send_keep_alive(&self) -> PingOutcome {
        match tokio::time::timeout(self.config.response_deadline, (self.ping)()).await {
            Ok(outcome) => outcome,
            Err(_) => PingOutcome::TimedOut,
        }
    }

    /// One iteration of Go's `keepAliveLoop`. Returns `None` to continue, or the
    /// reason to stop.
    async fn tick(&mut self) -> Option<KeepAliveExit> {
        match self.send_keep_alive().await {
            PingOutcome::Answered => {
                if self.error_count > 0 {
                    self.error_count = 0;
                    (self.emit)(WaEvent::KeepAliveRestored);
                }
                self.last_success = Instant::now();
                None
            }
            PingOutcome::TimedOut => {
                self.error_count += 1;
                (self.emit)(WaEvent::KeepAliveTimeout { error_count: self.error_count });
                if self.config.auto_reconnect && self.last_success.elapsed() > self.config.max_fail_time {
                    tracing::debug!("forcing reconnect due to keepalive failure");
                    (self.force_reconnect)();
                }
                None
            }
            PingOutcome::Disconnected => Some(KeepAliveExit::Disconnected),
        }
    }

    /// Run until `shutdown` completes or the connection disappears.
    pub async fn run<S>(mut self, shutdown: S) -> KeepAliveExit
    where
        S: Future<Output = ()>,
    {
        tokio::pin!(shutdown);
        loop {
            let interval = self.config.next_interval();
            tokio::select! {
                biased;
                () = &mut shutdown => return KeepAliveExit::Shutdown,
                () = tokio::time::sleep(interval) => {}
            }
            // The ping itself must also be cancellable by the shutdown signal.
            let outcome = tokio::select! {
                biased;
                () = &mut shutdown => return KeepAliveExit::Shutdown,
                outcome = self.tick() => outcome,
            };
            if let Some(exit) = outcome {
                return exit;
            }
        }
    }

    /// Number of consecutive failed pings (for diagnostics/tests).
    pub fn error_count(&self) -> u32 {
        self.error_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    };

    fn fast_config() -> KeepAliveConfig {
        KeepAliveConfig {
            interval_min: Duration::from_millis(5),
            interval_max: Duration::from_millis(5),
            response_deadline: Duration::from_millis(50),
            max_fail_time: Duration::from_secs(3600),
            auto_reconnect: true,
            jitter: false,
        }
    }

    fn collector() -> (Arc<Mutex<Vec<WaEvent>>>, impl Fn(WaEvent) + Send + Sync) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        (events, move |event: WaEvent| sink.lock().unwrap().push(event))
    }

    #[tokio::test]
    async fn healthy_connection_emits_nothing() {
        let (events, emit) = collector();
        let pings = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&pings);
        let keepalive = KeepAlive::new(
            fast_config(),
            move || {
                counter.fetch_add(1, Ordering::SeqCst);
                async { PingOutcome::Answered }
            },
            emit,
            || panic!("no reconnect expected"),
        );
        let exit =
            tokio::time::timeout(Duration::from_secs(2), keepalive.run(tokio::time::sleep(Duration::from_millis(80))))
                .await
                .expect("loop did not stop");
        assert_eq!(exit, KeepAliveExit::Shutdown);
        assert!(pings.load(Ordering::SeqCst) >= 3, "the loop keeps pinging");
        assert!(events.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn timeouts_are_reported_and_restored_after_recovery() {
        let (events, emit) = collector();
        let attempts = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&attempts);
        let keepalive = KeepAlive::new(
            fast_config(),
            move || {
                // Two failures, then recovery.
                let attempt = counter.fetch_add(1, Ordering::SeqCst);
                async move { if attempt < 2 { PingOutcome::TimedOut } else { PingOutcome::Answered } }
            },
            emit,
            || {},
        );
        let exit =
            tokio::time::timeout(Duration::from_secs(2), keepalive.run(tokio::time::sleep(Duration::from_millis(120))))
                .await
                .expect("loop did not stop");
        assert_eq!(exit, KeepAliveExit::Shutdown);

        let recorded = events.lock().unwrap().clone();
        assert!(matches!(recorded.first(), Some(WaEvent::KeepAliveTimeout { error_count: 1 })));
        assert!(matches!(recorded.get(1), Some(WaEvent::KeepAliveTimeout { error_count: 2 })));
        assert!(
            recorded.iter().any(|event| matches!(event, WaEvent::KeepAliveRestored)),
            "recovery after failures emits KeepAliveRestored: {recorded:?}"
        );
    }

    #[tokio::test]
    async fn repeated_failures_past_the_deadline_force_a_reconnect() {
        let (_events, emit) = collector();
        let reconnects = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&reconnects);
        let config = KeepAliveConfig {
            // Nothing has succeeded for longer than this after the first failure.
            max_fail_time: Duration::ZERO,
            ..fast_config()
        };
        let keepalive = KeepAlive::new(
            config,
            || async { PingOutcome::TimedOut },
            emit,
            move || {
                counter.fetch_add(1, Ordering::SeqCst);
            },
        );
        let exit =
            tokio::time::timeout(Duration::from_secs(2), keepalive.run(tokio::time::sleep(Duration::from_millis(60))))
                .await
                .expect("loop did not stop");
        assert_eq!(exit, KeepAliveExit::Shutdown);
        assert!(reconnects.load(Ordering::SeqCst) > 0, "a failing keep-alive forces a reconnect");
    }

    #[tokio::test]
    async fn no_force_reconnect_when_auto_reconnect_is_disabled() {
        let (_events, emit) = collector();
        let reconnects = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&reconnects);
        let config = KeepAliveConfig { max_fail_time: Duration::ZERO, auto_reconnect: false, ..fast_config() };
        let keepalive = KeepAlive::new(
            config,
            || async { PingOutcome::TimedOut },
            emit,
            move || {
                counter.fetch_add(1, Ordering::SeqCst);
            },
        );
        let _ =
            tokio::time::timeout(Duration::from_secs(2), keepalive.run(tokio::time::sleep(Duration::from_millis(60))))
                .await
                .expect("loop did not stop");
        assert_eq!(reconnects.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_dead_connection_stops_the_loop() {
        let (_events, emit) = collector();
        let keepalive = KeepAlive::new(fast_config(), || async { PingOutcome::Disconnected }, emit, || {});
        let exit = tokio::time::timeout(Duration::from_secs(2), keepalive.run(std::future::pending()))
            .await
            .expect("loop did not stop");
        assert_eq!(exit, KeepAliveExit::Disconnected);
    }

    #[tokio::test]
    async fn response_deadline_is_enforced() {
        let (_events, emit) = collector();
        // A ping that never resolves must be reported as a timeout.
        let keepalive = KeepAlive::new(
            KeepAliveConfig { response_deadline: Duration::from_millis(10), ..fast_config() },
            std::future::pending::<PingOutcome>,
            emit,
            || {},
        );
        assert_eq!(keepalive.send_keep_alive().await, PingOutcome::TimedOut);
        assert_eq!(keepalive.error_count(), 0, "send_keep_alive does not count");
    }

    #[test]
    fn defaults_match_the_go_variables() {
        let config = KeepAliveConfig::default();
        assert_eq!(config.interval_min, Duration::from_secs(20));
        assert_eq!(config.interval_max, Duration::from_secs(30));
        assert_eq!(config.response_deadline, Duration::from_secs(10));
        assert_eq!(config.max_fail_time, Duration::from_secs(180));
        assert_eq!(KEEPALIVE_INTERVAL_MIN, Duration::from_secs(20));
        assert_eq!(KEEPALIVE_INTERVAL_MAX, Duration::from_secs(30));
        assert_eq!(KEEPALIVE_RESPONSE_DEADLINE, Duration::from_secs(10));
        assert_eq!(KEEPALIVE_MAX_FAIL_TIME, Duration::from_secs(180));
        // Without jitter the interval is the minimum.
        let fixed = KeepAliveConfig { jitter: false, ..config };
        assert_eq!(fixed.next_interval(), Duration::from_secs(20));
        // With jitter it stays inside [min, max).
        for _ in 0..50 {
            let interval = config.next_interval();
            assert!(interval >= Duration::from_secs(20) && interval < Duration::from_secs(30));
        }
    }
}
