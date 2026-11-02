//! The seeded fault schedule.
//!
//! A schedule is a pure function of a chaos menu, a seed and a horizon: the
//! same three always give the same faults at the same offsets, so a failing
//! run is replayed by running again with its seed. Faults never overlap: each
//! one ends, and a quiet gap passes, before the next starts.

use serde::{Deserialize, Serialize};

use crate::prng::SplitMix64;
use crate::profile::{Chaos, FaultMenu, Span, Stream};

/// A fault the rig applies at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "fault", rename_all = "snake_case")]
pub enum Fault {
    /// Reset every connection that carries data in `stream` during the window.
    Reset {
        /// The direction whose data triggers the reset.
        stream: Stream,
        /// How long the reset stays armed.
        window_ms: u64,
    },
    /// Hold all data in `stream` without closing the connection.
    Blackhole {
        /// The direction that is held.
        stream: Stream,
        /// How long the data is held.
        duration_ms: u64,
    },
    /// Close each connection after `bytes` bytes in `stream` during the window.
    LimitData {
        /// The direction that is counted.
        stream: Stream,
        /// The bytes a connection may carry.
        bytes: u64,
        /// How long the limit stays armed.
        window_ms: u64,
    },
    /// Close every connection, so the client dials again from a new source
    /// address or port.
    AddressChange,
}

impl Fault {
    /// How long the fault lasts before the rig reverts it.
    pub fn duration_ms(&self) -> u64 {
        match *self {
            Fault::Reset { window_ms, .. } | Fault::LimitData { window_ms, .. } => window_ms,
            Fault::Blackhole { duration_ms, .. } => duration_ms,
            Fault::AddressChange => 0,
        }
    }

    /// The fault's kind, as named in a profile.
    pub fn kind(&self) -> &'static str {
        match self {
            Fault::Reset { .. } => "reset",
            Fault::Blackhole { .. } => "blackhole",
            Fault::LimitData { .. } => "limit_data",
            Fault::AddressChange => "address_change",
        }
    }
}

/// One fault at an offset from the schedule's start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// The event's position in the schedule.
    pub index: usize,
    /// Milliseconds after the schedule's start.
    pub at_ms: u64,
    /// What happens.
    pub fault: Fault,
}

/// The faults of one run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    /// The seed the schedule was drawn from.
    pub seed: u64,
    /// The span the schedule covers.
    pub horizon_ms: u64,
    /// The faults in order.
    pub events: Vec<Event>,
}

impl Schedule {
    /// A schedule with no faults.
    pub fn empty(seed: u64, horizon_ms: u64) -> Schedule {
        Schedule {
            seed,
            horizon_ms,
            events: Vec::new(),
        }
    }

    /// Draw the schedule for `chaos` from `seed`, covering `horizon_ms`.
    /// Every fault ends within the horizon.
    pub fn generate(chaos: &Chaos, seed: u64, horizon_ms: u64) -> Schedule {
        let mut rng = SplitMix64::new(seed);
        let weights: Vec<u64> = chaos.faults.iter().map(|choice| choice.weight).collect();
        let mut events = Vec::new();
        let mut at_ms = chaos.first_after_ms;
        while let Some(pick) = rng.weighted(&weights) {
            let fault = draw(&mut rng, &chaos.faults[pick].fault);
            let end = at_ms.saturating_add(fault.duration_ms());
            if end > horizon_ms {
                break;
            }
            events.push(Event {
                index: events.len(),
                at_ms,
                fault,
            });
            at_ms = end.saturating_add(span(&mut rng, chaos.gap_ms));
        }
        Schedule {
            seed,
            horizon_ms,
            events,
        }
    }
}

fn span(rng: &mut SplitMix64, span: Span) -> u64 {
    rng.range(span.0, span.1)
}

fn stream(rng: &mut SplitMix64) -> Stream {
    if rng.range(0, 1) == 0 {
        Stream::Upstream
    } else {
        Stream::Downstream
    }
}

fn draw(rng: &mut SplitMix64, menu: &FaultMenu) -> Fault {
    match *menu {
        FaultMenu::Reset { window_ms } => Fault::Reset {
            stream: stream(rng),
            window_ms: span(rng, window_ms),
        },
        FaultMenu::Blackhole { duration_ms } => Fault::Blackhole {
            stream: stream(rng),
            duration_ms: span(rng, duration_ms),
        },
        FaultMenu::LimitData { bytes, window_ms } => Fault::LimitData {
            stream: stream(rng),
            bytes: span(rng, bytes),
            window_ms: span(rng, window_ms),
        },
        FaultMenu::AddressChange => Fault::AddressChange,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::Profile;

    const HOUR_MS: u64 = 3_600_000;

    fn n3() -> Chaos {
        Profile::builtin("N3").unwrap().chaos.unwrap()
    }

    #[test]
    fn the_same_seed_gives_the_same_schedule() {
        let chaos = n3();
        for seed in [0, 1, 42, u64::MAX] {
            let first = Schedule::generate(&chaos, seed, HOUR_MS);
            let again = Schedule::generate(&chaos, seed, HOUR_MS);
            assert_eq!(first, again);
            assert!(!first.events.is_empty());
        }
    }

    #[test]
    fn different_seeds_give_different_schedules() {
        let chaos = n3();
        let a = Schedule::generate(&chaos, 1, HOUR_MS);
        let b = Schedule::generate(&chaos, 2, HOUR_MS);
        assert_ne!(a.events, b.events);
    }

    #[test]
    fn a_shorter_horizon_is_a_prefix_of_a_longer_one() {
        let chaos = n3();
        let short = Schedule::generate(&chaos, 9, HOUR_MS / 4);
        let long = Schedule::generate(&chaos, 9, HOUR_MS);
        assert_eq!(short.events[..], long.events[..short.events.len()]);
    }

    #[test]
    fn faults_are_in_order_apart_and_inside_the_horizon() {
        let chaos = n3();
        for seed in 0..200 {
            let schedule = Schedule::generate(&chaos, seed, HOUR_MS);
            assert!(schedule.events[0].at_ms >= chaos.first_after_ms);
            for (index, event) in schedule.events.iter().enumerate() {
                assert_eq!(event.index, index);
                assert!(event.at_ms + event.fault.duration_ms() <= HOUR_MS);
            }
            for pair in schedule.events.windows(2) {
                let end = pair[0].at_ms + pair[0].fault.duration_ms();
                assert!(pair[1].at_ms >= end + chaos.gap_ms.0, "seed {seed}");
                assert!(pair[1].at_ms <= end + chaos.gap_ms.1, "seed {seed}");
            }
        }
    }

    #[test]
    fn drawn_parameters_stay_inside_the_menu() {
        let chaos = n3();
        for seed in 0..200 {
            for event in Schedule::generate(&chaos, seed, HOUR_MS).events {
                match event.fault {
                    Fault::Reset { window_ms, .. } => assert!((1000..=6000).contains(&window_ms)),
                    Fault::Blackhole { duration_ms, .. } => {
                        assert!((3000..=25000).contains(&duration_ms))
                    }
                    Fault::LimitData {
                        bytes, window_ms, ..
                    } => {
                        assert!((4096..=1_048_576).contains(&bytes));
                        assert!((1000..=6000).contains(&window_ms));
                    }
                    Fault::AddressChange => {}
                }
            }
        }
    }

    /// The schedule for a seed is part of the replay contract: a seed
    /// recorded in an old report must give the same faults today.
    #[test]
    fn seed_42_gives_the_recorded_schedule() {
        let schedule = Schedule::generate(&n3(), 42, 300_000);
        let got = serde_json::to_value(&schedule.events).unwrap();
        let want: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/n3-seed-42.json")).unwrap();
        assert_eq!(got, want);
    }

    #[test]
    fn an_empty_schedule_has_no_faults() {
        assert!(Schedule::empty(3, HOUR_MS).events.is_empty());
    }
}
