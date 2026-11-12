//! A test build's daemon refuses the installed defaults (DC-251).
//!
//! A daemon that `cargo test` built serves a test, never the operator's
//! installation: it refuses to start on the installed ports, to bind
//! outside loopback, to take its selected environment from an inherited
//! `VM_ENVIRONMENT_FILE`, and to use any installed folder of the account
//! (`service_layout::test_build::INSTALLED_FOLDERS`), whatever `HOME` says.
//! The checks run before the daemon reads or writes any of them. A release
//! runs none of them.

use std::path::{Path, PathBuf};

use service_layout::test_build::{is_test_build, owned_bind, owned_path, owned_port, FEATURE};

use crate::config::{Config, GuestLink};

/// The variable a release reads for its selected environment, which a test
/// build never takes from its environment.
pub const ENVIRONMENT_VARIABLE: &str = "VM_ENVIRONMENT_FILE";

/// Refuse the inherited selection of an environment in a test build, which
/// selects one only with `--environment`.
pub fn check_environment_selection() -> Result<(), String> {
    if is_test_build() && std::env::var_os(ENVIRONMENT_VARIABLE).is_some() {
        return Err(format!(
            "this is a test build ({FEATURE}), which never takes {ENVIRONMENT_VARIABLE} from its environment; a test selects its environment with --environment"
        ));
    }
    Ok(())
}

/// Refuse, in a test build, every installed default `config` would use.
pub fn check(config: &Config) -> Result<(), String> {
    if !is_test_build() {
        return Ok(());
    }
    owned_port("MACHINE_SERVICE_PORT", config.port)?;
    owned_bind("MACHINE_SERVICE_HOST", &config.host)?;
    if config.guest_link == GuestLink::Relay {
        let session = remote_session::session_port_from_env().map_err(|error| error.to_string())?;
        owned_port("the relay session port", session)?;
    }
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"));
    owned_path("the state folder", &config.state_dir)?;
    owned_path("the Tart store", &tart_store(config, &home))?;
    // The live layer runs with the account's own `HOME` on purpose.
    if !service_layout::test_build::is_live() {
        owned_path("the credential packs", &home.join(".config/vm-credentials"))?;
    }
    let console = std::env::var_os("MACHINE_SERVICE_CONSOLE_CONFIG")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config/machine-service/console.json"));
    owned_path("the console configuration", &console)?;
    if let Ok(executable) = std::env::current_exe() {
        owned_path("the daemon's executable", &executable)?;
    }
    Ok(())
}

/// The Tart store `config` uses, as `RealHost::tart_store_root` finds it.
fn tart_store(config: &Config, home: &Path) -> PathBuf {
    config
        .environment
        .as_ref()
        .and_then(|bundle| bundle.get("profile"))
        .and_then(|profile| profile.get("tartHome"))
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("TART_HOME").map(PathBuf::from))
        .unwrap_or_else(|| home.join(".tart"))
}
