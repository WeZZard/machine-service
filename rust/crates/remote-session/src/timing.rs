//! Heartbeat, dead-connection and reconnect timing (RS-4, TC-3, TC-7).

use std::time::Duration;

use crate::Error;

/// The longest heartbeat interval the transport contract allows (TC-3): a
/// link may close a connection that is idle for 100 seconds.
pub const MAX_HEARTBEAT: Duration = Duration::from_secs(30);

/// Connection timing for one side of a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timing {
    /// Send a heartbeat after this long without sending anything (default 5 s).
    pub heartbeat: Duration,
    /// Treat the connection as dead after this long without receiving
    /// anything (default 15 s).
    pub dead_after: Duration,
    /// Bound on the TCP connect, the TLS and WebSocket handshakes and the
    /// hello/welcome exchange, sized for a 1 s round trip (default 10 s).
    pub handshake_timeout: Duration,
    /// How long one read waits before the loop looks for outgoing work
    /// (default 10 ms). It bounds the added latency of a send.
    pub poll_interval: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            heartbeat: Duration::from_secs(5),
            dead_after: Duration::from_secs(15),
            handshake_timeout: Duration::from_secs(10),
            poll_interval: Duration::from_millis(10),
        }
    }
}

impl Timing {
    pub fn validate(&self) -> Result<(), Error> {
        let fail = |m: &str| Err(Error::Config(m.to_string()));
        if self.heartbeat.is_zero() || self.heartbeat > MAX_HEARTBEAT {
            return fail("heartbeat must be above zero and at most 30 s (TC-3)");
        }
        if self.dead_after <= self.heartbeat {
            return fail("dead_after must be longer than heartbeat");
        }
        if self.poll_interval.is_zero() || self.poll_interval >= self.heartbeat {
            return fail("poll_interval must be above zero and below heartbeat");
        }
        if self.handshake_timeout.is_zero() {
            return fail("handshake_timeout must be above zero");
        }
        Ok(())
    }
}

/// Exponential reconnect backoff with jitter (RS-4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backoff {
    /// The base of the first delay (default 250 ms).
    pub initial: Duration,
    /// The cap on the base delay (default 5 s).
    pub max: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            initial: Duration::from_millis(250),
            max: Duration::from_secs(5),
        }
    }
}

impl Backoff {
    /// The delay before reconnect attempt `attempt` (from 0), given a uniform
    /// random `unit` in `[0, 1)`: the base doubles from `initial` up to `max`,
    /// and the delay is uniform in `[base / 2, base)`, so it never exceeds
    /// `max` and two clients cut at once spread out.
    pub fn delay(&self, attempt: u32, unit: f64) -> Duration {
        let base = self
            .initial
            .saturating_mul(1u32 << attempt.min(16))
            .min(self.max);
        base.mul_f64(0.5 + 0.5 * unit.clamp(0.0, 1.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_from_250_ms_to_5_s_with_jitter() {
        let b = Backoff::default();
        let ms = |d: Duration| d.as_millis();
        assert_eq!(ms(b.delay(0, 0.0)), 125);
        assert_eq!(ms(b.delay(0, 1.0)), 250);
        assert_eq!(ms(b.delay(1, 1.0)), 500);
        assert_eq!(ms(b.delay(5, 1.0)), 5000);
        assert_eq!(ms(b.delay(40, 1.0)), 5000);
        assert_eq!(ms(b.delay(40, 0.0)), 2500);
    }

    #[test]
    fn defaults_meet_the_transport_contract() {
        let t = Timing::default();
        t.validate().unwrap();
        assert!(t.heartbeat <= MAX_HEARTBEAT);
        assert_eq!(t.dead_after, Duration::from_secs(15));
        let bad = Timing {
            heartbeat: Duration::from_secs(31),
            dead_after: Duration::from_secs(60),
            ..t
        };
        assert!(bad.validate().is_err());
    }
}
