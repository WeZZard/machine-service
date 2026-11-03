//! Production, the host's installed machine-service, as the runner
//! controller uses it on loopback: its capacity, its leases, the egress
//! acquisition of an ephemeral runner with the `ci` credential pack, a file
//! into a guest, an exec and a release (LR-3, DC-283, DC-284). The same
//! HTTP API reads a test environment's `/health` and capacity.

use std::time::Duration;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// A host's macOS slots, as an instance's `GET /images` counts them
/// (DC-18, LR-7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Capacity {
    /// The host's limit.
    pub limit: u32,
    /// Every macOS guest on the host, whoever started it.
    pub taken: u32,
    /// Those another program started, such as an image build or a test
    /// environment.
    pub foreign: u32,
    /// The instance's own running macOS guests.
    pub running: u32,
}

/// A lease of production.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Lease {
    /// The VM's name.
    pub vm: String,
    /// The lease's ID.
    pub lease_id: String,
    /// Its purpose.
    pub purpose: String,
    /// The credential pack its record shows, if any.
    pub env: Option<String>,
}

/// The credential pack of every ephemeral runner VM: empty unless a CI need
/// is added on purpose, and never the agent pack (DC-283).
pub const CI_PACK: &str = "ci";

/// The installed instance's loopback API, which lends the runner VMs.
pub const PRODUCTION: &str = "http://127.0.0.1:6240";

/// Whether a lease's record shows the `ci` pack, which the controller
/// checks before it starts a runner: a request alone proves nothing on an
/// API without authentication (DC-283, FD-5).
pub fn check_ci(lease: &Lease) -> Result<(), String> {
    match lease.env.as_deref() {
        Some(CI_PACK) => Ok(()),
        other => Err(format!(
            "the runner VM {} was leased with the credential pack {:?}, not {CI_PACK:?}; it gets no runner",
            lease.vm,
            other.unwrap_or("none")
        )),
    }
}

/// What the runner controller asks of production.
pub trait Production: Send + Sync {
    /// The host's macOS slots.
    fn capacity(&self) -> Result<Capacity, String>;
    /// Production's current leases.
    fn leases(&self) -> Result<Vec<Lease>, String>;
    /// Acquire a VM; blocks until it is `running` or refused.
    fn acquire(&self, request: &Value) -> Result<Lease, String>;
    /// Place `bytes` at `path` in the guest.
    fn put_file(&self, lease: &Lease, path: &str, bytes: &[u8]) -> Result<(), String>;
    /// Run `script` in the guest's desktop session (the relay link) for at
    /// most `timeout` seconds; answers its exit status and output's tail.
    fn exec(&self, lease: &Lease, script: &str, timeout: u64) -> Result<(i64, String), String>;
    /// Release a VM, which deletes it.
    fn release(&self, vm: &str, reason: &str) -> Result<(), String>;
}

/// The acquisition of a runner VM (LR-3): the image, the `internet`
/// network with the egress allowlist, the `ci` credential pack named
/// explicitly (DC-283), and the TTL that outlasts the deadline.
pub fn runner_request(image: &str, purpose: &str) -> Value {
    json!({
        "purpose": purpose,
        "image": image,
        "env": CI_PACK,
        "network": "internet",
        "egress": "allowlist",
        "ttl_hours": crate::decide::TTL_HOURS,
        "wait": true,
    })
}

/// The capacity of a `GET /images` answer.
pub fn parse_capacity(value: &Value) -> Result<Capacity, String> {
    let capacity = value
        .get("capacity")
        .ok_or("the instance reports no capacity")?;
    let number = |key: &str| capacity.get(key).and_then(Value::as_u64).map(|n| n as u32);
    Ok(Capacity {
        limit: number("macos_limit").ok_or("the instance reports no macOS limit")?,
        taken: number("macos_taken")
            .or_else(|| number("macos_running"))
            .ok_or("the instance reports no macOS slots taken")?,
        foreign: number("foreign_macos_guests").unwrap_or(0),
        running: number("macos_running").unwrap_or(0),
    })
}

/// The virtual machine leases of a `GET /machines` answer, keyed by name
/// or listed.
pub fn parse_leases(value: &Value) -> Vec<Lease> {
    let records: Vec<&Value> = match value.get("machines") {
        Some(Value::Object(map)) => map.values().collect(),
        Some(Value::Array(list)) => list.iter().collect(),
        _ => Vec::new(),
    };
    records
        .into_iter()
        .filter(|record| record.get("kind").and_then(Value::as_str) != Some("durable"))
        .filter_map(parse_lease)
        .collect()
}

/// A lease from a machine record or an acquisition's answer.
pub fn parse_lease(record: &Value) -> Option<Lease> {
    Some(Lease {
        vm: record
            .get("vm")
            .or_else(|| record.get("name"))?
            .as_str()?
            .to_string(),
        lease_id: record
            .get("lease_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        purpose: record
            .get("purpose")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        env: record
            .get("env")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// Percent-encode a query value.
pub fn encode(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

/// An instance's HTTP API at `base`: production's, or a test
/// environment's.
pub struct Http {
    base: String,
}

impl Http {
    /// The instance at `base`, such as [`PRODUCTION`].
    pub fn new(base: &str) -> Http {
        Http {
            base: base.trim_end_matches('/').to_string(),
        }
    }

    fn agent(timeout: Duration) -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .proxy(None)
            .build()
            .into()
    }

    fn answer(
        response: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
        what: &str,
    ) -> Result<Value, String> {
        let mut response = response.map_err(|error| format!("machine-service {what}: {error}"))?;
        let status = response.status().as_u16();
        let text = response
            .body_mut()
            .with_config()
            .limit(64 * 1024 * 1024)
            .read_to_string()
            .unwrap_or_default();
        let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if status != 200 {
            let error = value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or(text.trim());
            return Err(format!("machine-service {what}: HTTP {status}: {error}"));
        }
        Ok(value)
    }

    fn get(&self, path: &str) -> Result<Value, String> {
        let response = Http::agent(Duration::from_secs(60))
            .get(format!("{}{path}", self.base))
            .call();
        Http::answer(response, path)
    }

    fn post(&self, path: &str, body: &Value, timeout: Duration) -> Result<Value, String> {
        let response = Http::agent(timeout)
            .post(format!("{}{path}", self.base))
            .header("Content-Type", "application/json")
            .send(body.to_string());
        Http::answer(response, path)
    }
}

impl Http {
    /// The instance's `/health`, if it answers within a few seconds.
    pub fn health(&self) -> Option<Value> {
        let mut response = Http::agent(Duration::from_secs(5))
            .get(format!("{}/health", self.base))
            .call()
            .ok()?;
        if response.status().as_u16() != 200 {
            return None;
        }
        serde_json::from_str(&response.body_mut().read_to_string().ok()?).ok()
    }
}

impl Production for Http {
    fn capacity(&self) -> Result<Capacity, String> {
        parse_capacity(&self.get("/images")?)
    }

    fn leases(&self) -> Result<Vec<Lease>, String> {
        Ok(parse_leases(&self.get("/machines")?))
    }

    fn acquire(&self, request: &Value) -> Result<Lease, String> {
        let answer = self.post("/acquire", request, Duration::from_secs(1800))?;
        parse_lease(&answer).ok_or_else(|| "the acquisition's answer names no VM".to_string())
    }

    fn put_file(&self, lease: &Lease, path: &str, bytes: &[u8]) -> Result<(), String> {
        let digest = Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let url = format!(
            "{}/machines/{}/files?lease_id={}&path={}",
            self.base,
            encode(&lease.vm),
            encode(&lease.lease_id),
            encode(path)
        );
        let response = Http::agent(Duration::from_secs(1800))
            .put(url)
            .header("Content-Type", "application/octet-stream")
            .header("X-Content-SHA256", &digest)
            .send(bytes);
        Http::answer(response, "files").map(|_| ())
    }

    fn exec(&self, lease: &Lease, script: &str, timeout: u64) -> Result<(i64, String), String> {
        let body = json!({"script": script, "timeout": timeout, "lease_id": lease.lease_id});
        let answer = self.post(
            &format!("/machines/{}/exec", encode(&lease.vm)),
            &body,
            Duration::from_secs(timeout + 300),
        )?;
        let rc = answer.get("rc").and_then(Value::as_i64).unwrap_or(-1);
        let output = answer
            .get("output")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        Ok((rc, output))
    }

    fn release(&self, vm: &str, reason: &str) -> Result<(), String> {
        self.post(
            &format!("/machines/{}/release", encode(vm)),
            &json!({"reason": reason}),
            Duration::from_secs(900),
        )
        .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_runner_vm_is_an_egress_guest_with_the_ci_pack() {
        let request = runner_request("macos26", "lab-42");
        assert_eq!(request["network"], "internet");
        assert_eq!(request["egress"], "allowlist");
        assert_eq!(request["env"], "ci");
        assert_eq!(request["purpose"], "lab-42");
        assert_eq!(request["ttl_hours"], json!(7.0));
    }

    #[test]
    fn capacity_counts_every_macos_guest_on_the_host() {
        let images = json!({"capacity": {"macos_running": 0, "macos_limit": 2, "host_macos_guests": 1,
            "foreign_macos_guests": 1, "macos_taken": 1}});
        assert_eq!(
            parse_capacity(&images).unwrap(),
            Capacity {
                limit: 2,
                taken: 1,
                foreign: 1,
                running: 0
            }
        );
        let older = json!({"capacity": {"macos_running": 1, "macos_limit": 2}});
        assert_eq!(
            parse_capacity(&older).unwrap(),
            Capacity {
                limit: 2,
                taken: 1,
                foreign: 0,
                running: 1
            }
        );
        assert!(parse_capacity(&json!({})).is_err());
    }

    #[test]
    fn leases_are_read_keyed_or_listed() {
        let keyed = json!({"machines": {"pilot-a": {"vm": "pilot-a", "lease_id": "x", "purpose": "lab-1", "env": "ci"},
            "mac": {"kind": "durable", "name": "mac", "lease_id": null}}});
        assert_eq!(
            parse_leases(&keyed),
            vec![Lease {
                vm: "pilot-a".into(),
                lease_id: "x".into(),
                purpose: "lab-1".into(),
                env: Some("ci".into()),
            }]
        );
        let listed = json!({"machines": [{"name": "pilot-b", "purpose": "acc-x", "env": null}]});
        assert_eq!(parse_leases(&listed)[0].vm, "pilot-b");
        assert_eq!(parse_leases(&listed)[0].env, None);
        assert_eq!(encode("/tmp/a b"), "/tmp/a%20b");
    }

    #[test]
    fn only_a_lease_whose_record_shows_the_ci_pack_gets_a_runner() {
        let lease = |env: Option<&str>| Lease {
            vm: "pilot-1".into(),
            env: env.map(str::to_string),
            ..Lease::default()
        };
        assert!(check_ci(&lease(Some("ci"))).is_ok());
        for env in [Some("default"), Some("CI"), Some(""), None] {
            let error = check_ci(&lease(env)).unwrap_err();
            assert!(error.contains("gets no runner"), "{error}");
        }
    }
}
