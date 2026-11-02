//! The relay agent's link to the broker: a resumable session of the
//! `remote-session` crate (`docs/arch/relay-agent.md` RA-3,
//! `docs/arch/relay-protocol.md`).
//!
//! The session already survives broken connections; a [`SessionLink`]
//! closes only when the session ends. When it ends expired, the dial-out
//! loop of [`crate::dial`] opens a new session, and the agent's hello on it
//! reports what it had in flight.

use std::path::PathBuf;

use remote_session::{ClientConfig, Resolver, Session, SessionEnd};

use crate::config::Config;
use crate::link::{Link, LinkError};

/// The variable naming a PEM file of extra authorities the agent trusts for
/// the broker, beside the configuration's `broker_ca_pem`, such as a test
/// rig's edge authority.
pub const CA_BUNDLE_VARIABLE: &str = "RELAY_AGENT_CA_BUNDLE";
/// The variable naming one `host:port:address` resolution, as `curl
/// --resolve` takes it, for a broker name no resolver knows, such as a test
/// rig's.
pub const RESOLVE_VARIABLE: &str = "RELAY_AGENT_RESOLVE";

/// The hello attributes of an agent session (RP-3).
pub fn agent_attributes() -> serde_json::Value {
    serde_json::json!({"role": "agent"})
}

/// How the agent dials, beyond its configuration file.
#[derive(Clone, Default)]
pub struct DialOptions {
    /// Extra PEM authorities to trust.
    pub extra_roots_pem: Vec<String>,
    /// A resolver for the broker's host name; `None` is the system's.
    pub resolver: Option<Resolver>,
    /// Session timing, for tests; `None` is the default.
    pub timing: Option<remote_session::Timing>,
}

impl DialOptions {
    /// The options the environment names: [`CA_BUNDLE_VARIABLE`] and
    /// [`RESOLVE_VARIABLE`].
    pub fn from_environment() -> Result<DialOptions, String> {
        let mut options = DialOptions::default();
        if let Some(path) = std::env::var_os(CA_BUNDLE_VARIABLE) {
            let path = PathBuf::from(path);
            let pem = std::fs::read_to_string(&path)
                .map_err(|error| format!("{CA_BUNDLE_VARIABLE}={}: {error}", path.display()))?;
            options.extra_roots_pem.push(pem);
        }
        if let Ok(spec) = std::env::var(RESOLVE_VARIABLE) {
            options.resolver = Some(
                remote_session::fixed_resolver(&spec)
                    .map_err(|error| format!("{RESOLVE_VARIABLE}: {error}"))?,
            );
        }
        Ok(options)
    }
}

/// A [`Link`] over a remote session.
pub struct SessionLink {
    session: Session,
}

impl SessionLink {
    /// Wrap an open session.
    pub fn new(session: Session) -> SessionLink {
        SessionLink { session }
    }

    /// The session.
    pub fn session(&self) -> &Session {
        &self.session
    }
}

fn link_error(error: remote_session::Error) -> LinkError {
    match error {
        remote_session::Error::TooLarge { size, .. } => LinkError::TooLarge(size),
        remote_session::Error::Ended(SessionEnd::Closed { .. }) => LinkError::Closed,
        remote_session::Error::Ended(end) => LinkError::Failed(format!("session ended: {end:?}")),
        other => LinkError::Failed(other.to_string()),
    }
}

impl Link for SessionLink {
    fn send(&self, payload: Vec<u8>) -> Result<(), LinkError> {
        self.session.send(payload).map(|_| ()).map_err(link_error)
    }

    fn receive(&self) -> Result<Vec<u8>, LinkError> {
        self.session.recv().map_err(link_error)
    }

    fn close(&self) {
        self.session.close();
    }
}

/// Open a new session to the broker of `config`, as an agent. The first
/// connection is made before this returns, so a wrong URL, trust or
/// credential fails here; the host name is resolved again on every later
/// connection attempt (TC-4, TC-6).
pub fn dial(config: &Config, options: &DialOptions) -> Result<SessionLink, LinkError> {
    let mut roots: Vec<&[u8]> = options
        .extra_roots_pem
        .iter()
        .map(|pem| pem.as_bytes())
        .collect();
    if let Some(pem) = &config.broker_ca_pem {
        roots.push(pem.as_bytes());
    }
    let tls = remote_session::tls::client_config(&roots)
        .map_err(|error| LinkError::Failed(error.to_string()))?;
    let mut client = ClientConfig::new(&config.broker_url, &config.agent_credential);
    client.attributes = agent_attributes();
    client.tls = Some(tls);
    client.resolver = options.resolver.clone();
    if let Some(timing) = &options.timing {
        client.timing = timing.clone();
    }
    remote_session::connect(client)
        .map(SessionLink::new)
        .map_err(|error| LinkError::Failed(error.to_string()))
}
