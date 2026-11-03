//! The TL-7 network rig: the chain every relay connection passes through in
//! the relay acceptance suite ([TL-7](../../../docs/arch/test-layers.md)).
//!
//! The chain is Toxiproxy, for faults at the TCP level, then mitmproxy with
//! the edge script in `edge/edge.py`, which ends TLS, routes by name, closes
//! an idle connection and refuses a WebSocket message over the limit. Both
//! run without root at pinned versions fetched into a cache outside the
//! repository ([`tools`]). The rig's own reset [`stage`] stands at both ends
//! of the chain and cuts a chosen open connection with a TCP reset toward the
//! client, the broker or both (DC-137). A [`Profile`] says what the chain does; a seeded
//! [`Schedule`] says when faults happen; a [`Control`] sets a fault at an
//! exact moment. The client reaches the broker only through the chain's one
//! port and only by a host name, which it resolves to that port itself
//! ([`ClientSide`]), so no system setting changes.
//!
//! [`cell`] runs one cell under one profile against a target; for now the
//! target is the [`standin`] WebSocket echo server.

pub mod admit;
pub mod cell;
pub mod client;
pub mod prng;
pub mod process;
pub mod profile;
pub mod rig;
pub mod schedule;
pub mod stage;
pub mod standin;
pub mod tls;
pub mod tools;
pub mod toxiproxy;

pub use profile::{EdgeRules, Profile, Stream, Toxic, ToxicKind};
pub use rig::{
    ClientSide, Control, EdgeAuthority, Note, Phase, Rig, RigConfig, DEFAULT_BROKER_NAME,
};
pub use schedule::{Event, Fault, Schedule};
pub use stage::{AddressChange, Connection, Select, Side};

/// The rig's errors.
#[derive(Debug, thiserror::Error)]
pub enum RigError {
    /// A profile document is malformed or inconsistent.
    #[error("profile: {0}")]
    Profile(String),
    /// A pinned tool could not be fetched, verified or installed.
    #[error("tool {tool}: {reason}")]
    Tool {
        /// The tool's name.
        tool: String,
        /// What went wrong.
        reason: String,
    },
    /// This platform has no pinned build of a tool.
    #[error("no pinned {tool} for {platform}")]
    Unsupported {
        /// The tool's name.
        tool: String,
        /// `<os>-<arch>`.
        platform: String,
    },
    /// Toxiproxy's API refused or failed a request.
    #[error("toxiproxy: {0}")]
    Toxiproxy(String),
    /// A child process did not start or did not become ready.
    #[error("{name}: {reason}")]
    Process {
        /// The child's name.
        name: String,
        /// What went wrong.
        reason: String,
    },
    /// Certificates or TLS configuration could not be made.
    #[error("tls: {0}")]
    Tls(String),
    /// A file or socket operation failed.
    #[error("{context}: {source}")]
    Io {
        /// What the rig was doing.
        context: String,
        /// The error.
        #[source]
        source: std::io::Error,
    },
}

impl RigError {
    pub(crate) fn io(context: impl Into<String>) -> impl FnOnce(std::io::Error) -> RigError {
        let context = context.into();
        move |source| RigError::Io { context, source }
    }
}
