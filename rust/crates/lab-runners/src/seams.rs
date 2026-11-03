//! A test build's seams (DC-251): one JSON file, named by
//! `LAB_HOST_TEST_SEAMS`, that stands for every party outside the test.
//! A release reads no such variable and has no seams; a test build without
//! the file refuses to reach GitHub, an instance or the keychain at all.

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
    /// The lab instance, a loopback fake.
    pub instance: Option<String>,
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
    /// The host's lab endpoints, in place of discovery.
    pub endpoints: Option<Value>,
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
        Ok(Seams {
            github: string("github"),
            instance: string("instance"),
            token: path_of("token"),
            state: path_of("state"),
            egress: path_of("egress"),
            dns,
            runners,
            endpoints: value.get("endpoints").cloned(),
            rounds: value.get("rounds").and_then(Value::as_u64),
            interval_s: value.get("interval_s").and_then(Value::as_u64),
        })
    }
}
