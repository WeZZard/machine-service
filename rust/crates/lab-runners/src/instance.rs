//! The host's lab instance, as the dispatcher uses it: its capacity, its
//! leases, an egress acquisition, a file into a guest, an exec and a
//! release (LR-3). Only the lab instance, never the installed one (DC-245).

use std::time::Duration;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// A host's macOS slots, as its lab instance's `GET /images` counts them
/// (DC-18, LR-7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Capacity {
    /// The host's limit.
    pub limit: u32,
    /// Every macOS guest on the host, whoever started it.
    pub taken: u32,
    /// Those another program started, such as an image build.
    pub foreign: u32,
}

/// A lease of the lab instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    /// The VM's name.
    pub vm: String,
    /// The lease's ID.
    pub lease_id: String,
    /// Its purpose.
    pub purpose: String,
}

/// What the dispatcher asks of its lab instance.
pub trait Instance: Send + Sync {
    /// The host's macOS slots.
    fn capacity(&self) -> Result<Capacity, String>;
    /// The instance's current leases.
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
/// network with the egress allowlist, no credential pack, and the TTL that
/// outlasts the deadline.
pub fn runner_request(image: &str, purpose: &str) -> Value {
    json!({
        "purpose": purpose,
        "image": image,
        "env": "none",
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

/// The lab instance's HTTP API at `base`.
pub struct Http {
    base: String,
}

impl Http {
    /// The instance at `base`, such as `http://<address>:6272`.
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
        let mut response = response.map_err(|error| format!("lab instance {what}: {error}"))?;
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
            return Err(format!("lab instance {what}: HTTP {status}: {error}"));
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

impl Instance for Http {
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
    fn a_runner_vm_is_an_egress_guest_without_a_pack() {
        let request = runner_request("macos26", "lab-42");
        assert_eq!(request["network"], "internet");
        assert_eq!(request["egress"], "allowlist");
        assert_eq!(request["env"], "none");
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
                foreign: 1
            }
        );
        let older = json!({"capacity": {"macos_running": 1, "macos_limit": 2}});
        assert_eq!(
            parse_capacity(&older).unwrap(),
            Capacity {
                limit: 2,
                taken: 1,
                foreign: 0
            }
        );
        assert!(parse_capacity(&json!({})).is_err());
    }

    #[test]
    fn leases_are_read_keyed_or_listed() {
        let keyed = json!({"machines": {"si-lab-a": {"vm": "si-lab-a", "lease_id": "x", "purpose": "lab-1"},
            "mac": {"kind": "durable", "name": "mac", "lease_id": null}}});
        assert_eq!(
            parse_leases(&keyed),
            vec![Lease {
                vm: "si-lab-a".into(),
                lease_id: "x".into(),
                purpose: "lab-1".into()
            }]
        );
        let listed = json!({"machines": [{"name": "si-lab-b", "purpose": "acc-x"}]});
        assert_eq!(parse_leases(&listed)[0].vm, "si-lab-b");
        assert_eq!(encode("/tmp/a b"), "/tmp/a%20b");
    }
}
