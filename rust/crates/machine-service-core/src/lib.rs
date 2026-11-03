//! `machine-service` core: lease state, Tart lifecycle, SSH access, and the HTTP
//! operations the daemon exposes.

pub mod catalog;
pub mod config;
pub mod console_api;
pub mod durable;
pub mod error;
pub mod guest_link;
pub mod host;
pub mod image_capabilities;
pub mod isolation;
pub mod launchd;
pub mod lines;
pub mod network;
pub mod ops;
pub mod packs;
pub mod plist;
pub mod proc;
pub mod python;
pub mod reentrant;
pub mod relay;
pub mod second;
pub mod service;
pub mod snapshot;
pub mod ssh;
pub mod state;
pub mod tart;

/// Python-compatible JSON number handling, re-exported for the daemon.
pub use environment::python_json;

/// The automatic garbage-collection interval, in seconds.
pub const GC_INTERVAL_S: u64 = 60;

// Re-export the sibling crates the daemon consumes, so `machine-service` needs only
// one dependency.
pub use acquisition_options;
pub use application_catalog;
pub use console as console_crate;
pub use control_only;
pub use environment;
pub use guest_firewall;
pub use lease_keys;
pub use relay_broker;
pub use remote_session;
pub use service_layout;

pub use error::{OpError, OpResult};
pub use host::{BootProcess, Host, RealHost, TartOutput};
pub use service::Service;
