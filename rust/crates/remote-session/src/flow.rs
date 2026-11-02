//! Flow control (RS-4): how many bytes one connection may hold in flight,
//! adapted from the throughput the peer's acknowledgements show, and the
//! round-trip time they show. Sans-IO: the connection loop passes the time.
//!
//! A connection is judged alive by progress: its acknowledgement must advance
//! within the dead-connection bound while bodies are in flight. Everything a
//! side writes waits in the path's queues (its own socket, every proxy and
//! the bottleneck) before the peer can acknowledge it, and an acknowledgement
//! or heartbeat from the peer may queue behind the same bytes in a proxy that
//! carries both directions in one loop. So the bytes in flight are bounded by
//! what the link carries in its own round trip plus a third of the bound: the
//! queue this side builds drains within a third of the bound, so on a healthy
//! link, however slow, the oldest byte in flight is acknowledged well within
//! the bound, and only a link that stops making progress is declared stalled.
//! The round-trip term keeps the window growing while the link has room:
//! without it the window would only sustain the rate it already has.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::wire::MAX_MESSAGE;

/// The window before the first measurement: enough to fill a link of 4 MB/s
/// with a 1 s round trip, small enough to drain within 1 s on it.
pub const INITIAL_WINDOW: usize = 4 << 20;

/// The smallest window: one largest message, so a body always goes out.
pub const MIN_WINDOW: usize = MAX_MESSAGE;

/// The shortest span a throughput measurement covers; it also covers at
/// least two round trips, so it never ends before what was sent in it could
/// be acknowledged.
const MIN_SPAN: Duration = Duration::from_secs(1);

/// How often a point of the acknowledged-bytes curve is kept.
const POINT: Duration = Duration::from_millis(250);

/// The in-flight window of one connection.
///
/// The throughput is the average over the last two drain times (10 s by
/// default) of busy time, not the best of short samples: a proxy that holds
/// the peer's acknowledgements and lets them go in a burst would make a
/// short sample read many times the link's rate, and a window sized from it
/// would deepen the very queue that held them.
#[derive(Debug, Clone)]
pub(crate) struct FlowWindow {
    /// How long the queue this side builds may take to drain at the
    /// measured throughput.
    drain: Duration,
    /// The span the throughput is averaged over.
    span: Duration,
    /// The largest window: the replay buffer's bound.
    ceiling: usize,
    window: usize,
    /// Points of busy time: when, and the bytes acknowledged by then.
    points: VecDeque<(Instant, u64)>,
}

impl FlowWindow {
    /// A window whose queue drains within a third of `dead_after`, at most
    /// `ceiling`.
    pub fn new(dead_after: Duration, ceiling: usize) -> Self {
        let ceiling = ceiling.max(MIN_WINDOW);
        let drain = dead_after / 3;
        Self {
            drain,
            span: drain * 2,
            ceiling,
            window: INITIAL_WINDOW.clamp(MIN_WINDOW, ceiling),
            points: VecDeque::new(),
        }
    }

    /// The bytes this connection may hold unacknowledged now.
    pub fn window(&self) -> usize {
        self.window
    }

    /// Observe `acked_bytes`, the bytes the peer acknowledged so far,
    /// whether bodies wait for an acknowledgement (`busy`), and the smallest
    /// round trip seen. Only busy time is measured, so an idle pause does not
    /// count as a slow link. A span in which nothing was acknowledged gives
    /// no rate at all: the stall rule judges a link that makes no progress.
    pub fn observe(&mut self, now: Instant, acked_bytes: u64, busy: bool, rtt: Option<Duration>) {
        if !busy {
            self.points.clear();
            return;
        }
        if self
            .points
            .back()
            .is_some_and(|&(t, _)| now.saturating_duration_since(t) < POINT)
        {
            return;
        }
        self.points.push_back((now, acked_bytes));
        while self.points.len() > 2 && now.saturating_duration_since(self.points[1].0) >= self.span
        {
            self.points.pop_front();
        }
        let (t0, b0) = self.points[0];
        let covered = now.saturating_duration_since(t0);
        if covered < MIN_SPAN.max(rtt.unwrap_or_default() * 2) {
            return;
        }
        let rate = acked_bytes.saturating_sub(b0) as f64 / covered.as_secs_f64();
        if rate == 0.0 {
            return;
        }
        let hold = self.drain + rtt.unwrap_or_default();
        let window = (rate * hold.as_secs_f64()) as usize;
        self.window = window.clamp(MIN_WINDOW, self.ceiling);
    }
}

/// Round-trip samples: a body's write to the acknowledgement that covers it.
/// The peer acknowledges on receipt, so on an idle link a sample is the
/// path's round trip plus at most one poll interval of the peer's loop.
#[derive(Debug, Default)]
pub(crate) struct RttMeter {
    /// Bodies written on this connection: sequence number and when.
    written: VecDeque<(u64, Instant)>,
}

impl RttMeter {
    pub fn wrote(&mut self, seq: u64, at: Instant) {
        self.written.push_back((seq, at));
    }

    /// Forget writes from `seq` on, because they were refused and go again.
    pub fn rewind(&mut self, seq: u64) {
        while self.written.back().is_some_and(|(s, _)| *s >= seq) {
            self.written.pop_back();
        }
    }

    /// The peer acknowledged everything up to `acked`; the sample is the
    /// round trip of the body with that sequence number, when it was written
    /// on this connection.
    pub fn acked(&mut self, acked: u64, now: Instant) -> Option<Duration> {
        let mut sample = None;
        while let Some(&(seq, at)) = self.written.front() {
            if seq > acked {
                break;
            }
            self.written.pop_front();
            if seq == acked {
                sample = Some(now.saturating_duration_since(at));
            }
        }
        sample
    }
}

/// A session's round-trip statistics, over all its connections.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Rtt {
    /// How many samples.
    pub samples: u64,
    /// The latest sample.
    pub last: Option<Duration>,
    /// The smallest sample: the path's own round trip without queueing.
    pub min: Option<Duration>,
    /// The smoothed round trip, as TCP smooths it (RFC 6298, gain 1/8).
    pub smoothed: Option<Duration>,
}

impl Rtt {
    pub(crate) fn sample(&mut self, rtt: Duration) {
        self.samples += 1;
        self.last = Some(rtt);
        self.min = Some(self.min.map_or(rtt, |m| m.min(rtt)));
        self.smoothed = Some(match self.smoothed {
            None => rtt,
            Some(s) => (s * 7 + rtt) / 8,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: f64 = 1e6;

    /// Drive a window on a link of `rate` bytes per second for `secs`, with
    /// the sender always backlogged, and return the window.
    fn settle(w: &mut FlowWindow, t0: Instant, rate: f64, secs: u64) -> Instant {
        let mut acked = 0u64;
        let mut now = t0;
        for _ in 0..secs * 4 {
            now += Duration::from_millis(250);
            acked += (rate / 4.0) as u64;
            w.observe(now, acked, true, None);
        }
        now
    }

    #[test]
    fn the_window_drains_within_a_third_of_the_bound_at_the_measured_rate() {
        let mut w = FlowWindow::new(Duration::from_secs(15), 64 << 20);
        assert_eq!(w.window(), INITIAL_WINDOW);
        settle(&mut w, Instant::now(), 4.0 * MB, 10);
        let drain = w.window() as f64 / (4.0 * MB);
        assert!((4.5..=5.5).contains(&drain), "drains in {drain} s");
        assert!(
            w.window() as f64 > 4.0 * MB * 1.7,
            "covers a 1.7 s round trip"
        );
    }

    #[test]
    fn the_window_is_bounded_by_the_replay_buffer_and_one_message() {
        let mut fast = FlowWindow::new(Duration::from_secs(15), 64 << 20);
        settle(&mut fast, Instant::now(), 1e9, 5);
        assert_eq!(fast.window(), 64 << 20);
        let mut slow = FlowWindow::new(Duration::from_secs(15), 64 << 20);
        settle(&mut slow, Instant::now(), 10e3, 5);
        assert_eq!(slow.window(), MIN_WINDOW);
    }

    #[test]
    fn idle_time_is_not_a_slow_link() {
        let mut w = FlowWindow::new(Duration::from_secs(15), 64 << 20);
        let t = settle(&mut w, Instant::now(), 4.0 * MB, 12);
        let before = w.window();
        w.observe(t + Duration::from_secs(3600), 0, false, None);
        assert_eq!(w.window(), before);
        // Nothing acknowledged for a second is no rate either.
        let t = t + Duration::from_secs(3601);
        w.observe(t, 100, true, None);
        w.observe(t + MIN_SPAN, 100, true, None);
        assert_eq!(w.window(), before);
    }

    #[test]
    fn bunched_acknowledgements_do_not_inflate_the_window() {
        // A proxy holds the acknowledgements of a 4 MB/s link for 8 s and
        // then lets them all go at once, again and again.
        let mut w = FlowWindow::new(Duration::from_secs(15), 64 << 20);
        let mut now = Instant::now();
        let mut acked = 0u64;
        let mut largest = 0;
        for step in 1..=160u32 {
            now += Duration::from_millis(250);
            if step % 32 == 0 {
                acked += (8.0 * 4.0 * MB) as u64;
            }
            w.observe(now, acked, true, None);
            largest = largest.max(w.window());
        }
        let drain = largest as f64 / (4.0 * MB);
        assert!(drain <= 8.5, "the window grew to {drain} s of the link");
    }

    #[test]
    fn a_measurement_spans_two_round_trips() {
        let mut w = FlowWindow::new(Duration::from_secs(15), 64 << 20);
        let t = Instant::now();
        let rtt = Some(Duration::from_millis(1700));
        w.observe(t, 0, true, rtt);
        // At 1 s only the first round trip's bytes are back: too early.
        w.observe(t + MIN_SPAN, 1_000_000, true, rtt);
        assert_eq!(w.window(), INITIAL_WINDOW);
        w.observe(t + Duration::from_millis(3400), 13_600_000, true, rtt);
        // 4 MB/s for the 1.7 s round trip and 5 s of queue.
        assert!(
            (26_799_000..=26_801_000).contains(&w.window()),
            "{}",
            w.window()
        );
    }

    #[test]
    fn the_window_follows_a_link_that_slows_down() {
        let mut w = FlowWindow::new(Duration::from_secs(15), 64 << 20);
        let mut now = settle(&mut w, Instant::now(), 8.0 * MB, 12);
        let mut acked = 0;
        w.observe(now, acked, false, None);
        for _ in 0..12 * 4 {
            now += Duration::from_millis(250);
            acked += 250_000;
            w.observe(now, acked, true, None);
        }
        let drain = w.window() as f64 / (1.0 * MB);
        assert!(
            (4.5..=5.5).contains(&drain),
            "drains in {drain} s at the new rate"
        );
    }

    #[test]
    fn a_round_trip_is_the_write_to_its_acknowledgement() {
        let mut m = RttMeter::default();
        let t = Instant::now();
        m.wrote(1, t);
        m.wrote(2, t + Duration::from_millis(10));
        m.wrote(3, t + Duration::from_millis(20));
        m.rewind(3);
        assert_eq!(
            m.acked(2, t + Duration::from_millis(510)),
            Some(Duration::from_millis(500))
        );
        assert_eq!(m.acked(2, t + Duration::from_secs(1)), None);
        let mut rtt = Rtt::default();
        rtt.sample(Duration::from_millis(800));
        rtt.sample(Duration::from_millis(400));
        assert_eq!(rtt.min, Some(Duration::from_millis(400)));
        assert_eq!(rtt.smoothed, Some(Duration::from_millis(750)));
        assert_eq!(rtt.samples, 2);
    }
}
