//! A test build's seams (DC-251): one JSON file, named by
//! `LAB_HOST_TEST_SEAMS`, that stands for every party outside the test.
//! A release reads no such variable and has no seams; a test build without
//! the file refuses to reach GitHub, production or the keychain at all.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::path::PathBuf;

use serde_json::Value;

/// The variable that names the seams file in a test build.
pub const VARIABLE: &str = "LAB_HOST_TEST_SEAMS";

/// What a test build uses in place of the world.
#[derive(Debug, Clone, Default)]
pub struct Seams {
    /// GitHub's API, a loopback fake.
    pub github: Option<String>,
    /// Production, a loopback fake.
    pub production: Option<String>,
    /// The host's role, `vm-host` or `durable`, in place of production's
    /// `/health`.
    pub role: Option<String>,
    /// The local configuration file, in place of
    /// `~/.config/vm-service-lab/controller.json`.
    pub config: Option<PathBuf>,
    /// The address a test environment binds, in place of the LAN address.
    pub address: Option<Ipv4Addr>,
    /// A test environment's HTTP port, in place of 6272.
    pub environment_port: Option<u16>,
    /// The other hosts' test environment endpoints to probe, in place of
    /// discovery.
    pub peers: Vec<(Ipv4Addr, u16)>,
    /// A file holding the token, in place of the keychain.
    pub token: Option<PathBuf>,
    /// The state folder, in place of `~/.local/state/vm-service-lab`.
    pub state: Option<PathBuf>,
    /// The egress files' folder, in place of the guest firewall's.
    pub egress: Option<PathBuf>,
    /// Names and their addresses, in place of DNS over TLS.
    pub dns: BTreeMap<String, Vec<Ipv4Addr>>,
    /// Runner archives by platform: a path and its SHA-256, in place of the
    /// pinned download.
    pub runners: BTreeMap<String, (PathBuf, String)>,
    /// How many rounds `run` makes before it exits.
    pub rounds: Option<u64>,
    /// Seconds between rounds.
    pub interval_s: Option<u64>,
}

impl Seams {
    /// The seams of a test build: the file `LAB_HOST_TEST_SEAMS` names. A
    /// test build without it refuses; a release always answers the empty
    /// seams without reading any variable.
    pub fn load() -> Result<Seams, String> {
        if !service_layout::test_build::is_test_build() {
            return Ok(Seams::default());
        }
        let path = service_layout::test_build::seam(VARIABLE).ok_or_else(|| {
            format!("a test build of lab-host reaches nothing outside its test without {VARIABLE}")
        })?;
        let text = std::fs::read_to_string(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        let value: Value =
            serde_json::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))?;
        let string = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
        let path_of = |key: &str| string(key).map(PathBuf::from);
        let dns = value
            .get("dns")
            .and_then(Value::as_object)
            .map(|names| {
                names
                    .iter()
                    .map(|(name, addresses)| {
                        let list = addresses
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|address| address.as_str()?.parse().ok())
                            .collect();
                        (name.clone(), list)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let runners = value
            .get("runners")
            .and_then(Value::as_object)
            .map(|platforms| {
                platforms
                    .iter()
                    .filter_map(|(platform, archive)| {
                        Some((
                            platform.clone(),
                            (
                                PathBuf::from(archive.get("path")?.as_str()?),
                                archive.get("sha256")?.as_str()?.to_string(),
                            ),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let peers = value
            .get("peers")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|peer| {
                let (address, port) = peer.as_str()?.rsplit_once(':')?;
                Some((address.parse().ok()?, port.parse().ok()?))
            })
            .collect();
        Ok(Seams {
            github: string("github"),
            production: string("production"),
            role: string("role"),
            config: path_of("config"),
            address: string("address").and_then(|address| address.parse().ok()),
            environment_port: value
                .get("environment_port")
                .and_then(Value::as_u64)
                .and_then(|port| u16::try_from(port).ok()),
            peers,
            token: path_of("token"),
            state: path_of("state"),
            egress: path_of("egress"),
            dns,
            runners,
            rounds: value.get("rounds").and_then(Value::as_u64),
            interval_s: value.get("interval_s").and_then(Value::as_u64),
        })
    }
}
