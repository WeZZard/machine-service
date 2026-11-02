//! The relay agent: a daemon in the desktop session of a machine's user that
//! carries actions for the broker (`docs/arch/relay-agent.md`).
//!
//! - [`protocol`]: the relay messages and their encoding.
//! - [`link`]: the transport trait, an in-memory link and a stream link.
//! - [`agent`]: the operations, with at-most-once execution IDs kept in
//!   [`store`].
//! - [`config`]: the broker URL and agent credential, and waiting for them.
//! - [`dial`]: the dial-out loop that serves each link a dialer opens.
//! - [`driver`]: the broker's end of one link.
//! - [`session_link`]: the link over a resumable session to the broker.
//!
//! The actions themselves are those of the guest session agent, shared
//! through the `guest-actions` crate.

pub mod agent;
pub mod config;
pub mod dial;
pub mod driver;
pub mod link;
pub mod protocol;
pub mod session_link;
pub mod store;

pub use agent::{Agent, AgentOptions};
pub use link::{memory_pair, Link, LinkError, MemoryLink, StreamLink};

#[cfg(test)]
mod tests;
