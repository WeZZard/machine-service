//! Dialing out to the broker (`docs/arch/relay-agent.md`, RA-3).
//!
//! [`run`] is the entry point for the dial-out: it opens a link with the
//! dialer it is given, serves it until it closes, and dials again with
//! exponential backoff from 250 ms to 5 s with random jitter. The resumable
//! WebSocket session that makes a real dialer is built separately; this
//! module holds only the loop.

use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::agent::Agent;
use crate::config::Config;
use crate::link::{Link, LinkError};

/// The first delay before dialing again.
pub const BACKOFF_MIN: Duration = Duration::from_millis(250);
/// The longest delay before dialing again.
pub const BACKOFF_MAX: Duration = Duration::from_secs(5);

/// Exponential backoff with jitter.
#[derive(Clone, Debug)]
pub struct Backoff {
    ceiling: Duration,
    seed: u64,
}

impl Default for Backoff {
    fn default() -> Backoff {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos() as u64)
            .unwrap_or(1)
            ^ u64::from(std::process::id());
        Backoff::seeded(seed)
    }
}

impl Backoff {
    /// A backoff whose jitter follows `seed`.
    pub fn seeded(seed: u64) -> Backoff {
        Backoff {
            ceiling: BACKOFF_MIN,
            seed: seed | 1,
        }
    }

    /// The next delay: a random point in the upper half of the current
    /// ceiling, which then doubles up to [`BACKOFF_MAX`].
    pub fn next_delay(&mut self) -> Duration {
        // xorshift64*
        self.seed ^= self.seed >> 12;
        self.seed ^= self.seed << 25;
        self.seed ^= self.seed >> 27;
        let random = self.seed.wrapping_mul(0x2545_F491_4F6C_DD1D);
        let ceiling = self.ceiling.as_millis() as u64;
        let delay = ceiling / 2 + random % (ceiling / 2 + 1);
        self.ceiling = (self.ceiling * 2).min(BACKOFF_MAX);
        Duration::from_millis(delay)
    }

    /// Start again from [`BACKOFF_MIN`], after a link that worked.
    pub fn reset(&mut self) {
        self.ceiling = BACKOFF_MIN;
    }
}

/// Dial the broker and serve each link until the agent is stopped.
/// `report` sees each new reason a dial or a link failed once, until a
/// link is served.
pub fn run<L: Link + 'static>(
    agent: &Agent,
    config: &Config,
    mut dial: impl FnMut(&Config) -> Result<L, LinkError>,
    mut report: impl FnMut(&LinkError),
) {
    let mut backoff = Backoff::default();
    let mut last: Option<LinkError> = None;
    while !agent.is_stopped() {
        let failure = match dial(config) {
            Ok(link) => {
                backoff.reset();
                last = None;
                agent.serve(Arc::new(link)).err()
            }
            Err(error) => Some(error),
        };
        if let Some(error) = failure {
            if last.as_ref() != Some(&error) {
                report(&error);
                last = Some(error);
            }
        }
        thread::sleep(backoff.next_delay());
    }
}
