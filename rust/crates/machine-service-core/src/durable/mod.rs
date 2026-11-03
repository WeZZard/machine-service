//! The durable machine: the Mac a machine-service instance runs on, leased
//! as an exclusive session with a scratch workspace (`docs/arch/machines.md`,
//! `docs/arch/durable-machine.md`, `docs/arch/agent-keys.md`,
//! `docs/arch/hygiene.md`).
//!
//! Exposure is local configuration of each instance and is off by default
//! (DC-39). With exposure off the daemon lists no durable machine and
//! refuses every durable request before any lock or workspace is touched.

pub mod agents;
pub mod apps;
pub mod capabilities;
pub mod config;
pub mod exec;
pub mod hostlock;
pub mod identity;
pub mod inventory;
pub mod launch;
pub mod lease;
pub mod process;
pub mod relay_agent;
pub mod seam;
pub mod session_env;

pub use config::DurableConfig;
pub use lease::{DurableMachine, Reply};
