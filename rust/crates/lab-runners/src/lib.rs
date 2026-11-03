//! `lab-host`: the lab's ephemeral GitHub Actions runners and its lab
//! instances (`docs/arch/lab-runners.md`, LR).
//!
//! - [`dispatcher`] serves queued lab jobs on a VM host: one runner VM per
//!   job, leased from the host's lab instance with the egress allowlist, a
//!   just-in-time runner for that one job inside it, and the VM released
//!   when the job ends or its deadline passes (LR-3).
//! - [`decide`] holds the dispatcher's decisions as functions over values:
//!   which jobs a host starts, given its capacity (LR-7).
//! - [`allowlist`] and [`dns`] build the egress allowlist from GitHub's
//!   published `meta` and names resolved over DNS over TLS (LR-5).
//! - [`lab`] keeps the host's lab instance running (LR-6).
//! - [`token`] reads the dispatcher's token from the login keychain and
//!   never shows it (LR-4).
//!
//! Every external party sits behind an interface ([`github::JobSource`],
//! [`instance::Instance`], [`dns::Resolve`]); a test build reaches them only
//! through its seams ([`seams`]), never GitHub, a real instance or the
//! keychain (DC-251).

pub mod allowlist;
pub mod decide;
pub mod dispatcher;
pub mod dns;
pub mod github;
pub mod instance;
pub mod lab;
pub mod labels;
pub mod runner;
pub mod seams;
pub mod token;

/// The repository whose lab jobs the dispatcher serves.
pub const REPOSITORY: &str = "WeZZard/vm-service";

/// The LaunchAgent's label on every lab host (LR-1).
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
