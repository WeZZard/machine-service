//! A second instance: a machine-service instance that a live tool starts
//! beside the installed one, on resources it owns (`docs/arch/test-layers.md`,
//! TL-4, TL-6, TL-7).
//!
//! Three parties share this crate, so they share one definition of what a
//! second instance is:
//!
//! - the daemon, which [`from_env`] lets run as a second instance only on
//!   its own ports, its own state and configuration folders, an explicit
//!   Tart store and a bind address that is not every interface, and which
//!   then reports itself in `/health` as `second_instance` (DC-1xx-LIVE-2);
//! - the launcher, the `second-instance` binary, which starts one under a
//!   root it owns and records it in the root's [`Manifest`], and whose
//!   `stop` removes everything it made (DC-1xx-LIVE-1);
//! - the live clients (`live_e2e`, `live_image_catalog`, `live_guest_session`
//!   and `relay-acceptance`), which [`guard`] refuses to run against any
//!   instance that does not report itself as a second instance, and against
//!   the installed instance's ports (DC-1xx-LIVE-3). There is no override.
//!
//! The installed instance never sets [`MARKER_VARIABLE`], so a positive mark
//! is what a client checks, not a list of the installed instance's paths: a
//! client on another machine cannot know those paths, and `/health` answers
//! anyone on the network, so it names no folder.

use std::net::IpAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The installed instance's HTTP port.
pub const INSTALLED_PORT: u16 = 6240;
/// The installed instance's broker port, `MACHINE_SERVICE_SESSION_PORT`'s
/// default.
pub const INSTALLED_SESSION_PORT: u16 = 6241;
/// Set to the second instance's ID; only the launcher sets it.
pub const MARKER_VARIABLE: &str = "MACHINE_SERVICE_SECOND_INSTANCE";
/// The broker's bind address. Unset is every interface, which the installed
/// instance keeps so that its guests reach the broker (BK-5); a second
/// instance must name one address.
pub const SESSION_HOST_VARIABLE: &str = "MACHINE_SERVICE_SESSION_HOST";
/// The installed instance's state folder, under the operator's home.
pub const INSTALLED_STATE: &str = ".local/state/machine-service";
/// The installed instance's configuration folder, under the operator's home.
pub const INSTALLED_CONFIG: &str = ".config/machine-service";
/// The manifest's file name under the launcher's root.
pub const MANIFEST: &str = "second-instance.json";
/// The LaunchAgent label of the installed instance's relay agent; a second
/// instance's agent carries a suffix that its state folder derives.
pub const INSTALLED_AGENT_LABEL: &str = "com.wezzard.relay-agent";

/// Whether `id` is a second instance's ID: 1 to 16 lower-case letters and
/// digits, so that it fits in a VM name and a LaunchAgent label.
pub fn valid_id(id: &str) -> bool {
    (1..=16).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// The prefix of every VM a second instance clones; the launcher deletes
/// the VMs that carry it and no others.
pub fn vm_prefix(id: &str) -> String {
    format!("si-{id}-")
}

/// The credential packs of an instance whose configuration folder is
/// `config_dir`: a second instance never reads the operator's packs.
pub fn packs_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("vm-credentials")
}

/// Whether a bind address is every interface (`0.0.0.0`, `::`, empty).
pub fn is_wildcard(host: &str) -> bool {
    let host = host.trim().trim_start_matches('[').trim_end_matches(']');
    host.is_empty() || host == "*" || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_unspecified())
}

/// Whether a bind address reaches only this machine.
pub fn is_loopback(host: &str) -> bool {
    let host = host.trim().trim_start_matches('[').trim_end_matches(']');
    host == "localhost" || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// What the daemon knows of itself as a second instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    /// The ID the launcher gave it.
    pub id: String,
    /// Its own credential packs ([`packs_dir`]).
    pub packs_dir: PathBuf,
}

impl Marker {
    /// The prefix of the VMs it clones ([`vm_prefix`]).
    pub fn vm_prefix(&self) -> String {
        vm_prefix(&self.id)
    }
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| {
        path.components()
            .filter(|c| !matches!(c, std::path::Component::CurDir))
            .collect()
    })
}

/// Whether one folder lies in the other, or they are the same.
pub fn overlaps(a: &Path, b: &Path) -> bool {
    let (a, b) = (canonical(a), canonical(b));
    a.starts_with(&b) || b.starts_with(&a)
}

/// The daemon's check of its own environment, `get`, on a Mac whose
/// operator's home is `home`. Without [`MARKER_VARIABLE`] it is not a
/// second instance and nothing is checked. With it, every resource the
/// installed instance uses must be named and must be another one: the
/// port, the broker's port, both bind addresses, the state and
/// configuration folders, and the Tart store, which must be named
/// explicitly even when it is the operator's (DC-1xx-LIVE-4).
pub fn from_env(
    get: &dyn Fn(&str) -> Option<String>,
    home: &Path,
) -> Result<Option<Marker>, String> {
    let Some(id) = get(MARKER_VARIABLE).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    let refuse = |why: String| Err(format!("{MARKER_VARIABLE}={id}: {why}"));
    if !valid_id(&id) {
        return refuse("the ID must be 1 to 16 lower-case letters and digits".into());
    }
    let need = |name: &str| {
        get(name)
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| format!("{MARKER_VARIABLE}={id}: a second instance must set {name}"))
    };
    let port = |name: &str| -> Result<u16, String> {
        need(name)?
            .trim()
            .parse::<u16>()
            .ok()
            .filter(|p| *p > 0)
            .ok_or_else(|| format!("{MARKER_VARIABLE}={id}: {name} is not a port"))
    };
    let http = port("MACHINE_SERVICE_PORT")?;
    let session = port("MACHINE_SERVICE_SESSION_PORT")?;
    if [http, session]
        .iter()
        .any(|p| [INSTALLED_PORT, INSTALLED_SESSION_PORT].contains(p))
    {
        return refuse(format!(
            "ports {INSTALLED_PORT} and {INSTALLED_SESSION_PORT} are the installed instance's"
        ));
    }
    if http == session {
        return refuse("MACHINE_SERVICE_PORT and MACHINE_SERVICE_SESSION_PORT are the same".into());
    }
    for name in ["MACHINE_SERVICE_HOST", SESSION_HOST_VARIABLE] {
        if is_wildcard(&need(name)?) {
            return refuse(format!(
                "{name} must name one address, never every interface"
            ));
        }
    }
    let state = PathBuf::from(need("MACHINE_SERVICE_STATE")?);
    if overlaps(&state, &home.join(INSTALLED_STATE)) {
        return refuse(
            "MACHINE_SERVICE_STATE overlaps the installed instance's state folder".into(),
        );
    }
    let config = PathBuf::from(need("MACHINE_SERVICE_CONFIG_DIR")?);
    if overlaps(&config, &home.join(INSTALLED_CONFIG)) {
        return refuse(
            "MACHINE_SERVICE_CONFIG_DIR overlaps the installed instance's configuration folder"
                .into(),
        );
    }
    if overlaps(&state, &config) {
        return refuse("MACHINE_SERVICE_STATE and MACHINE_SERVICE_CONFIG_DIR overlap".into());
    }
    need("TART_HOME")?;
    Ok(Some(Marker {
        packs_dir: packs_dir(&config),
        id,
    }))
}

/// [`from_env`] over this process's environment.
pub fn from_process_env(home: &Path) -> Result<Option<Marker>, String> {
    from_env(&|name| std::env::var(name).ok(), home)
}

/// A live client's check of the instance it was named: never the installed
/// instance's ports, and only an instance whose `/health` reports itself as
/// a second instance. Answers its ID.
pub fn guard(port: u16, health: &Value) -> Result<String, String> {
    if [INSTALLED_PORT, INSTALLED_SESSION_PORT].contains(&port) {
        return Err(format!(
            "port {port} is the installed instance's; live tools run only against a second \
instance (rust/scripts/second-instance.sh)"
        ));
    }
    match health["second_instance"]["id"].as_str() {
        Some(id) if valid_id(id) => Ok(id.to_string()),
        _ => Err(format!(
            "the instance on port {port} does not report itself as a second instance in /health; \
live tools never run against the installed instance (rust/scripts/second-instance.sh)"
        )),
    }
}

/// Whether a LaunchAgent label is a second instance's relay agent: the
/// installed agent's label with a suffix.
pub fn owned_agent_label(label: &str) -> bool {
    label
        .strip_prefix(INSTALLED_AGENT_LABEL)
        .and_then(|rest| rest.strip_prefix('.'))
        .is_some_and(|suffix| !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// What a launcher run owns, written under its root before anything is
/// made, so that `stop` finds everything after a crash too.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// The instance's ID.
    pub id: String,
    /// The root everything lies under.
    pub root: PathBuf,
    /// Whether the launcher made the root, and so removes it.
    pub created_root: bool,
    /// The HTTP port.
    pub port: u16,
    /// The broker's port.
    pub session_port: u16,
    /// The one address both listeners bind.
    pub bind: String,
    /// The Tart store it clones in.
    pub tart_home: PathBuf,
    /// The guest link.
    pub guest_link: String,
    /// Whether durable exposure is on.
    pub durable: bool,
    /// The durable acceptance cell that needs durable exposure off loopback.
    pub durable_cell: Option<String>,
    /// The daemon's executable.
    pub executable: PathBuf,
    /// The daemon's whole environment.
    pub env: std::collections::BTreeMap<String, String>,
}

impl Manifest {
    /// The manifest file under `root`.
    pub fn file(root: &Path) -> PathBuf {
        root.join(MANIFEST)
    }

    /// The manifest under `root`, if a run left one.
    pub fn read(root: &Path) -> Result<Option<Manifest>, String> {
        match std::fs::read_to_string(Manifest::file(root)) {
            Ok(text) => serde_json::from_str(&text)
                .map(Some)
                .map_err(|e| format!("{}: {e}", Manifest::file(root).display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{}: {e}", Manifest::file(root).display())),
        }
    }

    /// The state folder.
    pub fn state(&self) -> PathBuf {
        self.root.join("state")
    }

    /// The configuration folder.
    pub fn config(&self) -> PathBuf {
        self.root.join("config")
    }

    /// The credential packs.
    pub fn packs(&self) -> PathBuf {
        packs_dir(&self.config())
    }

    /// The prefix of its VMs.
    pub fn vm_prefix(&self) -> String {
        vm_prefix(&self.id)
    }

    /// What it exposes, for a run's evidence.
    pub fn exposure(&self) -> Value {
        serde_json::json!({
            "id": self.id,
            "bind": self.bind,
            "loopback": is_loopback(&self.bind),
            "port": self.port,
            "session_port": self.session_port,
            "durable": self.durable,
            "durable_cell": self.durable_cell,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn good(home: &Path) -> HashMap<String, String> {
        let root = home.join("work/si");
        env(&[
            (MARKER_VARIABLE, "ab12"),
            ("MACHINE_SERVICE_PORT", "6250"),
            ("MACHINE_SERVICE_SESSION_PORT", "6251"),
            ("MACHINE_SERVICE_HOST", "127.0.0.1"),
            (SESSION_HOST_VARIABLE, "127.0.0.1"),
            (
                "MACHINE_SERVICE_STATE",
                root.join("state").to_str().unwrap(),
            ),
            (
                "MACHINE_SERVICE_CONFIG_DIR",
                root.join("config").to_str().unwrap(),
            ),
            ("TART_HOME", home.join(".tart").to_str().unwrap()),
        ])
    }

    fn check(vars: &HashMap<String, String>, home: &Path) -> Result<Option<Marker>, String> {
        from_env(&|k| vars.get(k).cloned(), home)
    }

    #[test]
    fn an_instance_without_the_mark_is_not_checked() {
        assert_eq!(check(&HashMap::new(), Path::new("/h")), Ok(None));
    }

    #[test]
    fn a_second_instance_on_its_own_resources_is_marked() {
        let home = Path::new("/h");
        let marker = check(&good(home), home).unwrap().unwrap();
        assert_eq!(marker.id, "ab12");
        assert_eq!(marker.vm_prefix(), "si-ab12-");
        assert_eq!(marker.packs_dir, home.join("work/si/config/vm-credentials"));
    }

    #[test]
    fn a_second_instance_on_any_installed_resource_is_refused() {
        let home = Path::new("/h");
        let cases: Vec<(&str, String)> = vec![
            ("MACHINE_SERVICE_PORT", "6240".into()),
            ("MACHINE_SERVICE_SESSION_PORT", "6241".into()),
            ("MACHINE_SERVICE_SESSION_PORT", "6250".into()),
            ("MACHINE_SERVICE_HOST", "0.0.0.0".into()),
            (SESSION_HOST_VARIABLE, "::".into()),
            (
                "MACHINE_SERVICE_STATE",
                "/h/.local/state/machine-service".into(),
            ),
            ("MACHINE_SERVICE_STATE", "/h/.local/state".into()),
            (
                "MACHINE_SERVICE_CONFIG_DIR",
                "/h/.config/machine-service/x".into(),
            ),
            ("MACHINE_SERVICE_CONFIG_DIR", "/h/work/si/state/c".into()),
            (MARKER_VARIABLE, "Not-An-Id".into()),
        ];
        for (name, value) in cases {
            let mut vars = good(home);
            vars.insert(name.into(), value.clone());
            assert!(check(&vars, home).is_err(), "{name}={value} was accepted");
        }
        for name in [
            "MACHINE_SERVICE_PORT",
            "MACHINE_SERVICE_SESSION_PORT",
            "MACHINE_SERVICE_HOST",
            SESSION_HOST_VARIABLE,
            "MACHINE_SERVICE_STATE",
            "MACHINE_SERVICE_CONFIG_DIR",
            "TART_HOME",
        ] {
            let mut vars = good(home);
            vars.remove(name);
            let error = check(&vars, home).unwrap_err();
            assert!(error.contains(name), "{error}");
        }
    }

    #[test]
    fn a_client_runs_only_against_a_marked_instance_off_the_installed_ports() {
        let marked = serde_json::json!({"ok": true, "second_instance": {"id": "ab12"}});
        assert_eq!(guard(6250, &marked), Ok("ab12".into()));
        assert!(guard(6240, &marked).is_err());
        assert!(guard(6241, &marked).is_err());
        assert!(guard(6250, &serde_json::json!({"ok": true})).is_err());
        assert!(guard(
            6250,
            &serde_json::json!({"second_instance": {"id": "../x"}})
        )
        .is_err());
    }

    #[test]
    fn bind_addresses_are_classified() {
        for wildcard in ["0.0.0.0", "::", "[::]", "", "*"] {
            assert!(is_wildcard(wildcard), "{wildcard}");
        }
        assert!(!is_wildcard("127.0.0.1"));
        assert!(!is_wildcard("198.51.100.7"));
        assert!(is_loopback("127.0.0.1") && is_loopback("::1") && is_loopback("localhost"));
        assert!(!is_loopback("198.51.100.7"));
    }

    #[test]
    fn only_a_suffixed_label_is_a_second_instances_agent() {
        assert!(owned_agent_label("com.wezzard.relay-agent.0123456789ab"));
        assert!(!owned_agent_label("com.wezzard.relay-agent"));
        assert!(!owned_agent_label("com.wezzard.relay-agent."));
        assert!(!owned_agent_label("com.wezzard.relay-agentx.01"));
        assert!(!owned_agent_label("com.wezzard.machine-service"));
    }
}
