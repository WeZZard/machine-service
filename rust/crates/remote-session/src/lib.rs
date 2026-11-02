//! The remote session: a resumable, payload-agnostic session over WebSocket,
//! client and server.
//!
//! The design is `docs/arch/remote-session.md` (RS-1 to RS-6) under the
//! transport contract `docs/arch/transport-contract.md` (TC-1 to TC-8). A
//! [`Session`] is an ordered, reliable, message-oriented duplex link with
//! opaque bodies: every body sent is delivered once and in order, across any
//! number of broken connections and changed addresses, or the session ends
//! with [`SessionEnd::Expired`] and the payload layer asks about what it had
//! in flight. Operations and execution IDs (RS-3) live above this crate.
//!
//! - [`connect`] opens a session from a [`ClientConfig`] and keeps it alive.
//! - [`SessionServer`] serves sessions, on a listener of its own or on
//!   connections that machine-service's session listener hands it.
//!
//! The wire: the client's first message is the JSON text message
//! `hello {version, session, received, credential, attributes}`; the server
//! answers `welcome {session, resumed, received, heartbeat_ms, grace_ms}`,
//! `expired {session, reason}` or `error {code, message}`. Envelopes are
//! binary messages, `seq` and `ack` as big-endian `u64` and then the body.
//! `ack {ack}` acknowledges without a body and doubles as the heartbeat, and
//! `bye` closes the session on purpose.

mod client;
mod link;
mod server;
mod session;
mod state;
mod table;
mod timing;
pub mod tls;
mod wire;

pub use client::{connect, ClientConfig};
pub use server::{
    AuthRequest, Authenticator, Listening, Rejection, ServerConfig, SessionServer, StaticToken,
    DEFAULT_GRACE, DEFAULT_SESSION_PORT, RELAY_PATH, SESSION_PORT_ENV,
};
pub use session::{LinkEvent, Session, SessionEnd, Status, DEFAULT_MAX_BUFFERED};
pub use state::ProtocolError;
pub use timing::{Backoff, Timing, MAX_HEARTBEAT};
pub use wire::{split_body, ExpiryReason, RejectCode, ENVELOPE_HEADER, MAX_BODY, MAX_MESSAGE};

/// Everything that can go wrong with a session.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// The configuration cannot work, for example plain WebSocket off loopback.
    #[error("configuration: {0}")]
    Config(String),
    /// A connection could not be made; a reconnecting client tries again.
    #[error("connect: {0}")]
    Connect(String),
    /// The server refused the hello.
    #[error("rejected ({code:?}): {message}")]
    Rejected { code: RejectCode, message: String },
    /// The peer broke the protocol.
    #[error("protocol: {0}")]
    Protocol(String),
    /// A body is larger than one envelope carries (TC-8); split it with
    /// [`split_body`].
    #[error("body of {size} bytes exceeds the {max}-byte limit")]
    TooLarge { size: usize, max: usize },
    /// The replay buffer is full and the caller asked not to wait.
    #[error("replay buffer is full")]
    BufferFull,
    /// The session ended.
    #[error("session ended: {0:?}")]
    Ended(SessionEnd),
    #[error("timed out")]
    Timeout,
}

/// The session listener's port: `MACHINE_SERVICE_SESSION_PORT`, else 6241.
pub fn session_port_from_env() -> Result<u16, Error> {
    match std::env::var(SESSION_PORT_ENV) {
        Ok(v) => v
            .trim()
            .parse()
            .map_err(|_| Error::Config(format!("{SESSION_PORT_ENV}={v:?} is not a port"))),
        Err(_) => Ok(DEFAULT_SESSION_PORT),
    }
}

#[cfg(test)]
mod sim;
#[cfg(test)]
mod tests;
