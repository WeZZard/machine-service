//! The durable Mac's own relay agent at installation (`docs/arch/relay-agent.md`
//! RA-2, RA-4; `docs/arch/installation.md` IN-3; DC-140, DC-213).
//!
//! The mechanism is machine-service's own,
//! [`machine_service_core::durable::relay_agent`], which the daemon also runs
//! at startup: the agent's label, home and files derive from the instance's
//! state folder, so an installation and a second instance's agent (TL-6) are
//! the same mechanism and never share a file.

use std::path::{Path, PathBuf};

use machine_service_core::durable::relay_agent as core;
pub use machine_service_core::durable::relay_agent::{
    durable_lease_current, identity, paths, plist, Ensured, Identity, Paths, DEFAULT_LABEL,
    EXECUTABLE, IDENTIFIER, RECORDS,
};

use crate::installer::InstallError;

/// What one installation of the relay agent needs.
#[derive(Debug, Clone)]
pub struct Spec {
    /// The operator's home folder.
    pub home: PathBuf,
    /// The instance's state folder, which names the agent and holds the
    /// credential and the broker's authority.
    pub state_dir: PathBuf,
    /// The instance's session port, where its broker listens (BK-1).
    pub session_port: u16,
    /// The release folder to install from.
    pub release_dir: PathBuf,
    /// Make a new agent credential first (RA-4).
    pub rotate: bool,
    /// The Developer ID team the operator configured
    /// (`MACHINE_SERVICE_RELAY_AGENT_TEAM_ID`); without it the instance's
    /// record, or the first Developer ID team installed (DC-231).
    pub team: Option<String>,
}

impl Spec {
    fn core(&self, identity: Identity) -> core::Spec {
        core::Spec {
            identity,
            state_dir: self.state_dir.clone(),
            session_port: self.session_port,
            // The installed agent dials its broker directly (DC-327).
            agent_port: None,
            release_dir: self.release_dir.clone(),
            rotate: self.rotate,
            team: self.team.clone(),
        }
    }
}

/// Check, writing nothing, that the release of `spec` may be installed: its
/// `SHA256SUMS`, its signature against the instance's trust (DC-224,
/// DC-231) and against the agent in place, whose signing never downgrades
/// (DC-232). Prints which signature it carries and how it is trusted.
pub fn check(spec: &Spec) -> Result<Vec<u8>, InstallError> {
    let checked = core::check(&spec.core(identity(&spec.state_dir, &spec.home)))
        .map_err(InstallError::Refused)?;
    println!("relay agent release signed {}", checked.describe());
    Ok(checked.bytes)
}

/// Install the relay agent of `spec` and start it in the desktop session,
/// restarting it only when its executable or LaunchAgent changed. The
/// caller has refused a current durable lease already.
pub fn install(spec: &Spec) -> Result<(), InstallError> {
    let wanted = spec.core(identity(&spec.state_dir, &spec.home).with_login_path());
    let done = core::ensure(&wanted, true).map_err(InstallError::Refused)?;
    let what = match done {
        Ensured::Unchanged => "unchanged".to_string(),
        Ensured::Configured => "configuration updated".to_string(),
        Ensured::Started { build } => format!("loaded; release sha256 {build}"),
    };
    println!(
        "relay agent {} {what} ({}; broker {})",
        wanted.identity.label,
        paths(&wanted.identity).executable.display(),
        wanted.broker_url(),
    );
    Ok(())
}

/// Unload the relay agent of the instance on `state_dir` and remove its
/// files. Nothing to remove is not a failure.
pub fn remove(home: &Path, state_dir: &Path) -> Result<bool, InstallError> {
    let identity = identity(state_dir, home);
    let present = core::remove(&identity).map_err(InstallError::Refused)?;
    if present {
        println!(
            "removed relay agent {} ({})",
            identity.label,
            paths(&identity).plist.display()
        );
    }
    Ok(present)
}
