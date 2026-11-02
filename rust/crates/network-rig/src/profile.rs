//! Network profiles N0 to N3 of TL-7, as declarative configuration.
//!
//! A profile is a JSON document in `profiles/`. It names the edge's rules
//! (the idle close and the message limit), the Toxiproxy toxics that stay on
//! for the whole run, and the chaos menu from which the seeded schedule is
//! drawn. The four built-in profiles are compiled into the crate; a test may
//! parse its own with [`Profile::parse`].

use serde::{Deserialize, Serialize};

use crate::RigError;

/// One network profile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// `N0` to `N3`, or a test's own name.
    pub id: String,
    /// The profile's name in TL-7.
    pub name: String,
    /// What the edge script enforces.
    pub edge: EdgeRules,
    /// Toxics that stay on for the whole run, in the order Toxiproxy applies them.
    #[serde(default)]
    pub toxics: Vec<Toxic>,
    /// The menu of the seeded fault schedule, or none for a profile without chaos.
    #[serde(default)]
    pub chaos: Option<Chaos>,
}

/// The edge script's rules. Zero turns a rule off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeRules {
    /// Close a connection that carries no data for this many seconds (TC-3).
    pub idle_close_s: u64,
    /// Close a WebSocket that sends a larger message (TC-8).
    pub max_message_bytes: u64,
}

/// The direction of a toxic, in Toxiproxy's words: `upstream` is client to
/// broker, `downstream` is broker to client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stream {
    /// Client to broker.
    Upstream,
    /// Broker to client.
    Downstream,
}

impl Stream {
    /// Toxiproxy's name for the direction.
    pub fn as_str(self) -> &'static str {
        match self {
            Stream::Upstream => "upstream",
            Stream::Downstream => "downstream",
        }
    }
}

/// One Toxiproxy toxic, serialized exactly as Toxiproxy's API takes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Toxic {
    /// The toxic's name, unique within the proxy.
    pub name: String,
    /// The direction it acts on.
    pub stream: Stream,
    /// The share of connections it acts on, from 0 to 1.
    #[serde(default = "always")]
    pub toxicity: f64,
    /// What it does.
    #[serde(flatten)]
    pub kind: ToxicKind,
}

fn always() -> f64 {
    1.0
}

/// The toxics of Toxiproxy 2.x that the rig uses, with their attributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "attributes", rename_all = "snake_case")]
pub enum ToxicKind {
    /// Delay each chunk by `latency` ms, plus or minus up to `jitter` ms.
    Latency {
        /// Milliseconds.
        latency: u64,
        /// Milliseconds.
        jitter: u64,
    },
    /// Limit the rate to `rate` KB per second.
    Bandwidth {
        /// KB per second.
        rate: u64,
    },
    /// Cut data into pieces of about `average_size` bytes.
    Slicer {
        /// Bytes.
        average_size: u64,
        /// Bytes.
        size_variation: u64,
        /// Microseconds between pieces.
        delay: u64,
    },
    /// Stop all data; close the connection after `timeout` ms, or never when 0.
    Timeout {
        /// Milliseconds.
        timeout: u64,
    },
    /// Reset the connection with a TCP RST after `timeout` ms.
    ResetPeer {
        /// Milliseconds.
        timeout: u64,
    },
    /// Close the connection after `bytes` bytes.
    LimitData {
        /// Bytes.
        bytes: u64,
    },
}

/// The menu of a seeded fault schedule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Chaos {
    /// When the first fault starts, after the schedule's start.
    pub first_after_ms: u64,
    /// The quiet time between the end of one fault and the start of the next.
    pub gap_ms: Span,
    /// The faults to draw from.
    pub faults: Vec<FaultChoice>,
}

/// An inclusive range `[lo, hi]` from which the schedule draws uniformly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span(pub u64, pub u64);

/// One entry of the chaos menu.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaultChoice {
    /// The relative chance of this entry.
    pub weight: u64,
    /// The fault and its ranges.
    #[serde(flatten)]
    pub fault: FaultMenu,
}

/// A fault with the ranges its parameters are drawn from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "fault", rename_all = "snake_case", deny_unknown_fields)]
pub enum FaultMenu {
    /// Reset every connection that sends data during the window (TC-2).
    Reset {
        /// How long the reset stays armed.
        window_ms: Span,
    },
    /// Hold all data without closing the connection (TC-2).
    Blackhole {
        /// How long the data is held.
        duration_ms: Span,
    },
    /// Close each connection after a number of bytes during the window (TC-2).
    LimitData {
        /// The bytes a connection may carry.
        bytes: Span,
        /// How long the limit stays armed.
        window_ms: Span,
    },
    /// Close every connection so the client dials again from a new source
    /// address or port (TC-4).
    AddressChange,
}

const BUILTIN: [(&str, &str); 4] = [
    ("N0", include_str!("../profiles/n0.json")),
    ("N1", include_str!("../profiles/n1.json")),
    ("N2", include_str!("../profiles/n2.json")),
    ("N3", include_str!("../profiles/n3.json")),
];

impl Profile {
    /// Parse and check a profile document.
    pub fn parse(text: &str) -> Result<Profile, RigError> {
        let profile: Profile =
            serde_json::from_str(text).map_err(|error| RigError::Profile(error.to_string()))?;
        profile.check()?;
        Ok(profile)
    }

    /// One of the built-in profiles `N0` to `N3`.
    pub fn builtin(id: &str) -> Result<Profile, RigError> {
        let (_, text) = BUILTIN
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(id))
            .ok_or_else(|| RigError::Profile(format!("no built-in profile {id}")))?;
        Profile::parse(text)
    }

    /// The built-in profiles in order.
    pub fn all() -> Result<Vec<Profile>, RigError> {
        BUILTIN.iter().map(|(id, _)| Profile::builtin(id)).collect()
    }

    fn check(&self) -> Result<(), RigError> {
        let bad = |why: String| Err(RigError::Profile(format!("{}: {why}", self.id)));
        let mut names: Vec<&str> = self
            .toxics
            .iter()
            .map(|toxic| toxic.name.as_str())
            .collect();
        names.sort_unstable();
        if names.windows(2).any(|pair| pair[0] == pair[1]) {
            return bad("two toxics share a name".into());
        }
        if let Some(toxic) = self
            .toxics
            .iter()
            .find(|t| !(0.0..=1.0).contains(&t.toxicity))
        {
            return bad(format!("toxic {} has a toxicity outside 0..1", toxic.name));
        }
        if let Some(chaos) = &self.chaos {
            if chaos.faults.iter().all(|choice| choice.weight == 0) {
                return bad("the chaos menu has no fault with a weight".into());
            }
            let spans = chaos.faults.iter().flat_map(|choice| match &choice.fault {
                FaultMenu::Reset { window_ms } => vec![*window_ms],
                FaultMenu::Blackhole { duration_ms } => vec![*duration_ms],
                FaultMenu::LimitData { bytes, window_ms } => vec![*bytes, *window_ms],
                FaultMenu::AddressChange => vec![],
            });
            if let Some(span) = spans.chain([chaos.gap_ms]).find(|span| span.0 > span.1) {
                return bad(format!("the range [{}, {}] is reversed", span.0, span.1));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_four_builtin_profiles_parse() {
        let ids: Vec<String> = Profile::all().unwrap().into_iter().map(|p| p.id).collect();
        assert_eq!(ids, ["N0", "N1", "N2", "N3"]);
        assert_eq!(Profile::builtin("n2").unwrap().id, "N2");
        assert!(Profile::builtin("N4").is_err());
    }

    #[test]
    fn n0_injects_nothing() {
        let n0 = Profile::builtin("N0").unwrap();
        assert_eq!(
            n0.edge,
            EdgeRules {
                idle_close_s: 0,
                max_message_bytes: 0
            }
        );
        assert!(n0.toxics.is_empty());
        assert!(n0.chaos.is_none());
    }

    #[test]
    fn n1_to_n3_enforce_the_edge_of_the_contract() {
        for id in ["N1", "N2", "N3"] {
            let profile = Profile::builtin(id).unwrap();
            assert_eq!(profile.edge.idle_close_s, 100, "{id}: TC-3");
            assert_eq!(profile.edge.max_message_bytes, 1 << 20, "{id}: TC-8");
        }
        let n1 = Profile::builtin("N1").unwrap();
        assert!(n1.toxics.is_empty() && n1.chaos.is_none());
    }

    #[test]
    fn n2_adds_a_round_trip_of_300_ms_to_1_s_and_limits_the_link() {
        let n2 = Profile::builtin("N2").unwrap();
        let latency = |stream: Stream| {
            n2.toxics
                .iter()
                .find_map(|t| match t.kind {
                    ToxicKind::Latency { latency, jitter } if t.stream == stream => {
                        Some((latency - jitter, latency + jitter))
                    }
                    _ => None,
                })
                .unwrap()
        };
        let (up, down) = (latency(Stream::Upstream), latency(Stream::Downstream));
        assert_eq!((up.0 + down.0, up.1 + down.1), (300, 1000), "TC-7");
        for stream in [Stream::Upstream, Stream::Downstream] {
            let kinds: Vec<_> = n2.toxics.iter().filter(|t| t.stream == stream).collect();
            assert!(kinds
                .iter()
                .any(|t| matches!(t.kind, ToxicKind::Bandwidth { .. })));
            assert!(kinds
                .iter()
                .any(|t| matches!(t.kind, ToxicKind::Slicer { .. })));
        }
        assert!(n2.chaos.is_none());
    }

    #[test]
    fn n3_is_n2_with_chaos_of_every_kind() {
        let n2 = Profile::builtin("N2").unwrap();
        let n3 = Profile::builtin("N3").unwrap();
        assert_eq!(n3.toxics, n2.toxics);
        assert_eq!(n3.edge, n2.edge);
        let chaos = n3.chaos.unwrap();
        let has =
            |f: fn(&FaultMenu) -> bool| chaos.faults.iter().any(|c| c.weight > 0 && f(&c.fault));
        assert!(has(|f| matches!(f, FaultMenu::Reset { .. })));
        assert!(has(|f| matches!(f, FaultMenu::Blackhole { .. })));
        assert!(has(|f| matches!(f, FaultMenu::LimitData { .. })));
        assert!(has(|f| matches!(f, FaultMenu::AddressChange)));
    }

    #[test]
    fn a_toxic_serializes_as_toxiproxy_takes_it() {
        let toxic = Toxic {
            name: "lag".into(),
            stream: Stream::Downstream,
            toxicity: 1.0,
            kind: ToxicKind::Latency {
                latency: 325,
                jitter: 175,
            },
        };
        assert_eq!(
            serde_json::to_value(&toxic).unwrap(),
            serde_json::json!({
                "name": "lag",
                "stream": "downstream",
                "toxicity": 1.0,
                "type": "latency",
                "attributes": {"latency": 325, "jitter": 175}
            })
        );
    }

    #[test]
    fn a_bad_profile_is_refused() {
        let base = r#"{"id":"T","name":"t","edge":{"idle_close_s":0,"max_message_bytes":0}"#;
        assert!(Profile::parse(&format!("{base}}}")).is_ok());
        assert!(Profile::parse(&format!("{base},\"extra\":1}}")).is_err());
        let toxic = r#"{"name":"a","stream":"upstream","type":"latency","attributes":{"latency":1,"jitter":0}}"#;
        assert!(Profile::parse(&format!("{base},\"toxics\":[{toxic},{toxic}]}}")).is_err());
        let unknown = r#"{"name":"a","stream":"upstream","type":"warp","attributes":{}}"#;
        assert!(Profile::parse(&format!("{base},\"toxics\":[{unknown}]}}")).is_err());
        let reversed = r#""chaos":{"first_after_ms":0,"gap_ms":[5,1],"faults":[{"weight":1,"fault":"address_change"}]}"#;
        assert!(Profile::parse(&format!("{base},{reversed}}}")).is_err());
        let extra = r#""chaos":{"first_after_ms":0,"gap_ms":[1,5],"faults":[{"weight":1,"fault":"reset","window_ms":[1,2],"oops":1}]}"#;
        assert!(Profile::parse(&format!("{base},{extra}}}")).is_err());
        let weightless = r#""chaos":{"first_after_ms":0,"gap_ms":[1,5],"faults":[{"weight":0,"fault":"address_change"}]}"#;
        assert!(Profile::parse(&format!("{base},{weightless}}}")).is_err());
    }
}
