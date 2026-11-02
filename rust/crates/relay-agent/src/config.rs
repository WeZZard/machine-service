//! The relay agent's configuration: the broker URL and the agent credential
//! that machine-service writes at lease time (pilot-images RA-I2, RA-I3).
//!
//! The file is `~/.config/relay-agent/config.json`. No image holds it, so the
//! agent waits until it exists. It holds a credential, so the agent refuses
//! it unless it is a regular file of the agent's own user with mode 600.

use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;

/// Where the configuration lives under a home directory.
pub fn config_path(home: &Path) -> PathBuf {
    home.join(".config/relay-agent/config.json")
}

/// The variable naming the folder that holds the agent's own files, its
/// configuration and its records, in place of `HOME`. A durable Mac's second
/// instance runs its own agent with it, so the agent's files are its
/// instance's while its commands keep the operator's `HOME` (DC-213).
pub const ROOT_VARIABLE: &str = "RELAY_AGENT_ROOT";

/// The folder the agent's own files lie under: [`ROOT_VARIABLE`] when it
/// is set, else the home directory, as in every guest.
pub fn agent_root() -> PathBuf {
    std::env::var_os(ROOT_VARIABLE)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(guest_actions::user_home)
}

/// The configuration.
#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct Config {
    /// The broker's URL, `wss://…` (or `ws://…` on loopback).
    pub broker_url: String,
    /// The bearer token the broker checks in the session's first message.
    pub agent_credential: String,
    /// The broker's own authority, PEM, trusted beside the public roots, as
    /// machine-service writes it for its self-signed broker (RP-1).
    #[serde(default)]
    pub broker_ca_pem: Option<String>,
}

impl fmt::Debug for Config {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Config")
            .field("broker_url", &self.broker_url)
            .field("agent_credential", &"<redacted>")
            .field(
                "broker_ca_pem",
                &self.broker_ca_pem.as_ref().map(|_| "<pem>"),
            )
            .finish()
    }
}

/// Why the configuration cannot be used yet.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ConfigError {
    /// The file does not exist yet.
    #[error("no configuration at {0}")]
    Missing(PathBuf),
    /// The file is not a regular file.
    #[error("{0} is not a regular file")]
    NotAFile(PathBuf),
    /// Others than the user may read or change the file.
    #[error("refusing {path}: its mode is {mode:o}, and it must be 600")]
    Mode {
        /// The file.
        path: PathBuf,
        /// Its permission bits.
        mode: u32,
    },
    /// The file belongs to another user.
    #[error("refusing {path}: it belongs to user {owner}, not to this user")]
    Owner {
        /// The file.
        path: PathBuf,
        /// Its owner's user ID.
        owner: u32,
    },
    /// The file's content is not a valid configuration.
    #[error("{path} is not a valid configuration: {reason}")]
    Invalid {
        /// The file.
        path: PathBuf,
        /// What is wrong.
        reason: String,
    },
    /// The file could not be read.
    #[error("cannot read {path}: {reason}")]
    Unreadable {
        /// The file.
        path: PathBuf,
        /// The error.
        reason: String,
    },
}

/// Load the configuration at `path`. The mode and owner are checked on the
/// file that was opened, so the file cannot be swapped between the check
/// and the read.
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let unreadable = |error: io::Error| ConfigError::Unreadable {
        path: path.to_path_buf(),
        reason: error.to_string(),
    };
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(ConfigError::Missing(path.to_path_buf()))
        }
        Err(error) => return Err(unreadable(error)),
    };
    let metadata = file.metadata().map_err(unreadable)?;
    if !metadata.is_file() {
        return Err(ConfigError::NotAFile(path.to_path_buf()));
    }
    // SAFETY: `geteuid` has no preconditions and no memory effects.
    let user = unsafe { libc::geteuid() };
    if metadata.uid() != user {
        return Err(ConfigError::Owner {
            path: path.to_path_buf(),
            owner: metadata.uid(),
        });
    }
    let mode = metadata.permissions().mode() & 0o7777;
    if mode != 0o600 {
        return Err(ConfigError::Mode {
            path: path.to_path_buf(),
            mode,
        });
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(unreadable)?;
    let invalid = |reason: String| ConfigError::Invalid {
        path: path.to_path_buf(),
        reason,
    };
    let config: Config =
        serde_json::from_slice(&bytes).map_err(|error| invalid(error.to_string()))?;
    if !(config.broker_url.starts_with("wss://") || config.broker_url.starts_with("ws://")) {
        return Err(invalid("broker_url must be a ws:// or wss:// URL".into()));
    }
    if config.agent_credential.is_empty() {
        return Err(invalid("agent_credential is empty".into()));
    }
    Ok(config)
}

/// Wait until the configuration at `path` loads, checking every `poll`.
/// `report` sees each new reason for waiting once, so a log names a refused
/// file without repeating itself.
pub fn wait_for(path: &Path, poll: Duration, mut report: impl FnMut(&ConfigError)) -> Config {
    let mut last: Option<ConfigError> = None;
    loop {
        match load(path) {
            Ok(config) => return config,
            Err(error) => {
                if last.as_ref() != Some(&error) {
                    report(&error);
                    last = Some(error);
                }
            }
        }
        thread::sleep(poll);
    }
}
