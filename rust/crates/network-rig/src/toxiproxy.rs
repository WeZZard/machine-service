//! A client of Toxiproxy's HTTP API, the rig's control channel for faults.

use std::net::SocketAddr;
use std::time::Duration;

use serde_json::{json, Value};

use crate::profile::Toxic;
use crate::RigError;

/// Toxiproxy's API at one address.
#[derive(Debug, Clone)]
pub struct Api {
    base: String,
    agent: ureq::Agent,
}

impl Api {
    /// The API served at `addr`.
    pub fn new(addr: SocketAddr) -> Api {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(10)))
            .http_status_as_error(false)
            .proxy(None)
            .max_redirects(0)
            .build()
            .into();
        Api {
            base: format!("http://{addr}"),
            agent,
        }
    }

    /// The server's version, which also says the API is up.
    pub fn version(&self) -> Result<String, RigError> {
        let mut response = self
            .agent
            .get(format!("{}/version", self.base))
            .call()
            .map_err(|error| RigError::Toxiproxy(error.to_string()))?;
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(|error| RigError::Toxiproxy(error.to_string()))?;
        let value: Value = serde_json::from_str(&text).unwrap_or(Value::String(text.clone()));
        Ok(value
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or(text.trim())
            .to_string())
    }

    /// Create a proxy that listens on `listen` and forwards to `upstream`.
    pub fn create_proxy(
        &self,
        name: &str,
        listen: SocketAddr,
        upstream: SocketAddr,
    ) -> Result<(), RigError> {
        let body = json!({
            "name": name,
            "listen": listen.to_string(),
            "upstream": upstream.to_string(),
            "enabled": true,
        });
        self.send("POST", "/proxies", Some(body)).map(drop)
    }

    /// Turn a proxy on or off. Turning it off closes every connection.
    pub fn set_enabled(&self, proxy: &str, enabled: bool) -> Result<(), RigError> {
        self.send(
            "POST",
            &format!("/proxies/{proxy}"),
            Some(json!({ "enabled": enabled })),
        )
        .map(drop)
    }

    /// Add a toxic to a proxy.
    pub fn add_toxic(&self, proxy: &str, toxic: &Toxic) -> Result<(), RigError> {
        let body = serde_json::to_value(toxic).map_err(|e| RigError::Toxiproxy(e.to_string()))?;
        self.send("POST", &format!("/proxies/{proxy}/toxics"), Some(body))
            .map(drop)
    }

    /// Remove a toxic from a proxy.
    pub fn remove_toxic(&self, proxy: &str, toxic: &str) -> Result<(), RigError> {
        self.send("DELETE", &format!("/proxies/{proxy}/toxics/{toxic}"), None)
            .map(drop)
    }

    /// The names of a proxy's toxics.
    pub fn toxics(&self, proxy: &str) -> Result<Vec<String>, RigError> {
        let value = self.send("GET", &format!("/proxies/{proxy}/toxics"), None)?;
        Ok(value
            .as_array()
            .map(|toxics| {
                toxics
                    .iter()
                    .filter_map(|toxic| toxic.get("name").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default())
    }

    fn send(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value, RigError> {
        let url = format!("{}{path}", self.base);
        let result = match (method, body) {
            ("GET", _) => self.agent.get(&url).call(),
            ("DELETE", _) => self.agent.delete(&url).call(),
            (_, Some(body)) => self
                .agent
                .post(&url)
                .header("Content-Type", "application/json")
                .send(body.to_string()),
            (_, None) => self.agent.post(&url).send_empty(),
        };
        let mut response =
            result.map_err(|error| RigError::Toxiproxy(format!("{method} {path}: {error}")))?;
        let status = response.status().as_u16();
        let text = response.body_mut().read_to_string().unwrap_or_default();
        if !(200..300).contains(&status) {
            return Err(RigError::Toxiproxy(format!(
                "{method} {path}: HTTP {status}: {}",
                text.trim()
            )));
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }
}
