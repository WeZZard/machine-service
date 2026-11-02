//! The relay link (`docs/arch/relay-agent.md`, `docs/arch/broker.md`,
//! `docs/arch/relay-protocol.md`), chosen with
//! `MACHINE_SERVICE_GUEST_LINK=relay`.
//!
//! - [`Relay`] holds the broker, its TLS authority and its listener on
//!   `MACHINE_SERVICE_SESSION_PORT` at `/relay` (BK-1).
//! - At acquisition, [`Service::bootstrap_relay`] writes the relay agent's
//!   configuration into the clone over SSH, uploads the relay agent when the
//!   image has none or another build, starts it in the desktop session, and
//!   waits for it to register and to answer a probe command and a
//!   checksummed transfer (RA-7). A lease whose relay agent does not
//!   register is served over SSH and says so in its record (RA-8).
//! - A `relay` lease's exec, push and pull go through its relay agent.
//! - Release and expiry revoke the lease's credentials at once (RP-10).

use std::collections::HashMap;
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use acquisition_options::GuestNetwork;
use relay_agent::protocol::{Op, Outcome};
use relay_broker::tls::Authority;
use relay_broker::{Answer, Broker, BrokerConfig, Role, RunError, Target};
use remote_session::{Listening, ServerConfig, SessionServer};
use serde_json::{json, Value};

use crate::error::{OpError, OpResult};
use crate::service::Service;

/// The lease record's `link` when the relay agent carries its actions.
pub const LINK_RELAY: &str = "relay";
/// The lease record's `link` when its relay agent did not register (RA-8).
pub const LINK_SSH_FALLBACK: &str = "ssh-fallback";
/// The broker URL discovery advertises, instead of the default (BK-4).
pub const RELAY_URL_VARIABLE: &str = "MACHINE_SERVICE_RELAY_URL";
/// The broker URL written into a guest's relay agent configuration, instead
/// of the host's address on the guest's network.
pub const GUEST_URL_VARIABLE: &str = "MACHINE_SERVICE_RELAY_GUEST_URL";
/// Seconds a new clone's relay agent gets to register (RA-8).
pub const REGISTER_TIMEOUT_VARIABLE: &str = "MACHINE_SERVICE_RELAY_REGISTER_TIMEOUT";
/// Seconds a call waits for a relay agent that has no session (RP-8).
pub const AGENT_WAIT_VARIABLE: &str = "MACHINE_SERVICE_RELAY_AGENT_WAIT";
/// The relay agent build to upload, instead of the one beside the daemon.
pub const AGENT_VARIABLE: &str = "MACHINE_SERVICE_RELAY_AGENT";

/// Where the image installs the relay agent, and where this service uploads
/// it (pilot-images RA-I1).
pub const GUEST_EXECUTABLE: &str = ".local/libexec/relay-agent/relay-agent";
/// The relay agent's LaunchAgent label (pilot-images RA-I2).
pub const LAUNCH_AGENT_LABEL: &str = "com.wezzard.relay-agent";
/// The relay agent's systemd user unit (pilot-images RA-I2).
pub const SYSTEMD_UNIT: &str = "relay-agent.service";
/// The file of the RA-7 transfer probe, under the guest's home.
const PROBE_FILE: &str = ".machine-service-relay-probe";

/// A marker every relay bootstrap command carries as its `$0`, so a log or
/// a test can tell them apart.
pub const COMMAND_MARKER: &str = "machine-service-relay";

/// The relay link's settings, from the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelaySettings {
    /// The session listener's port (BK-1).
    pub session_port: u16,
    /// The URL discovery advertises (BK-4).
    pub relay_url: String,
    /// The URL guests are given, when not derived from the guest's network.
    pub guest_url: Option<String>,
    /// How long a new clone's relay agent gets to register.
    pub register_timeout: Duration,
    /// How long a call waits for a relay agent without a session.
    pub agent_wait: Duration,
}

fn seconds(variable: &str, default: u64) -> Result<Duration, String> {
    match std::env::var(variable) {
        Ok(value) if !value.trim().is_empty() => value
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|s| *s > 0)
            .map(Duration::from_secs)
            .ok_or_else(|| format!("{variable}={value:?} is not a positive number of seconds")),
        _ => Ok(Duration::from_secs(default)),
    }
}

fn websocket_url(variable: &str, value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.starts_with("wss://") || value.starts_with("ws://") {
        Ok(value.to_string())
    } else {
        Err(format!("{variable}={value:?} is not a ws:// or wss:// URL"))
    }
}

/// The default broker URL: this host's name and the session port (BK-4).
/// On macOS the name is the Bonjour name, `<LocalHostName>.local`, which
/// every Mac on the network resolves; `gethostname` may give a bare DHCP
/// name that nothing resolves.
pub fn default_relay_url(port: u16) -> String {
    let host = relay_broker::net::network_host_name()
        .or_else(relay_broker::net::host_name)
        .unwrap_or_else(|| "localhost".into());
    format!("wss://{host}:{port}{}", remote_session::RELAY_PATH)
}

impl RelaySettings {
    /// Read the settings: `MACHINE_SERVICE_SESSION_PORT` (6241),
    /// `MACHINE_SERVICE_RELAY_URL`, `MACHINE_SERVICE_RELAY_GUEST_URL`,
    /// `MACHINE_SERVICE_RELAY_REGISTER_TIMEOUT` (120 s) and
    /// `MACHINE_SERVICE_RELAY_AGENT_WAIT` (600 s).
    pub fn from_environment() -> Result<RelaySettings, String> {
        let session_port = remote_session::session_port_from_env().map_err(|e| e.to_string())?;
        let relay_url = match std::env::var(RELAY_URL_VARIABLE) {
            Ok(value) if !value.trim().is_empty() => websocket_url(RELAY_URL_VARIABLE, &value)?,
            _ => default_relay_url(session_port),
        };
        let guest_url = match std::env::var(GUEST_URL_VARIABLE) {
            Ok(value) if !value.trim().is_empty() => {
                Some(websocket_url(GUEST_URL_VARIABLE, &value)?)
            }
            _ => None,
        };
        Ok(RelaySettings {
            session_port,
            relay_url,
            guest_url,
            register_timeout: seconds(REGISTER_TIMEOUT_VARIABLE, 120)?,
            agent_wait: seconds(AGENT_WAIT_VARIABLE, 600)?,
        })
    }
}

/// The path of the relay agent build for a guest of `kind`:
/// `MACHINE_SERVICE_RELAY_AGENT`, else `relay-agent-<kind>` beside the
/// daemon, else the host-native `relay-agent` there.
pub fn agent_path_for(kind: &str) -> OpResult<PathBuf> {
    if let Some(path) = std::env::var_os(AGENT_VARIABLE).filter(|p| !p.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    service_layout::guest_build_beside_daemon(service_layout::Executable::RelayAgent, kind)
        .ok_or_else(|| OpError::new("relay agent build unavailable"))
}

/// Read the relay agent build for a guest of `kind`.
pub fn agent_bytes_for(kind: &str) -> OpResult<Vec<u8>> {
    let path = agent_path_for(kind)?;
    std::fs::read(&path).map_err(|error| {
        OpError::new(format!(
            "relay agent build unavailable at {}: {error}",
            path.display()
        ))
    })
}

/// The broker and its listener.
pub struct Relay {
    /// The settings it runs with.
    pub settings: RelaySettings,
    /// The broker.
    pub broker: Broker,
    /// The broker's TLS authority.
    pub authority: Arc<Authority>,
    listening: Mutex<Option<Listening>>,
    /// Client credentials issued at acquisition, until the acquisition's
    /// answer carries them.
    credentials: Mutex<HashMap<String, String>>,
}

impl Relay {
    /// Make or load the authority under `<state>/relay/tls`, and start the
    /// broker on the session listener, on every interface, under TLS: the
    /// guests reach it on their network's host address (BK-5).
    pub fn start(
        settings: RelaySettings,
        state_dir: &Path,
        log: Arc<dyn Fn(&str) + Send + Sync>,
    ) -> Result<Arc<Relay>, String> {
        let authority = Arc::new(Authority::load_or_create(&state_dir.join("relay/tls"))?);
        let sink = Arc::clone(&log);
        let broker = Broker::new(
            BrokerConfig {
                agent_wait: settings.agent_wait,
                ..BrokerConfig::default()
            },
            move |line| sink(line),
        );
        let mut server = ServerConfig::new(broker.authenticator());
        server.tls = Some(relay_broker::tls::server_config(
            Arc::clone(&authority),
            relay_broker::tls::host_names,
        )?);
        let server = SessionServer::new(server).map_err(|e| e.to_string())?;
        let listener = TcpListener::bind(("0.0.0.0", settings.session_port)).map_err(|e| {
            format!(
                "relay: cannot listen on port {} ({}): {e}",
                settings.session_port,
                remote_session::SESSION_PORT_ENV
            )
        })?;
        let listening = server.listen(listener).map_err(|e| e.to_string())?;
        broker.serve(server);
        log(&format!(
            "relay: broker listening on port {} at {}; advertised as {}; authority {}",
            settings.session_port,
            remote_session::RELAY_PATH,
            settings.relay_url,
            authority.sha256()
        ));
        Ok(Arc::new(Relay {
            settings,
            broker,
            authority,
            listening: Mutex::new(Some(listening)),
            credentials: Mutex::new(HashMap::new()),
        }))
    }

    /// Stop accepting connections.
    pub fn stop(&self) {
        if let Some(listening) = self
            .listening
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            listening.stop();
        }
    }

    /// The fields `/health` carries (BK-4, RP-11).
    pub fn discovery(&self) -> Vec<(&'static str, String)> {
        vec![
            ("relay_url", self.settings.relay_url.clone()),
            ("relay_ca_sha256", self.authority.sha256().to_string()),
        ]
    }

    /// The acquisition answer's `relay` object (RP-11).
    pub fn answer(&self, credential: &str) -> Value {
        json!({
            "url": self.settings.relay_url,
            "credential": credential,
            "ca_sha256": self.authority.sha256(),
            "ca_pem": self.authority.cert_pem(),
        })
    }

    fn stash(&self, vm: &str, credential: String) {
        self.credentials
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(vm.to_string(), credential);
    }

    /// The client credential of a new lease, once.
    pub fn take_credential(&self, vm: &str) -> Option<String> {
        self.credentials
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(vm)
    }
}

/// The relay agent's configuration file for one lease.
fn agent_config(broker_url: &str, credential: &str, ca_pem: &str) -> Vec<u8> {
    let mut text = serde_json::to_string_pretty(&json!({
        "broker_url": broker_url,
        "agent_credential": credential,
        "broker_ca_pem": ca_pem,
    }))
    .expect("a configuration serializes");
    text.push('\n');
    text.into_bytes()
}

/// `sh -c` with `script`, named by [`COMMAND_MARKER`] and `step`.
fn sh(script: &str, step: &str) -> String {
    format!(
        "sh -c {} {COMMAND_MARKER}-{step}",
        crate::ssh::shell_quote(script)
    )
}

/// Print the SHA-256 of the guest's relay agent, or `none`.
pub fn inspect_command() -> String {
    sh(
        &format!(
            "p=\"$HOME/{GUEST_EXECUTABLE}\"; if [ -f \"$p\" ]; then \
{{ shasum -a 256 \"$p\" 2>/dev/null || sha256sum \"$p\"; }} | cut -d' ' -f1; else echo none; fi"
        ),
        "inspect",
    )
}

/// Store the relay agent from standard input, atomically.
pub fn upload_command() -> String {
    sh(
        &format!(
            "set -e; p=\"$HOME/{GUEST_EXECUTABLE}\"; d=$(dirname \"$p\"); mkdir -p \"$d\"; \
t=\"$d/.relay-agent.$$\"; (umask 077; cat >\"$t\"); chmod 755 \"$t\"; mv -f \"$t\" \"$p\""
        ),
        "upload",
    )
}

/// Store the configuration from standard input, atomically, mode 600 in a
/// mode-700 folder (DC-126).
pub fn config_command() -> String {
    sh(
        "set -e; d=\"$HOME/.config/relay-agent\"; mkdir -p \"$d\"; chmod 700 \"$d\"; \
t=\"$d/.config.json.$$\"; (umask 077; cat >\"$t\"); chmod 600 \"$t\"; mv -f \"$t\" \"$d/config.json\"",
        "config",
    )
}

/// The command that runs the start script given on standard input.
pub fn start_command() -> String {
    format!("sh -s {COMMAND_MARKER}-start")
}

/// The start script for a guest of `kind`: install the job when the image
/// has none, wait for the desktop session, and (re)start the relay agent in
/// it, so it runs the build and the configuration just written.
pub fn start_script(kind: &str) -> String {
    if kind == "macos" {
        // The desktop session's launchd domain is gui/<uid>; it exists once
        // the user is logged in at the display. A LaunchAgent bootstrapped
        // there runs in that session (Aqua), with its keychain and TCC.
        format!(
            r#"set -eu
label={LAUNCH_AGENT_LABEL}
exe="$HOME/{GUEST_EXECUTABLE}"
plist="$HOME/Library/LaunchAgents/$label.plist"
log="$HOME/Library/Logs/relay-agent.log"
uid=$(id -u)
mkdir -p "$HOME/Library/LaunchAgents" "$HOME/Library/Logs"
if [ ! -f "$plist" ]; then
  tmp="$plist.tmp.$$"
  cat >"$tmp" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>$label</string>
	<key>ProgramArguments</key>
	<array>
		<string>$exe</string>
		<string>run</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>LimitLoadToSessionType</key>
	<string>Aqua</string>
	<key>ProcessType</key>
	<string>Interactive</string>
	<key>StandardOutPath</key>
	<string>$log</string>
	<key>StandardErrorPath</key>
	<string>$log</string>
</dict>
</plist>
PLIST
  mv -f "$tmp" "$plist"
  echo "relay-start: installed $plist"
fi
i=0
until launchctl print "gui/$uid" >/dev/null 2>&1; do
  i=$((i + 1))
  if [ "$i" -gt 120 ]; then echo "relay-start: no desktop session (gui/$uid) after 120 s"; exit 3; fi
  sleep 1
done
if launchctl print "gui/$uid/$label" >/dev/null 2>&1; then
  launchctl kickstart -k "gui/$uid/$label"
else
  launchctl bootstrap "gui/$uid" "$plist"
fi
launchctl print "gui/$uid/$label" | grep -E '^[[:space:]]*(state|pid) = ' | sed 's/^[[:space:]]*/relay-start: /'
"#
        )
    } else {
        // The systemd user manager serves the user's sessions; a unit wanted
        // by graphical-session.target starts with the desktop session and
        // gets its environment (DISPLAY, the session bus, the keyring).
        format!(
            r#"set -eu
exe="$HOME/{GUEST_EXECUTABLE}"
units="$HOME/.config/systemd/user"
unit="$units/{SYSTEMD_UNIT}"
uid=$(id -u)
export XDG_RUNTIME_DIR="${{XDG_RUNTIME_DIR:-/run/user/$uid}}"
export DBUS_SESSION_BUS_ADDRESS="${{DBUS_SESSION_BUS_ADDRESS:-unix:path=$XDG_RUNTIME_DIR/bus}}"
# An agent that already ran (the image's) restarts onto the new build and
# configuration; one that did not is started once.
was=$(systemctl --user is-active {SYSTEMD_UNIT} 2>/dev/null || true)
mkdir -p "$units"
if [ ! -f "$unit" ]; then
  cat >"$unit.tmp.$$" <<UNIT
[Unit]
Description=Relay agent for machine-service
PartOf=graphical-session.target
After=graphical-session.target

[Service]
ExecStart=$exe run
Restart=always
RestartSec=1
SuccessExitStatus=143

[Install]
WantedBy=graphical-session.target
UNIT
  mv -f "$unit.tmp.$$" "$unit"
  echo "relay-start: installed $unit"
fi
systemctl --user daemon-reload
systemctl --user enable {SYSTEMD_UNIT} >/dev/null 2>&1 || true
i=0
until systemctl --user is-active --quiet graphical-session.target; do
  i=$((i + 1))
  if [ "$i" -gt 120 ]; then echo "relay-start: graphical-session.target is not active after 120 s"; exit 3; fi
  sleep 1
done
if [ "$was" = active ]; then
  systemctl --user restart {SYSTEMD_UNIT}
else
  systemctl --user start {SYSTEMD_UNIT}
fi
echo "relay-start: state = $(systemctl --user is-active {SYSTEMD_UNIT})"
"#
        )
    }
}

/// Where a guest reaches the broker: the override, else the host's address
/// on the guest's network (BK-5).
fn guest_url(settings: &RelaySettings, guest_ip: &str) -> Result<String, String> {
    if let Some(url) = &settings.guest_url {
        return Ok(url.clone());
    }
    let ip: Ipv4Addr = guest_ip
        .parse()
        .map_err(|_| format!("the guest's address {guest_ip} is not IPv4"))?;
    let host = relay_broker::net::address_toward(ip).ok_or_else(|| {
        format!("this host has no address on the subnet of the guest's {guest_ip}")
    })?;
    Ok(format!(
        "wss://{host}:{}{}",
        settings.session_port,
        remote_session::RELAY_PATH
    ))
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn reason_of_release(expired: bool) -> &'static str {
    if expired {
        "expired"
    } else {
        "released"
    }
}

/// An exec's outcome as the HTTP answer of RA-5 over the relay.
fn exit_answer(vm: &str, execution: &str, answer: &Answer, timeout_s: u64) -> OpResult<Value> {
    match &answer.outcome {
        Outcome::Exit {
            rc,
            signal,
            timed_out,
            ..
        } => {
            if *timed_out {
                return Err(OpError::new(format!(
                    "the command timed out after {timeout_s}s and was killed"
                )));
            }
            let text = String::from_utf8_lossy(&answer.bytes);
            let mut body = json!({
                "vm": vm,
                "rc": rc.unwrap_or(255),
                "output": tail_chars(&text, 64000),
                "link": LINK_RELAY,
                "execution_id": execution,
            });
            if let Some(signal) = signal {
                body["signal"] = json!(signal);
            }
            Ok(body)
        }
        other => Err(outcome_error(other)),
    }
}

fn outcome_error(outcome: &Outcome) -> OpError {
    match outcome {
        Outcome::Failed {
            code,
            message,
            committed: false,
        } => OpError::new(format!("relay: {code}: {message}")),
        Outcome::Failed { code, message, .. } => OpError::uncertain(format!(
            "relay: {code}: {message}; whether it took effect is unknown"
        )),
        Outcome::Interrupted { message } => OpError::uncertain(format!(
            "relay: the relay agent stopped while the operation ran: {message}"
        )),
        other => OpError::new(format!("relay: unexpected outcome {other:?}")),
    }
}

fn tail_chars(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    text.chars().skip(count - max).collect()
}

/// An execution ID for an HTTP request: the body's `execution_id`, else a
/// new one.
fn execution_of(body: &Value) -> OpResult<String> {
    match body.get("execution_id") {
        None | Some(Value::Null) => Ok(uuid::Uuid::new_v4().to_string()),
        Some(Value::String(id)) if !id.is_empty() && id.len() <= 256 => Ok(id.clone()),
        Some(_) => Err(OpError::new(
            "execution_id must be a string of 1 to 256 bytes",
        )),
    }
}

impl Service {
    /// The relay link, when this instance runs one.
    pub fn relay(&self) -> Option<&Arc<Relay>> {
        self.relay.get()
    }

    /// Install the relay link, and accept again the credentials of the
    /// `relay` leases that survived a restart.
    pub fn install_relay(&self, relay: Arc<Relay>) -> OpResult<()> {
        let data = self.state.read()?;
        if let Some(records) = data.get("vms").and_then(Value::as_object) {
            for (vm, record) in records {
                if record.get("link").and_then(Value::as_str) != Some(LINK_RELAY) {
                    continue;
                }
                let target = Target::Lease(vm.clone());
                for (field, role) in [
                    ("relay_agent_sha256", Role::Agent),
                    ("relay_client_sha256", Role::Client),
                ] {
                    if let Some(hash) = record.get(field).and_then(Value::as_str) {
                        relay.broker.admit(&target, role, hash, None);
                    }
                }
                self.log(&format!(
                    "relay: {vm}: credentials admitted again after restart"
                ));
            }
        }
        self.relay
            .set(relay)
            .map_err(|_| OpError::new("the relay link was installed twice"))
    }

    /// Revoke a lease's relay credentials and end its sessions.
    pub(crate) fn revoke_relay(&self, vm: &str, expired: bool) {
        if let Some(relay) = self.relay() {
            let target = Target::Lease(vm.to_string());
            relay.take_credential(vm);
            relay.broker.revoke(&target, reason_of_release(expired));
        }
    }

    /// The relay target of a lease whose actions go through its relay agent.
    pub(crate) fn relay_target(&self, record: &Value) -> Option<Target> {
        self.relay()?;
        (record.get("link").and_then(Value::as_str) == Some(LINK_RELAY))
            .then(|| record.get("vm").and_then(Value::as_str))
            .flatten()
            .map(|vm| Target::Lease(vm.to_string()))
    }

    /// RA-7 for a new clone, already reachable over SSH with its lease key:
    /// returns the record fields that say which link serves the lease. It
    /// never fails the acquisition; a relay agent that does not register
    /// leaves the lease on SSH (RA-8).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn bootstrap_relay(
        &self,
        vm: &str,
        kind: &str,
        network: GuestNetwork,
        ip: &str,
        user: &str,
        key_dir: &Path,
    ) -> Value {
        let Some(relay) = self.relay() else {
            return json!({});
        };
        let target = Target::Lease(vm.to_string());
        let agent_credential = relay.broker.issue(&target, Role::Agent, None);
        let client_credential = relay.broker.issue(&target, Role::Client, None);
        match self.relay_bootstrap_steps(
            relay,
            vm,
            kind,
            network,
            ip,
            user,
            key_dir,
            &agent_credential,
        ) {
            Ok(agent) => {
                self.log(&format!(
                    "relay: {vm}: relay agent registered and verified ({} {}); link relay",
                    agent["os"].as_str().unwrap_or("?"),
                    agent["arch"].as_str().unwrap_or("?")
                ));
                let fields = json!({
                    "link": LINK_RELAY,
                    "relay_agent": agent,
                    "relay_agent_sha256": relay_broker::credential_sha256(&agent_credential),
                    "relay_client_sha256": relay_broker::credential_sha256(&client_credential),
                });
                relay.stash(vm, client_credential);
                fields
            }
            Err(reason) => {
                relay.broker.revoke(&target, "released");
                self.log(&format!(
                    "WARN: relay: {vm}: {reason}; serving the lease over SSH (link ssh-fallback, RA-8)"
                ));
                json!({"link": LINK_SSH_FALLBACK, "link_reason": reason})
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn relay_bootstrap_steps(
        &self,
        relay: &Relay,
        vm: &str,
        kind: &str,
        network: GuestNetwork,
        ip: &str,
        user: &str,
        key_dir: &Path,
        credential: &str,
    ) -> Result<Value, String> {
        let settings = &relay.settings;
        if network == GuestNetwork::Internet {
            let firewall = self.host.guest_firewall()?;
            if firewall.broker_port != Some(settings.session_port) {
                return Err(format!(
                    "the guest firewall lets internet guests reach {}, not the broker port {} (DC-107); reinstall it with: {}",
                    firewall
                        .broker_port
                        .map_or("no host port".to_string(), |p| format!("port {p}")),
                    settings.session_port,
                    guest_firewall::install_command_for(
                        &crate::isolation::installer_path(),
                        &firewall.table,
                        Some(settings.session_port)
                    )
                ));
            }
        }
        let broker_url = guest_url(settings, ip)?;
        let ssh = |command: &str, stdin: Option<Vec<u8>>, timeout: u64| {
            self.host
                .ssh(ip, user, key_dir, command, stdin, timeout)
                .ok_or_else(|| "ssh failed".to_string())
        };

        // The build: the image's when it is the one this service holds,
        // else this service's (RA-6).
        let build = self.host.relay_agent(kind).map_err(|e| e.to_string())?;
        let want = sha256_hex(&build);
        let (rc, have) = ssh(&inspect_command(), None, 60)?;
        let have = have.trim().lines().last().unwrap_or("").trim().to_string();
        if rc != 0 {
            return Err(format!(
                "inspecting the guest's relay agent failed ({rc}): {have}"
            ));
        }
        if have != want {
            self.log(&format!(
                "relay: {vm}: uploading the relay agent ({} bytes; the guest has {})",
                build.len(),
                if have == "none" {
                    "none"
                } else {
                    "another build"
                }
            ));
            let (rc, text) = ssh(&upload_command(), Some(build), 300)?;
            if rc != 0 {
                return Err(format!(
                    "uploading the relay agent failed ({rc}): {}",
                    text.trim()
                ));
            }
            let (_, after) = ssh(&inspect_command(), None, 60)?;
            if after.trim().lines().last().unwrap_or("").trim() != want {
                return Err("the uploaded relay agent does not match its SHA-256".into());
            }
        }
        let config = agent_config(&broker_url, credential, relay.authority.cert_pem());
        let (rc, text) = ssh(&config_command(), Some(config), 60)?;
        if rc != 0 {
            return Err(format!(
                "writing the relay agent configuration failed ({rc}): {}",
                text.trim()
            ));
        }
        let (rc, text) = ssh(&start_command(), Some(start_script(kind).into_bytes()), 300)?;
        if rc != 0 {
            return Err(format!(
                "starting the relay agent in the desktop session failed ({rc}): {}",
                text.trim()
            ));
        }
        self.log(&format!(
            "relay: {vm}: relay agent started ({}); broker {broker_url}; waiting {}s for it to register",
            text.trim().replace('\n', "; "),
            settings.register_timeout.as_secs()
        ));

        let target = Target::Lease(vm.to_string());
        let agent = relay
            .broker
            .wait_for_agent(&target, settings.register_timeout)
            .ok_or_else(|| {
                format!(
                    "the relay agent did not register within {}s",
                    settings.register_timeout.as_secs()
                )
            })?;
        // A probe command and a checksummed transfer (RA-7).
        let bound = Duration::from_secs(60);
        let run = |op: Op, upload: Vec<u8>| {
            relay
                .broker
                .run(
                    &target,
                    &uuid::Uuid::new_v4().to_string(),
                    op,
                    upload,
                    bound,
                )
                .map_err(|e| format!("the relay agent did not answer in time: {e:?}"))
        };
        let probe = run(
            Op::Exec {
                command: "echo relay-probe".into(),
                stdin: false,
                timeout_s: 30.0,
            },
            Vec::new(),
        )?;
        let ran = matches!(probe.outcome, Outcome::Exit { rc: Some(0), .. });
        if !ran || probe.bytes != b"relay-probe\n" {
            return Err(format!(
                "the relay agent's probe command failed: {:?}",
                probe.outcome
            ));
        }
        let bytes: Vec<u8> = (0..64 * 1024).map(|_| rand::random::<u8>()).collect();
        let written = run(
            Op::Write {
                path: PROBE_FILE.into(),
                name: String::new(),
                mode: Some(0o600),
            },
            bytes.clone(),
        )?;
        if !matches!(written.outcome, Outcome::Written(_)) {
            return Err(format!(
                "the relay agent's probe write failed: {:?}",
                written.outcome
            ));
        }
        let read = run(
            Op::Read {
                path: PROBE_FILE.into(),
            },
            Vec::new(),
        )?;
        if read.bytes != bytes || !read.end_matches() {
            return Err("the relay agent's probe transfer did not round-trip".into());
        }
        let _ = run(
            Op::Exec {
                command: format!("rm -f \"$HOME/{PROBE_FILE}\""),
                stdin: false,
                timeout_s: 30.0,
            },
            Vec::new(),
        );
        Ok(json!({
            "os": agent.os,
            "arch": agent.arch,
            "build": agent.build,
            "broker_url": broker_url,
        }))
    }

    /// Run one call on a `relay` lease and map the broker's errors.
    fn relay_run(
        &self,
        vm: &str,
        target: &Target,
        execution: &str,
        op: Op,
        upload: Vec<u8>,
        timeout_s: u64,
    ) -> OpResult<Answer> {
        let relay = self
            .relay()
            .ok_or_else(|| OpError::new("the relay link is not running"))?;
        let bound =
            Duration::from_secs(timeout_s) + relay.settings.agent_wait + Duration::from_secs(60);
        let answer = relay.broker.run(target, execution, op, upload, bound);
        let released = self.release_flag(vm).load(Ordering::SeqCst);
        match answer {
            Ok(answer) => {
                let revoked = matches!(&answer.outcome, Outcome::Failed { code, .. } if code == "revoked");
                if revoked || released {
                    self.log(&format!("{vm}: running relay operation cancelled by release"));
                    return Err(OpError::new(format!(
                        "command cancelled by release of {vm}"
                    )));
                }
                Ok(answer)
            }
            Err(RunError::Timeout) => Err(OpError::uncertain(format!(
                "relay: no outcome for execution {execution} within {}s; it may still run, and its record answers later",
                bound.as_secs()
            ))),
            Err(RunError::Internal) => Err(OpError::uncertain(format!(
                "relay: the broker lost execution {execution}"
            ))),
        }
    }

    /// `exec` on a `relay` lease: a command line or a script through the
    /// relay agent in the desktop session.
    pub(crate) fn relay_exec(
        &self,
        vm: &str,
        target: &Target,
        body: &Value,
        command: Option<String>,
        script: Option<Vec<u8>>,
        timeout_s: u64,
    ) -> OpResult<Value> {
        let execution = execution_of(body)?;
        let (op, upload) = match (command, script) {
            (_, Some(script)) => (
                Op::Script {
                    timeout_s: timeout_s as f64,
                },
                script,
            ),
            (Some(command), None) => (
                Op::Exec {
                    command,
                    stdin: false,
                    timeout_s: timeout_s as f64,
                },
                Vec::new(),
            ),
            (None, None) => return Err(OpError::new("need 'argv' or 'script'")),
        };
        let answer = self.relay_run(vm, target, &execution, op, upload, timeout_s)?;
        exit_answer(vm, &execution, &answer, timeout_s)
    }

    /// `push` of one file on a `relay` lease. `None` for a folder, which
    /// still goes by `scp -r`.
    pub(crate) fn relay_push(
        &self,
        vm: &str,
        target: &Target,
        body: &Value,
        local: &Path,
        remote: &str,
    ) -> OpResult<Option<Value>> {
        if local.is_dir() {
            return Ok(None);
        }
        use std::os::unix::fs::PermissionsExt;
        let bytes = std::fs::read(local)
            .map_err(|e| OpError::new(format!("cannot read {}: {e}", local.display())))?;
        let mode = std::fs::metadata(local)
            .map(|m| m.permissions().mode() & 0o777)
            .ok();
        let name = local
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let execution = execution_of(body)?;
        let op = Op::Write {
            path: remote.to_string(),
            name,
            mode,
        };
        let answer = self.relay_run(vm, target, &execution, op, bytes, 300)?;
        match &answer.outcome {
            Outcome::Written(digest) => Ok(Some(json!({
                "vm": vm, "pushed": local.display().to_string(), "to": remote,
                "bytes": digest.size, "sha256": digest.sha256,
                "link": LINK_RELAY, "execution_id": execution,
            }))),
            other => Err(outcome_error(other)),
        }
    }

    /// `pull` of one file on a `relay` lease. `None` for a guest folder,
    /// which still goes by `scp -r`.
    pub(crate) fn relay_pull(
        &self,
        vm: &str,
        target: &Target,
        body: &Value,
        remote: &str,
        local: &Path,
    ) -> OpResult<Option<Value>> {
        let execution = execution_of(body)?;
        let op = Op::Read {
            path: remote.to_string(),
        };
        let answer = self.relay_run(vm, target, &execution, op, Vec::new(), 300)?;
        match &answer.outcome {
            Outcome::Read(digest) => {
                if !answer.end_matches() || digest.size != answer.bytes.len() as u64 {
                    return Err(OpError::new(
                        "pull failed: the bytes do not match their SHA-256; pull again",
                    ));
                }
                let parent = local.parent().unwrap_or_else(|| Path::new("."));
                let mut file = tempfile::NamedTempFile::new_in(parent)
                    .map_err(|e| OpError::new(format!("pull failed: {e}")))?;
                std::io::Write::write_all(&mut file, &answer.bytes)
                    .map_err(|e| OpError::new(format!("pull failed: {e}")))?;
                file.persist(local)
                    .map_err(|e| OpError::new(format!("pull failed: {e}")))?;
                Ok(Some(json!({
                    "vm": vm, "pulled": remote, "to": local.display().to_string(),
                    "bytes": digest.size, "sha256": digest.sha256,
                    "link": LINK_RELAY, "execution_id": execution,
                })))
            }
            Outcome::Failed { code, .. } if code == "is_directory" => Ok(None),
            other => Err(outcome_error(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_commands_are_marked_and_atomic() {
        for (command, step) in [
            (inspect_command(), "inspect"),
            (upload_command(), "upload"),
            (config_command(), "config"),
            (start_command(), "start"),
        ] {
            assert!(
                command.ends_with(&format!("{COMMAND_MARKER}-{step}")),
                "{command}"
            );
        }
        assert!(config_command().contains("chmod 600"));
        assert!(config_command().contains("mv -f"));
        assert!(upload_command().contains(GUEST_EXECUTABLE));
    }

    #[test]
    fn the_start_scripts_target_the_desktop_session() {
        let mac = start_script("macos");
        assert!(mac.contains("<string>Aqua</string>"));
        assert!(mac.contains("launchctl bootstrap \"gui/$uid\""));
        assert!(mac.contains("launchctl kickstart -k"));
        assert!(mac.contains(LAUNCH_AGENT_LABEL));
        let linux = start_script("linux");
        assert!(linux.contains("WantedBy=graphical-session.target"));
        assert!(linux.contains("systemctl --user restart relay-agent.service"));
        assert!(linux.contains("systemctl --user start relay-agent.service"));
        assert!(linux.contains("SuccessExitStatus=143"));
        assert!(linux.contains("${XDG_RUNTIME_DIR:-/run/user/$uid}"));
        // Both parse as POSIX shell.
        for script in [mac, linux] {
            let status = std::process::Command::new("sh")
                .arg("-n")
                .stdin(std::process::Stdio::piped())
                .spawn()
                .and_then(|mut child| {
                    use std::io::Write;
                    child.stdin.take().unwrap().write_all(script.as_bytes())?;
                    child.wait()
                })
                .unwrap();
            assert!(status.success());
        }
    }

    #[test]
    fn the_configuration_holds_the_url_credential_and_authority() {
        let config: Value =
            serde_json::from_slice(&agent_config("wss://h:1/relay", "c", "PEM")).unwrap();
        assert_eq!(config["broker_url"], "wss://h:1/relay");
        assert_eq!(config["agent_credential"], "c");
        assert_eq!(config["broker_ca_pem"], "PEM");
    }

    #[test]
    fn the_guest_url_is_the_override_or_the_hosts_address_on_the_guests_subnet() {
        let mut settings = RelaySettings {
            session_port: 6241,
            relay_url: "wss://h:6241/relay".into(),
            guest_url: Some("wss://o:1/relay".into()),
            register_timeout: Duration::from_secs(1),
            agent_wait: Duration::from_secs(1),
        };
        assert_eq!(
            guest_url(&settings, "198.51.100.7").unwrap(),
            "wss://o:1/relay"
        );
        settings.guest_url = None;
        // No interface of a test host is on a documentation subnet.
        assert!(guest_url(&settings, "198.51.100.7").is_err());
        assert!(guest_url(&settings, "not-an-address").is_err());
        assert!(default_relay_url(6241).ends_with(":6241/relay"));
    }

    #[test]
    fn execution_ids_come_from_the_body_or_are_new() {
        assert_eq!(execution_of(&json!({"execution_id": "x"})).unwrap(), "x");
        assert!(execution_of(&json!({"execution_id": ""})).is_err());
        assert!(execution_of(&json!({"execution_id": 3})).is_err());
        let a = execution_of(&json!({})).unwrap();
        assert_ne!(a, execution_of(&json!({})).unwrap());
    }
}
