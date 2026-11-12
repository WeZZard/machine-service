//! `machine-service-install catalog-description`: print this Mac's
//! service-catalog description (`docs/arch/federation.md` FD-1).
//!
//! The service-catalog registrar publishes a launchd job in Consul from a
//! description file that the operator places in the registrar's own folder.
//! Consul shows the file's tags as they are, so the tags must be right for
//! the Mac the file sits on: every instance carries `machines` and `agents`,
//! an instance with durable exposure on carries `durable` (DC-39), and an
//! instance that can create virtual machines carries `vm` and `tart`.
//!
//! The description states the Mac's permanent hardware UUID as
//! `machine_id` metadata, so a rename never makes it stale (MI-4, DC-87).
//! Clients obtain the current name from the confirmed instance's `/health`.
//!
//! The command only reads. It prints the description on standard output and
//! its reasoning on standard error, and it never writes into the registrar's
//! folder, because those files belong to service-catalog (FD-1, DC-60).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use crate::installer::{self, Exposure, InstallError};

/// The Consul service name every instance registers under (FD-1).
pub const SERVICE_ID: &str = "machine-service";
/// The variable that turns durable exposure on in the LaunchAgent (DC-39).
pub const EXPOSE_VARIABLE: &str = "MACHINE_SERVICE_EXPOSE_DURABLE";
/// The port the daemon serves when `MACHINE_SERVICE_PORT` is unset.
const DEFAULT_PORT: u16 = 6240;
/// How long `tart list` may take before the check gives up on the store.
const TART_LIST_TIMEOUT: Duration = Duration::from_secs(30);

/// What one instance offers, which decides its tags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offers {
    /// Durable exposure is on (DC-39).
    pub durable: bool,
    /// The instance can create virtual machines (MM-1).
    pub virtual_machines: bool,
}

impl Offers {
    /// The tags Consul shows for this instance (FD-1). `disposable-infra`
    /// stays with `vm` and `tart`: it describes the disposable clones, and
    /// the instance registered before the rename already carried it.
    pub fn tags(&self) -> Vec<&'static str> {
        let mut tags = vec!["machines", "agents"];
        if self.durable {
            tags.push("durable");
        }
        if self.virtual_machines {
            tags.extend(["vm", "tart", "disposable-infra"]);
        }
        tags
    }

    /// The description's one-paragraph purpose, naming only what this Mac
    /// offers.
    pub fn purpose(&self) -> String {
        let mut purpose = String::from(
            "Leases machines to AI agents through one HTTP API: GET /machines lists them, \
POST /acquire leases one, exec/push/pull work inside the lease, and release ends it.",
        );
        if self.durable {
            purpose.push_str(
                " This Mac is leased as a durable machine: one exclusive session at a time, \
as the logged-in user, in a scratch workspace that release removes.",
            );
        }
        if self.virtual_machines {
            purpose.push_str(
                " This Mac also offers disposable Tart virtual machines: a lease clones a \
golden base (macOS 26 or Ubuntu 24.04), boots it headless over NAT and destroys it at \
release; at most 2 macOS guests run at once.",
            );
        }
        purpose
    }
}

/// A service-catalog description, in the field order of service-catalog's
/// own examples. The registrar ignores unknown fields and requires `id`,
/// `name`, `purpose`, `protocol`, `expected` and `supervisor`.
#[derive(Debug, Serialize)]
pub struct Description {
    /// The Consul service name.
    pub id: String,
    /// The display name.
    pub name: String,
    /// What the instance is for.
    pub purpose: String,
    /// The protocol, in service-catalog's vocabulary.
    pub protocol: String,
    /// What the instance offers (FD-1).
    pub tags: Vec<String>,
    /// Where the documentation lives.
    pub docs: String,
    /// `always-on`: an absent instance is a fault.
    pub expected: String,
    /// The launchd job the registrar watches.
    pub supervisor: Supervisor,
    /// The HTTP check Consul runs.
    pub health: Health,
    /// The port that is the service.
    pub port: u16,
    /// What the registrar publishes as the registration's own metadata:
    /// `machine_id`, the hardware UUID that `/health` reports (MI-4, DC-87).
    pub meta: BTreeMap<String, String>,
}

/// The launchd job of a description.
#[derive(Debug, Serialize)]
pub struct Supervisor {
    /// Always `launchd`.
    pub kind: String,
    /// The job's label.
    pub label: String,
}

/// The health check of a description.
#[derive(Debug, Serialize)]
pub struct Health {
    /// The path Consul requests.
    pub http_path: String,
    /// How often Consul requests it.
    pub interval: String,
}

/// The metadata key that carries permanent machine identity (MI-4, DC-87).
pub const MACHINE_ID_META: &str = "machine_id";

/// The description of the instance on `machine_id` that offers `offers`
/// on `port`. No mutable host name is stored (MI-4, DC-87).
pub fn description(offers: &Offers, port: u16, machine_id: &str) -> Description {
    Description {
        id: SERVICE_ID.to_string(),
        name: "Machine service".to_string(),
        purpose: offers.purpose(),
        protocol: "machine-service-http".to_string(),
        tags: offers.tags().into_iter().map(str::to_string).collect(),
        docs: "https://github.com/WeZZard/vm-service".to_string(),
        expected: "always-on".to_string(),
        supervisor: Supervisor {
            kind: "launchd".to_string(),
            label: installer::LABEL.to_string(),
        },
        health: Health {
            http_path: "/health".to_string(),
            interval: "30s".to_string(),
        },
        port,
        meta: BTreeMap::from([(MACHINE_ID_META.to_string(), machine_id.to_string())]),
    }
}

/// The description as the file the operator places: two-space JSON and a
/// trailing newline.
pub fn render(description: &Description) -> String {
    let mut text = serde_json::to_string_pretty(description)
        .expect("a description of plain strings and integers always serializes");
    text.push('\n');
    text
}

/// Where the exposure setting came from, for the operator's note.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExposureSource {
    /// `--expose-durable` on the command line.
    Argument,
    /// The installed LaunchAgent's environment.
    Installed,
    /// Neither: exposure is off by default (DC-39).
    Default,
}

/// Resolve durable exposure: the explicit argument wins, then the installed
/// LaunchAgent's `MACHINE_SERVICE_EXPOSE_DURABLE`, then off (DC-39, DC-60).
/// A value other than `on`, `off` or empty is refused rather than read as
/// either, so a typo never adds or drops the `durable` tag.
pub fn resolve_exposure(
    argument: Option<Exposure>,
    installed: &BTreeMap<String, String>,
) -> Result<(bool, ExposureSource), InstallError> {
    if let Some(exposure) = argument {
        return Ok((exposure == Exposure::On, ExposureSource::Argument));
    }
    match installed.get(EXPOSE_VARIABLE).map(String::as_str) {
        None => Ok((false, ExposureSource::Default)),
        Some("") | Some("off") => Ok((false, ExposureSource::Installed)),
        Some("on") => Ok((true, ExposureSource::Installed)),
        Some(other) => Err(InstallError::Refused(format!(
            "the installed LaunchAgent sets {EXPOSE_VARIABLE} to {other:?}; it must be on or off"
        ))),
    }
}

/// Whether the instance configured by `env` can create virtual machines
/// (MM-1): Tart is installed, the image catalog builds with at least one
/// line, and at least one line's base VM exists in the selected Tart store.
///
/// `env` is the instance's environment: the installed LaunchAgent's
/// variables over the installer's defaults. Answers the base VMs found, or
/// the first reason the instance cannot create virtual machines.
pub fn virtual_capability(env: &BTreeMap<String, String>) -> Result<Vec<String>, String> {
    let (effective, bundle) = effective_environment(env)?;
    let tart = match &bundle {
        Some(bundle) => bundle
            .get("profile")
            .and_then(|profile| profile.get("tartPath"))
            .and_then(Value::as_str)
            .map(str::to_string),
        None => effective
            .get("TART")
            .filter(|value| !value.is_empty())
            .cloned()
            .or_else(|| installer::which("tart", effective.get("PATH").map(String::as_str))),
    };
    let tart = match tart {
        Some(path) if Path::new(&path).is_file() && installer::is_executable(Path::new(&path)) => {
            service_layout::tools::admitted(service_layout::tools::Tool::Tart, Path::new(&path))?
        }
        _ => return Err("Tart is not installed".to_string()),
    };

    // No configured image source means no images; no checkout is assumed
    // (IC-7, DC-94).
    let pilot = match &bundle {
        Some(bundle) => bundle
            .get("profile")
            .and_then(|profile| profile.get("imageRepository"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .ok_or("no image source: the profile names no imageRepository")?,
        None => effective
            .get("PILOT_REPO")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .ok_or("no image source: PILOT_REPO is not set")?,
    };
    // The daemon's own strict build (IC-4), so the check and the catalog
    // the daemon serves can never disagree about which lines exist.
    let images = pilot.join("images");
    let identity = machine_service_core::catalog::file::identity(&images);
    let built = machine_service_core::catalog::file::build(&images, &identity)
        .map_err(|failure| failure.to_string())?;
    let bases: BTreeSet<String> = built
        .lines
        .values()
        .map(|line| line.base_vm.clone())
        .collect();

    let mut command = Command::new(&tart);
    command.arg("list");
    command.env_clear();
    for (key, value) in &effective {
        command.env(key, value);
    }
    let output = installer::run_command(&mut command, TART_LIST_TIMEOUT)
        .map_err(|error| format!("tart list failed: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "tart list failed with exit status {}",
            output.status.code().unwrap_or(-1)
        ));
    }
    // `tart list` prints `Source Name ... State`; only local VMs are bases.
    let local: BTreeSet<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .skip(1)
        .filter_map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            (parts.len() >= 2 && parts[0] == "local").then(|| parts[1].to_string())
        })
        .collect();
    let present: Vec<String> = bases.intersection(&local).cloned().collect();
    if present.is_empty() {
        return Err(format!(
            "no configured base VM exists in the Tart store ({})",
            bases.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    Ok(present)
}

/// The instance's environment with a selected environment's exports laid
/// over it, as the installer's own Tart check reads it.
fn effective_environment(
    env: &BTreeMap<String, String>,
) -> Result<(BTreeMap<String, String>, Option<Value>), String> {
    let map = env
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let bundle =
        environment::load_environment_with(None, &map).map_err(|error| error.to_string())?;
    let mut effective = env.clone();
    if let Some(exports) = bundle
        .as_ref()
        .and_then(|bundle| bundle.get("environment"))
        .and_then(Value::as_object)
    {
        for (key, value) in exports {
            if let Some(text) = value.as_str() {
                effective.insert(key.clone(), text.to_string());
            }
        }
    }
    Ok((effective, bundle))
}

/// Run `catalog-description` for the Mac whose home is `home`.
/// The broker's metadata for Consul (BK-4, RP-11): `relay_url` and, once
/// the instance has made its authority, `relay_ca_sha256`. Empty unless the
/// installed instance serves the relay link.
pub fn relay_meta(env: &BTreeMap<String, String>, home: &Path) -> BTreeMap<String, String> {
    let mut meta = BTreeMap::new();
    if env.get("MACHINE_SERVICE_GUEST_LINK").map(String::as_str) != Some("relay") {
        return meta;
    }
    let port = env
        .get(machine_service_core::remote_session::SESSION_PORT_ENV)
        .and_then(|value| value.trim().parse::<u16>().ok())
        .unwrap_or(machine_service_core::remote_session::DEFAULT_SESSION_PORT);
    let url = env
        .get(machine_service_core::relay::RELAY_URL_VARIABLE)
        .filter(|value| !value.trim().is_empty())
        .cloned()
        .unwrap_or_else(|| machine_service_core::relay::default_relay_url(port));
    meta.insert("relay_url".to_string(), url);
    let state = env
        .get("MACHINE_SERVICE_STATE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| home.join(".local/state/machine-service"));
    if let Some(sha256) = std::fs::read_to_string(state.join("relay/tls/ca.pem"))
        .ok()
        .and_then(|pem| machine_service_core::relay_broker::tls::pem_sha256(&pem))
    {
        meta.insert("relay_ca_sha256".to_string(), sha256);
    }
    meta
}

pub fn run(argument: Option<Exposure>, home: &Path) -> Result<(), InstallError> {
    let plist = crate::migration::agent_plist(home, installer::LABEL);
    let installed = match installer::load_old_plist(&plist)? {
        Some(map) => Some(installer::installed_environment(&map)?),
        None => None,
    };
    if installed.is_none() {
        eprintln!(
            "note: no LaunchAgent at {}; describing the installer's defaults",
            plist.display()
        );
    }
    let installed = installed.unwrap_or_default();
    let mut env = installer::default_environment(home);
    env.extend(installed.clone());

    let (durable, source) = resolve_exposure(argument, &installed)?;
    let from = match source {
        ExposureSource::Argument => "from --expose-durable",
        ExposureSource::Installed => "from the installed LaunchAgent",
        ExposureSource::Default => "the default",
    };
    eprintln!(
        "durable exposure: {} ({from})",
        if durable { "on" } else { "off" }
    );
    let virtual_machines = match virtual_capability(&env) {
        Ok(bases) => {
            eprintln!(
                "virtual machines: yes (base VMs present: {})",
                bases.join(", ")
            );
            true
        }
        Err(reason) => {
            eprintln!("virtual machines: no ({reason})");
            false
        }
    };
    let host = env
        .get("MACHINE_SERVICE_HOST")
        .map(String::as_str)
        .unwrap_or("127.0.0.1");
    if host == "127.0.0.1" || host == "localhost" || host == "::1" {
        eprintln!(
            "warning: the instance binds {host}; Consul will publish it loopback_only and the \
CLI on other Macs will list it as unreachable. Reinstall with MACHINE_SERVICE_HOST=0.0.0.0 \
to serve the LAN (FD-1)."
        );
    }
    let port = match env
        .get("MACHINE_SERVICE_PORT")
        .filter(|value| !value.is_empty())
    {
        None => DEFAULT_PORT,
        Some(value) => value.parse::<u16>().map_err(|_| {
            InstallError::Refused(format!(
                "the installed MACHINE_SERVICE_PORT is not a port: {value:?}"
            ))
        })?,
    };
    let offers = Offers {
        durable,
        virtual_machines,
    };
    // Reuse the capability report's hardware UUID source (MI-1/MI-4, DC-87).
    let machine_id = machine_service_core::durable::capabilities::hardware_uuid()
        .ok_or_else(|| InstallError::Refused("cannot read this Mac's hardware UUID".into()))?;
    eprintln!(
        "machine_id: {machine_id} (this Mac's hardware UUID, published as meta.{MACHINE_ID_META})"
    );
    let mut described = description(&offers, port, &machine_id);
    let relay = relay_meta(&env, home);
    if let Some(url) = relay.get("relay_url") {
        eprintln!("relay: the broker is advertised as {url} (meta.relay_url)");
    }
    described.meta.extend(relay);
    print!("{}", render(&described));
    eprintln!(
        "place this file at ~/.config/service-catalog/services/{SERVICE_ID}.json; the registrar \
publishes it on its next pass"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// MI-4/DC-87: the Consul description survives a mutable host name.
    #[test]
    fn the_broker_is_in_the_metadata_only_when_the_relay_link_is_served() {
        let home = tempfile::tempdir().unwrap();
        let mut env = BTreeMap::new();
        assert!(relay_meta(&env, home.path()).is_empty());
        env.insert("MACHINE_SERVICE_GUEST_LINK".into(), "relay".into());
        env.insert("MACHINE_SERVICE_SESSION_PORT".into(), "7001".into());
        let meta = relay_meta(&env, home.path());
        assert!(meta["relay_url"].ends_with(":7001/relay"), "{meta:?}");
        assert!(!meta.contains_key("relay_ca_sha256"));
        env.insert(
            "MACHINE_SERVICE_RELAY_URL".into(),
            "wss://broker.example.test/relay".into(),
        );
        let state = home.path().join("state");
        env.insert("MACHINE_SERVICE_STATE".into(), state.display().to_string());
        let authority = machine_service_core::relay_broker::tls::Authority::load_or_create(
            &state.join("relay/tls"),
        )
        .unwrap();
        let meta = relay_meta(&env, home.path());
        assert_eq!(meta["relay_url"], "wss://broker.example.test/relay");
        assert_eq!(meta["relay_ca_sha256"], authority.sha256());
    }

    #[test]
    fn mi4_catalog_metadata_is_machine_identity_only() {
        let machine_id = "00000000-0000-4000-8000-000000000001";
        let value: Value = serde_json::from_str(&render(&description(
            &Offers {
                durable: true,
                virtual_machines: false,
            },
            6250,
            machine_id,
        )))
        .unwrap();
        assert_eq!(value["meta"], serde_json::json!({"machine_id": machine_id}));
        assert!(value["meta"].get("instance").is_none());
    }

    fn installed(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    /// AC-37, FD-1: the tags follow exposure and the capability check.
    #[test]
    fn ac37_tags_follow_exposure_and_capability() {
        let tart_mac = Offers {
            durable: false,
            virtual_machines: true,
        };
        assert_eq!(
            tart_mac.tags(),
            vec!["machines", "agents", "vm", "tart", "disposable-infra"]
        );
        let durable_mac = Offers {
            durable: true,
            virtual_machines: false,
        };
        assert_eq!(durable_mac.tags(), vec!["machines", "agents", "durable"]);
        let both = Offers {
            durable: true,
            virtual_machines: true,
        };
        assert!(both.tags().contains(&"durable") && both.tags().contains(&"vm"));
        let neither = Offers {
            durable: false,
            virtual_machines: false,
        };
        assert_eq!(neither.tags(), vec!["machines", "agents"]);
    }

    /// AC-37: the purpose names only what the Mac offers.
    #[test]
    fn ac37_purpose_names_only_what_the_mac_offers() {
        let durable_only = Offers {
            durable: true,
            virtual_machines: false,
        }
        .purpose();
        assert!(durable_only.contains("durable machine"));
        assert!(!durable_only.contains("Tart"));
        let tart_only = Offers {
            durable: false,
            virtual_machines: true,
        }
        .purpose();
        assert!(tart_only.contains("Tart"));
        assert!(!tart_only.contains("durable"));
    }

    /// DC-39, DC-60: the argument wins, then the installed agent, then off;
    /// any other installed value is refused.
    #[test]
    fn ac37_exposure_resolution_order() {
        let on = installed(&[(EXPOSE_VARIABLE, "on")]);
        assert_eq!(
            resolve_exposure(Some(Exposure::Off), &on).unwrap(),
            (false, ExposureSource::Argument)
        );
        assert_eq!(
            resolve_exposure(None, &on).unwrap(),
            (true, ExposureSource::Installed)
        );
        assert_eq!(
            resolve_exposure(None, &installed(&[])).unwrap(),
            (false, ExposureSource::Default)
        );
        assert_eq!(
            resolve_exposure(None, &installed(&[(EXPOSE_VARIABLE, "")])).unwrap(),
            (false, ExposureSource::Installed)
        );
        let typo = resolve_exposure(None, &installed(&[(EXPOSE_VARIABLE, "yes")]))
            .unwrap_err()
            .to_string();
        assert!(
            typo.contains(EXPOSE_VARIABLE) && typo.contains("\"yes\""),
            "{typo}"
        );
    }

    /// FD-1: the description carries the registrar's required fields, the
    /// launchd label, port and health path.
    #[test]
    fn ac37_description_shape() {
        let text = render(&description(
            &Offers {
                durable: true,
                virtual_machines: false,
            },
            6240,
            "00000000-0000-4000-8000-000000000001",
        ));
        assert!(text.ends_with("}\n"));
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["id"], "machine-service");
        assert_eq!(value["expected"], "always-on");
        assert_eq!(value["supervisor"]["kind"], "launchd");
        assert_eq!(value["supervisor"]["label"], "com.wezzard.machine-service");
        assert_eq!(value["health"]["http_path"], "/health");
        assert_eq!(value["port"], 6240);
        assert_eq!(
            value["tags"],
            serde_json::json!(["machines", "agents", "durable"])
        );
        assert_eq!(
            value["meta"],
            serde_json::json!({"machine_id": "00000000-0000-4000-8000-000000000001"})
        );
    }

    /// The checked-in description is exactly what the command prints for a
    /// Mac with Tart and images and durable exposure off. The fixture uses
    /// a neutral example UUID, never the development Mac's identity.
    #[test]
    fn checked_in_description_matches_the_generator() {
        let checked_in = include_str!("../../../../services/machine-service.json");
        let generated = render(&description(
            &Offers {
                durable: false,
                virtual_machines: true,
            },
            DEFAULT_PORT,
            "00000000-0000-4000-8000-000000000001",
        ));
        assert_eq!(checked_in, generated);
    }
}
