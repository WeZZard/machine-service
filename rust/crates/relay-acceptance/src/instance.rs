//! A machine-service instance over its HTTP API: discovery, acquisition,
//! renewal and release (`docs/arch/relay-protocol.md`, RP-11).

use std::time::Duration;

use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};

/// One instance, by its base URL, `http://<name>:<port>`.
#[derive(Debug, Clone)]
pub struct Instance {
    base: String,
    agent: ureq::Agent,
}

/// What discovery says of the broker (BK-4).
#[derive(Debug, Clone)]
pub struct Discovery {
    /// The advertised `relay_url`.
    pub relay_url: String,
    /// The advertised authority fingerprint.
    pub relay_ca_sha256: String,
    /// The authority, from `GET /relay/ca.pem`, checked against the
    /// fingerprint.
    pub ca_pem: String,
    /// The whole `/health` answer.
    pub health: Value,
}

impl Instance {
    /// The instance at `base`.
    pub fn new(base: &str) -> Instance {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(1800)))
            .http_status_as_error(false)
            .proxy(None)
            .max_redirects(0)
            .build()
            .into();
        Instance {
            base: base.trim_end_matches('/').to_string(),
            agent,
        }
    }

    /// The base URL.
    pub fn base(&self) -> &str {
        &self.base
    }

    fn send(&self, method: &str, path: &str, body: Option<Value>) -> Result<(u16, String), String> {
        let url = format!("{}{path}", self.base);
        let result = match (method, body) {
            ("GET", _) => self.agent.get(&url).call(),
            (_, Some(body)) => self
                .agent
                .post(&url)
                .header("Content-Type", "application/json")
                .send(body.to_string()),
            (_, None) => self.agent.post(&url).send_empty(),
        };
        let mut response = result.map_err(|error| format!("{method} {url}: {error}"))?;
        let status = response.status().as_u16();
        let text = response
            .body_mut()
            .with_config()
            .limit(64 * 1024 * 1024)
            .read_to_string()
            .map_err(|error| format!("{method} {url}: {error}"))?;
        Ok((status, text))
    }

    fn json(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value, String> {
        let (status, text) = self.send(method, path, body)?;
        let value: Value = serde_json::from_str(&text)
            .map_err(|_| format!("{method} {path}: HTTP {status}: {}", text.trim()))?;
        if !(200..300).contains(&status) {
            return Err(format!("{method} {path}: HTTP {status}: {value}"));
        }
        Ok(value)
    }

    /// `GET /health`, and the authority it names (RP-1, RP-11).
    pub fn discover(&self) -> Result<Discovery, String> {
        let health = self.json("GET", "/health", None)?;
        let field = |name: &str| {
            health
                .get(name)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| format!("/health has no {name}: is the relay link on?"))
        };
        let relay_url = field("relay_url")?;
        let relay_ca_sha256 = field("relay_ca_sha256")?;
        let (status, ca_pem) = self.send("GET", "/relay/ca.pem", None)?;
        if status != 200 {
            return Err(format!("GET /relay/ca.pem: HTTP {status}"));
        }
        let fingerprint = pem_sha256(&ca_pem)?;
        if fingerprint != relay_ca_sha256 {
            return Err(format!(
                "the authority served hashes to {fingerprint}, discovery says {relay_ca_sha256}"
            ));
        }
        Ok(Discovery {
            relay_url,
            relay_ca_sha256,
            ca_pem,
            health,
        })
    }

    /// `POST /acquire`, waiting until the lease runs.
    pub fn acquire(&self, image: &str, purpose: &str, ttl_hours: f64) -> Result<Value, String> {
        self.json(
            "POST",
            "/acquire",
            Some(json!({
                "purpose": purpose,
                "image": image,
                "ttl_hours": ttl_hours,
                "wait": true,
            })),
        )
    }

    /// `POST /acquire` of a durable machine (DM).
    pub fn acquire_durable(&self, machine: &str, purpose: &str) -> Result<Value, String> {
        self.json(
            "POST",
            "/acquire",
            Some(json!({"purpose": purpose, "machine": machine, "wait": true})),
        )
    }

    /// `GET /machines`.
    pub fn machines(&self) -> Result<Value, String> {
        self.json("GET", "/machines", None)
    }

    /// `GET /machines/<name>`.
    pub fn record(&self, name: &str) -> Result<Value, String> {
        self.json("GET", &format!("/machines/{name}"), None)
    }

    /// `POST /machines/<name>/heartbeat`.
    pub fn heartbeat(&self, name: &str, body: Value) -> Result<Value, String> {
        self.json("POST", &format!("/machines/{name}/heartbeat"), Some(body))
    }

    /// `POST /machines/<name>/release`.
    pub fn release(&self, name: &str, body: Value) -> Result<Value, String> {
        self.json("POST", &format!("/machines/{name}/release"), Some(body))
    }
}

/// The lower-case hexadecimal SHA-256 of the first certificate in `pem`, as
/// `relay_ca_sha256` names an authority (RP-1).
pub fn pem_sha256(pem: &str) -> Result<String, String> {
    let certs = network_rig::tls::parse_pem_certs(pem).map_err(|e| e.to_string())?;
    Ok(crate::client::hex(&Sha256::digest(certs[0].as_ref())))
}

/// The host and port of a `wss://host:port/path` URL.
pub fn url_host_port(url: &str) -> Result<(String, u16), String> {
    let rest = url
        .strip_prefix("wss://")
        .or_else(|| url.strip_prefix("ws://"))
        .ok_or_else(|| format!("{url} is not a WebSocket URL"))?;
    let authority = rest.split('/').next().unwrap_or_default();
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !host.ends_with(']') || host.starts_with('[') => (
            host.trim_start_matches('[')
                .trim_end_matches(']')
                .to_string(),
            port.parse::<u16>()
                .map_err(|_| format!("{url} has a bad port"))?,
        ),
        _ => (authority.to_string(), 443),
    };
    if host.is_empty() {
        return Err(format!("{url} has no host"));
    }
    Ok((host, port))
}

/// The host of an `http://host:port` base URL.
pub fn base_host(base: &str) -> String {
    let rest = base.split_once("://").map(|(_, rest)| rest).unwrap_or(base);
    let authority = rest.split('/').next().unwrap_or_default();
    match authority.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => host,
        _ => authority,
    }
    .trim_start_matches('[')
    .trim_end_matches(']')
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_split_into_host_and_port() {
        assert_eq!(
            url_host_port("wss://studio.local:6241/relay").unwrap(),
            ("studio.local".to_string(), 6241)
        );
        assert_eq!(
            url_host_port("wss://tunnel.example/relay").unwrap(),
            ("tunnel.example".to_string(), 443)
        );
        assert_eq!(
            url_host_port("wss://[2001:db8::1]:7000/relay").unwrap(),
            ("2001:db8::1".to_string(), 7000)
        );
        assert!(url_host_port("https://x/relay").is_err());
        assert_eq!(base_host("http://mac.local:6262"), "mac.local");
        assert_eq!(base_host("http://192.0.2.4:6262/"), "192.0.2.4");
    }
}
