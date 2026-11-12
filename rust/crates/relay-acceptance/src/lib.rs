//! The relay acceptance suite of TL-7 (`docs/arch/test-layers.md`): AC-55
//! to AC-70 of `docs/acceptance/relay.acceptance`, run by a real client
//! against real targets through the network rig.
//!
//! - [`target::RealTarget`] is a cell's target: a machine-service instance
//!   with the relay link on, found through its discovery, and a lease on it.
//! - [`client::RelayClient`] is the Rust test client of the relay protocol.
//! - [`scenarios::suite`] is the scenarios, each under the profiles its
//!   tags name; `network_rig::cell::run_cell` runs them in one cell under
//!   one profile and writes `<evidence>/<cell>/<profile>/report.json`.
//!
//! The suite is opt-in: the `relay-acceptance` binary runs one cell under
//! one profile against the instance an operator names, and `tests/live.rs`
//! runs it from `cargo test` when asked (`#[ignore]`).

pub mod client;
pub mod guest;
pub mod instance;
pub mod scenarios;
pub mod stats;
pub mod target;
