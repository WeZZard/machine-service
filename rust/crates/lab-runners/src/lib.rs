//! `lab-host`: the lab's runner controller (`docs/arch/lab-runners.md`, LR).
//!
//! - On a VM host, [`dispatcher`] serves queued lab jobs: one ephemeral
//!   runner VM per job, lent by production (the host's installed
//!   machine-service) with the `ci` credential pack and the egress
//!   allowlist, a just-in-time runner for that one job inside it, and the
//!   VM released when the job ends or its deadline passes (LR-3).
//! - [`environment`] runs each run's test environment, the run's own branch
//!   build, verified against the run and started with the `second-instance`
//!   launcher for the run's duration only (LR-6).
//! - On a durable Mac, [`durable`] starts the test environment only for a
//!   run its owner dispatched by hand (LR-10).
//! - [`decide`] and [`runs`] hold the decisions as functions over values:
//!   which jobs a host starts, given its capacity (LR-7), and which runs it
//!   serves (LR-11).
//! - [`allowlist`] and [`dns`] build the egress allowlist from GitHub's
//!   published `meta` and names resolved over DNS over TLS (LR-5).
//! - [`token`] reads the controller's token from the login keychain and
//!   never shows it (LR-4).
//!
//! Every external party sits behind an interface ([`github::JobSource`],
//! [`production::Production`], [`dns::Resolve`]); a test build reaches them
//! only through its seams ([`seams`]), never GitHub, production or the
//! keychain (DC-251).

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub mod allowlist;
pub mod config;
pub mod decide;
pub mod dispatcher;
pub mod dns;
pub mod durable;
pub mod environment;
pub mod github;
pub mod lab;
pub mod labels;
pub mod production;
pub mod runner;
pub mod runs;
pub mod seams;
pub mod token;

/// The repository whose lab jobs the controller serves.
pub const REPOSITORY: &str = "WeZZard/vm-service";

/// The LaunchAgent's label on every Mac that runs a controller (LR-1).
pub const LAUNCH_AGENT: &str = "com.wezzard.vm-service.lab";

/// Log one line with the time, to standard error, which the LaunchAgent
/// keeps in its log. Never a token, a JIT config, an address or a name.
pub fn log(message: &str) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default();
    eprintln!("{now} lab-host: {message}");
}

/// Messages said once in a while, not every round.
#[derive(Default)]
pub struct Told(Mutex<BTreeMap<String, Instant>>);

impl Told {
    /// Log `message` unless it was logged within `every`.
    pub fn say(&self, message: &str, every: Duration) {
        let mut told = self.0.lock().expect("told");
        if told.get(message).is_none_or(|at| at.elapsed() >= every) {
            log(message);
            told.insert(message.to_string(), Instant::now());
        }
    }
}
