//! Install and remove the `com.wezzard.machine-service` launchd agent.
//!
//! This crate is the Rust port of `bin/install-vm-service.sh`. The shell script
//! embedded a Python program that validated the selected environment and the
//! console configuration with `environment_config` and `console_config`; this
//! port performs the same checks through the [`environment`] and [`console`]
//! workspace crates.
//!
//! Every refusal prints `machine-service installation refused: <reason>` on stderr
//! and exits `1`, matching the Python wrapper.

pub mod catalog_description;
pub mod installer;
pub mod migration;
pub use machine_service_core::plist;
pub mod relay_agent;

pub use installer::{main_entry, Action, Cli, Exposure, InstallError};
