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
//! - A `relay` lease's exec, push and pull go through its relay agent; a
//!   folder travels as a tar stream (RP-13).
//! - Release and expiry revoke the lease's credentials at once (RP-10).
//! - A `relay` lease whose relay agent registers with another build than
//!   this instance holds gets this instance's build written over its
//!   executable through the relay, and the agent restarts onto it once no
//!   call is out (RA-6, [`Service::watch_relay_agents`]).
//! - Neither path downgrades the agent's signing (DC-232): a guest whose
//!   agent is signed with Developer ID is never given a build that is not.
//!   Bootstrap reads the guest's agent's signature with `/usr/bin/codesign`
//!   over SSH, and a long-lived lease takes it from the agent's hello
//!   (protocol 3) or, from an earlier agent, with `/usr/bin/codesign`
//!   through the relay ([`GUEST_CODESIGN`]).
//! - Every build sent comes from a release checked against the instance's
//!   trust, as the installer checks it ([`Relay::held_release`], DC-231):
//!   at startup and before every upload or replacement. A release that
//!   fails serves leases over SSH with the reason (RA-8), and `/health`
//!   shows it as `relay_agent_release`.

use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use acquisition_options::GuestNetwork;
use relay_agent::protocol::AgentInfo;
use relay_agent::protocol::{Op, Outcome};
use relay_broker::tls::Authority;
use relay_broker::{Answer, Broker, BrokerConfig, RestartError, Role, RunError, Target, Upload};
use remote_session::{Listening, ServerConfig, SessionServer};
use serde_json::{json, Value};
use service_layout::signing::{self, Signature, Signing};

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
/// The port of the broker URL written into a guest, on the host's address on
/// the guest's network, instead of the session port: a test rig's chain in
/// front of the broker listens there (TL-7).
pub const GUEST_PORT_VARIABLE: &str = "MACHINE_SERVICE_RELAY_GUEST_PORT";
/// Seconds a new clone's relay agent gets to register (RA-8).
pub const REGISTER_TIMEOUT_VARIABLE: &str = "MACHINE_SERVICE_RELAY_REGISTER_TIMEOUT";
/// Seconds a call waits for a relay agent that has no session (RP-8).
pub const AGENT_WAIT_VARIABLE: &str = "MACHINE_SERVICE_RELAY_AGENT_WAIT";
/// The relay agent release folder to use, instead of the one beside the
/// daemon; it has the release's own form (DC-140).
pub const RELEASE_VARIABLE: &str = "MACHINE_SERVICE_RELAY_AGENT_RELEASE";

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
    /// The session listener's address: every interface unless
    /// `MACHINE_SERVICE_SESSION_HOST` names one, which a second instance
    /// must (DC-244).
    pub session_host: String,
    /// The URL discovery advertises (BK-4).
    pub relay_url: String,
    /// The URL guests are given, when not derived from the guest's network.
    pub guest_url: Option<String>,
    /// The port of the URL derived from the guest's network, when not the
    /// session port.
    pub guest_port: Option<u16>,
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
    /// `MACHINE_SERVICE_SESSION_HOST` (every interface),
    /// `MACHINE_SERVICE_RELAY_URL`, `MACHINE_SERVICE_RELAY_GUEST_URL`,
    /// `MACHINE_SERVICE_RELAY_GUEST_PORT`,
    /// `MACHINE_SERVICE_RELAY_REGISTER_TIMEOUT` (120 s) and
    /// `MACHINE_SERVICE_RELAY_AGENT_WAIT` (600 s).
    pub fn from_environment() -> Result<RelaySettings, String> {
        let session_port = remote_session::session_port_from_env().map_err(|e| e.to_string())?;
        let session_host = std::env::var(second_instance::SESSION_HOST_VARIABLE)
            .ok()
            .map(|host| host.trim().to_string())
            .filter(|host| !host.is_empty())
            .unwrap_or_else(|| "0.0.0.0".into());
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
        let guest_port = match std::env::var(GUEST_PORT_VARIABLE) {
            Ok(value) if !value.trim().is_empty() => Some(
                value
                    .trim()
                    .parse::<u16>()
                    .ok()
                    .filter(|port| *port > 0)
                    .ok_or_else(|| format!("{GUEST_PORT_VARIABLE}={value:?} is not a port"))?,
            ),
            _ => None,
        };
        Ok(RelaySettings {
            session_port,
            session_host,
            relay_url,
            guest_url,
            guest_port,
            register_timeout: seconds(REGISTER_TIMEOUT_VARIABLE, 120)?,
            agent_wait: seconds(AGENT_WAIT_VARIABLE, 600)?,
        })
    }
}

/// The relay agent release this instance holds: `MACHINE_SERVICE_RELAY_AGENT_RELEASE`,
/// else the release folder beside the daemon (DC-140, IN-1).
pub fn release_dir() -> OpResult<PathBuf> {
    if let Some(path) = std::env::var_os(RELEASE_VARIABLE).filter(|p| !p.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    service_layout::release::beside_daemon()
        .ok_or_else(|| OpError::new("relay agent release unavailable"))
}

/// The relay agent release this instance holds, once its trust is checked
/// ([`held_release`]). It is the only source of the builds this service
/// ever sends or expects (DC-140).
#[derive(Debug, Clone)]
pub struct HeldRelease {
    trusted: crate::durable::relay_agent::Trusted,
}

impl HeldRelease {
    /// The signature of its macOS executable.
    pub fn signing(&self) -> &Signing {
        &self.trusted.signing
    }

    /// Its version.
    pub fn version(&self) -> &str {
        &self.trusted.release.version
    }

    /// The relay agent for a guest of `kind`: the executable of the
    /// release's archive for its platform, checked against the
    /// `SHA256SUMS` the trust was checked with.
    pub fn build(&self, kind: &str) -> Result<Vec<u8>, String> {
        self.trusted
            .release
            .executable(&service_layout::release::platform_for(kind))
    }
}

/// Check the relay agent release in `release_dir` that the instance on
/// `state_dir` holds against the instance's trust, with the operator's
/// `configured` team, exactly as the installer checks a release
/// ([`crate::durable::relay_agent::release_executable`], DC-231,
/// DC-236). A Developer ID team the record does not name yet is
/// recorded, as an installation records it (DC-237). A refusal
/// names every team by its fingerprint, because it reaches a lease's
/// `link_reason` and `/health` (DC-238).
pub fn check_release(
    release_dir: &Path,
    state_dir: &Path,
    configured: Option<String>,
) -> Result<HeldRelease, String> {
    let anchor = signing::Anchor::resolve(state_dir, configured)?;
    let trusted = crate::durable::relay_agent::release_executable(release_dir, &anchor)
        .map_err(|error| redacted(&error, &anchor, release_dir))?;
    if let Some(team) = anchor.to_record(&trusted.signing) {
        anchor.record(team).map_err(|error| {
            format!("the team that signed it could not be recorded as trusted: {error}")
        })?;
    }
    Ok(HeldRelease { trusted })
}

/// `error` with the anchor's teams and the release's signer shown by
/// their fingerprints.
fn redacted(error: &str, anchor: &signing::Anchor, release_dir: &Path) -> String {
    let signer = service_layout::release::Release::open(release_dir)
        .and_then(|release| release.executable(&service_layout::release::platform_for("macos")))
        .and_then(|bytes| build_signature(&bytes))
        .ok()
        .and_then(|signature| signature.team().map(str::to_string));
    let teams = [&anchor.configured, &anchor.recorded, &signer];
    signing::redact(error, teams.into_iter().flatten().map(String::as_str))
}

/// [`check_release`] for this process: the release [`release_dir`] names,
/// and the team [`signing::TEAM_VARIABLE`] names.
pub fn held_release(state_dir: &Path) -> Result<HeldRelease, String> {
    let release_dir = release_dir().map_err(|error| error.message)?;
    check_release(
        &release_dir,
        state_dir,
        crate::durable::relay_agent::configured_team()?,
    )
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
    /// Leases whose relay agent is being replaced now.
    replacing: Mutex<HashSet<String>>,
    /// The build each lease's replacement last failed to bring, so a guest
    /// that cannot take it is not tried again for that build.
    failed_builds: Mutex<HashMap<String, String>>,
    /// The instance's state folder, which holds its trust record.
    state_dir: PathBuf,
    /// What `/health` shows of the last check of the held release; held
    /// through each check, so two checks never record the trust at once.
    release_check: Mutex<Value>,
}

impl Relay {
    /// Make or load the authority under `<state>/relay/tls`, and start the
    /// broker on the session listener, under TLS: on every interface, where
    /// the guests reach it on their network's host address (BK-5), unless
    /// the settings name one address.
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
        let host = settings
            .session_host
            .trim_start_matches('[')
            .trim_end_matches(']');
        let listener = TcpListener::bind((host, settings.session_port)).map_err(|e| {
            format!(
                "relay: cannot listen on {host} port {} ({}): {e}",
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
        let relay = Arc::new(Relay {
            settings,
            broker,
            authority,
            listening: Mutex::new(Some(listening)),
            credentials: Mutex::new(HashMap::new()),
            replacing: Mutex::new(HashSet::new()),
            failed_builds: Mutex::new(HashMap::new()),
            state_dir: state_dir.to_path_buf(),
            release_check: Mutex::new(Value::Null),
        });
        // DC-236: the held release is checked at startup, whatever
        // else this instance offers.
        match relay.held_release() {
            Ok(held) => log(&format!(
                "relay: the relay agent release {} is trusted: signed {}",
                held.version(),
                held.signing().kind()
            )),
            Err(reason) => log(&format!(
                "WARN: relay: the relay agent release is not trusted: {reason}; leases are served over SSH until it is (RA-8)"
            )),
        }
        Ok(relay)
    }

    /// Check the relay agent release this instance holds against its trust
    /// ([`held_release`]): at startup, and before every upload or
    /// replacement of a guest's relay agent, so the release in place when
    /// a build is sent is the one checked (DC-236). A release
    /// that fails serves leases over SSH with the reason (RA-8).
    pub fn held_release(&self) -> Result<HeldRelease, String> {
        let mut shown = self.release_check.lock().unwrap_or_else(|e| e.into_inner());
        let held = held_release(&self.state_dir);
        *shown = match &held {
            Ok(held) => json!({
                "trusted": true,
                "version": held.version(),
                "signing": held.signing().kind(),
            }),
            Err(reason) => json!({"trusted": false, "reason": reason}),
        };
        held
    }

    /// What `/health` shows of the held release as `relay_agent_release`:
    /// the outcome of its last check, `null` before any.
    pub fn release_health(&self) -> Value {
        self.release_check
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
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

/// The per-machine agent credential of a durable Mac's relay agent, under
/// the state folder (RA-4, DC-152).
pub const DURABLE_AGENT_CREDENTIAL: &str = "relay/durable-agent.credential";
/// The broker's authority under the state folder (BK-1).
pub const AUTHORITY_DIR: &str = "relay/tls";
/// The relay agent's configuration under its home (DC-126).
pub const AGENT_CONFIG: &str = ".config/relay-agent/config.json";

/// The broker URL a durable Mac's own relay agent dials: the loopback name
/// and the session port, so neither a rename nor a network change moves it.
pub fn durable_broker_url(session_port: u16) -> String {
    format!(
        "wss://localhost:{session_port}{}",
        remote_session::RELAY_PATH
    )
}

/// The durable machine's agent credential: read it, or make it mode 600;
/// with `rotate`, always make a new one (RA-4).
pub fn durable_agent_credential(state_dir: &Path, rotate: bool) -> Result<String, String> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let path = state_dir.join(DURABLE_AGENT_CREDENTIAL);
    if !rotate {
        if let Ok(text) = std::fs::read_to_string(&path) {
            let credential = text.trim().to_string();
            if !credential.is_empty() {
                return Ok(credential);
            }
        }
    }
    let parent = path.parent().unwrap_or(state_dir);
    std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    let credential = relay_broker::new_credential();
    let mut file = tempfile::Builder::new()
        .prefix(".durable-agent.credential.")
        .tempfile_in(parent)
        .map_err(|e| format!("{}: {e}", parent.display()))?;
    file.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    writeln!(file, "{credential}").map_err(|e| format!("{}: {e}", path.display()))?;
    file.persist(&path)
        .map_err(|e| format!("{}: {}", path.display(), e.error))?;
    Ok(credential)
}

/// The broker's authority, made on first use (BK-1, DC-143).
pub fn authority(state_dir: &Path) -> Result<Authority, String> {
    Authority::load_or_create(&state_dir.join(AUTHORITY_DIR))
}

/// The relay agent's configuration file.
pub fn agent_config(broker_url: &str, credential: &str, ca_pem: &str) -> Vec<u8> {
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

/// What [`signature_command`] prints where the guest has no `codesign`.
const NO_CODESIGN: &str = "machine-service-relay: no codesign";

/// The `codesign` a guest's signature check runs: macOS's only system
/// `codesign`, never one found on the guest's `PATH`, which the guest's
/// account controls (DC-239).
pub const GUEST_CODESIGN: &str = "/usr/bin/codesign";

/// Print what [`GUEST_CODESIGN`] shows of the guest's relay agent's
/// signature (DC-232), or [`NO_CODESIGN`] where the guest has none.
pub fn signature_command() -> String {
    sh(
        &format!(
            "p=\"$HOME/{GUEST_EXECUTABLE}\"; if [ -x {GUEST_CODESIGN} ]; then \
{GUEST_CODESIGN} -d -r- \"$p\" 2>&1; else echo '{NO_CODESIGN}'; fi; exit 0"
        ),
        "signature",
    )
}

/// The command a relay agent of an earlier protocol on macOS runs to show
/// its own executable's signature with [`GUEST_CODESIGN`] (DC-232).
pub fn relay_signature_command() -> String {
    format!("{GUEST_CODESIGN} -d -r- \"$HOME/{GUEST_EXECUTABLE}\" 2>&1")
}

/// The signature of a relay agent in place from what [`signature_command`]
/// printed on a guest of `kind`. A guest without `codesign` has no signed
/// agent, except a macOS guest, whose signature then cannot be told.
fn guest_signature(kind: &str, output: &str) -> Result<Signature, String> {
    if !output.contains(NO_CODESIGN) {
        return Ok(Signature::from_codesign(output));
    }
    if kind == "macos" {
        return Err(
            "the macOS guest has no codesign, so the signing of its relay agent cannot be told (DC-232)"
                .into(),
        );
    }
    Ok(Signature::Unsigned)
}

/// The signature of `build`, a relay agent executable this instance holds,
/// read with the host's `codesign`: a Linux build is unsigned.
pub fn build_signature(build: &[u8]) -> Result<Signature, String> {
    let directory = tempfile::tempdir().map_err(|e| e.to_string())?;
    let path = directory.path().join(service_layout::release::MEMBER);
    std::fs::write(&path, build).map_err(|e| e.to_string())?;
    signing::inspect(&path)
}

/// Why replacing a relay agent signed `present` with `build` would
/// downgrade its signing, if it would (DC-232).
fn downgrade_of(present: &Signature, build: &[u8]) -> Result<Option<String>, String> {
    if !matches!(present, Signature::DeveloperId { .. }) {
        return Ok(None);
    }
    Ok(signing::downgrade(present, &build_signature(build)?))
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
until /bin/launchctl print "gui/$uid" >/dev/null 2>&1; do
  i=$((i + 1))
  if [ "$i" -gt 120 ]; then echo "relay-start: no desktop session (gui/$uid) after 120 s"; exit 3; fi
  sleep 1
done
if /bin/launchctl print "gui/$uid/$label" >/dev/null 2>&1; then
  /bin/launchctl kickstart -k "gui/$uid/$label"
else
  /bin/launchctl bootstrap "gui/$uid" "$plist"
fi
/bin/launchctl print "gui/$uid/$label" | grep -E '^[[:space:]]*(state|pid) = ' | sed 's/^[[:space:]]*/relay-start: /'
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
    guest_url_toward(settings, guest_ip, relay_broker::net::address_toward)
}

fn guest_url_toward(
    settings: &RelaySettings,
    guest_ip: &str,
    address_toward: impl Fn(Ipv4Addr) -> Option<Ipv4Addr>,
) -> Result<String, String> {
    if let Some(url) = &settings.guest_url {
        return Ok(url.clone());
    }
    let ip: Ipv4Addr = guest_ip
        .parse()
        .map_err(|_| format!("the guest's address {guest_ip} is not IPv4"))?;
    let host = address_toward(ip).ok_or_else(|| {
        format!("this host has no address on the subnet of the guest's {guest_ip}")
    })?;
    Ok(format!(
        "wss://{host}:{}{}",
        settings.guest_port.unwrap_or(settings.session_port),
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

/// The end of a virtual machine lease's relay credentials, from its record
/// (DC-214); `None` for a record without a TTL.
fn credential_until(record: &Value) -> Option<f64> {
    let ttl = record.get("ttl_expires_at").and_then(Value::as_f64)?;
    Some(crate::ops::relay_credential_until(
        ttl,
        crate::ops::lease_initial_ttl_hours(record),
        crate::ops::grace_hours(),
    ))
}

/// An execution ID for an HTTP request: the body's `execution_id`, else a
/// new one.
pub(crate) fn execution_of(body: &Value) -> OpResult<String> {
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
                        relay
                            .broker
                            .credentials_until(&target, role, credential_until(record));
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

    /// RA-6 for leases that live on: whenever a `relay` lease's relay agent
    /// registers, such as after this instance restarted with a new build,
    /// compare its build with the one this instance holds and replace it
    /// when they differ ([`Service::replace_relay_agent`]).
    pub fn watch_relay_agents(self: &Arc<Self>) {
        let Some(relay) = self.relay() else {
            return;
        };
        let service = Arc::downgrade(self);
        relay.broker.on_agent(move |target, info| {
            let Target::Lease(vm) = target else {
                // A durable machine's agent is the installer's (RA-6).
                return;
            };
            let Some(service) = service.upgrade() else {
                return;
            };
            let (vm, info) = (vm.clone(), info.clone());
            std::thread::spawn(move || service.replace_relay_agent(&vm, &info));
        });
    }

    /// Replace the relay agent of `vm` when `agent` runs another build than
    /// this instance holds: write the build over the agent's executable
    /// through the relay, then restart the agent onto it once no call of
    /// the lease is out, holding new calls for its next session. No
    /// execution is lost: none runs at the restart, and the agent's records
    /// outlive it.
    pub(crate) fn replace_relay_agent(&self, vm: &str, agent: &AgentInfo) {
        let Some(relay) = self.relay() else {
            return;
        };
        let record = match self.state.read() {
            Ok(data) => data.get("vms").and_then(|vms| vms.get(vm)).cloned(),
            Err(_) => None,
        };
        // A lease still bootstrapping registers its agent before its record
        // says `relay`; bootstrap has already put this build in place.
        let Some(record) =
            record.filter(|r| r.get("link").and_then(Value::as_str) == Some(LINK_RELAY))
        else {
            return;
        };
        let kind = crate::ops::record_kind(&record).to_string();
        // DC-236: the release is checked against the instance's
        // trust before its build is compared, let alone written.
        let build = match relay.held_release().and_then(|held| held.build(&kind)) {
            Ok(build) => build,
            Err(error) => {
                self.log(&format!(
                    "WARN: relay: {vm}: the relay agent is not replaced: the relay agent release is not trusted or has no build for it: {error}"
                ));
                return;
            }
        };
        let want = sha256_hex(&build);
        if agent.build == want {
            return;
        }
        let failed = relay
            .failed_builds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(vm)
            == Some(&want);
        let first = relay
            .replacing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(vm.to_string());
        if failed || !first {
            return;
        }
        let outcome = self.replace_relay_agent_steps(relay, vm, &want, build, agent);
        relay
            .replacing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(vm);
        match outcome {
            Ok(()) => {}
            Err(reason) => {
                self.log(&format!(
                    "WARN: relay: {vm}: the relay agent was not replaced: {reason}; it keeps build {}",
                    agent.build.get(..12).unwrap_or(&agent.build)
                ));
                relay
                    .failed_builds
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(vm.to_string(), want);
            }
        }
    }

    /// The signature of the relay agent `agent` of `target`: from its hello,
    /// or, from an agent of an earlier protocol on macOS, what `codesign`
    /// shows of its executable through the relay (DC-232).
    fn agent_signature(
        &self,
        relay: &Relay,
        target: &Target,
        agent: &AgentInfo,
    ) -> Result<Signature, String> {
        if let Some(signing) = &agent.signing {
            return Ok(Signature::from_parts(
                &signing.kind,
                signing.team.as_deref(),
            ));
        }
        if agent.os != "macos" {
            return Ok(Signature::Unsigned);
        }
        let shown = relay
            .broker
            .run(
                target,
                &uuid::Uuid::new_v4().to_string(),
                Op::Exec {
                    command: relay_signature_command(),
                    stdin: false,
                    timeout_s: 60.0,
                },
                Upload::bytes(Vec::new()),
                relay.settings.agent_wait + Duration::from_secs(60),
            )
            .map_err(|e| format!("reading the relay agent's signature got no answer: {e:?}"))?;
        Ok(Signature::from_codesign(&String::from_utf8_lossy(
            &shown.bytes,
        )))
    }

    fn replace_relay_agent_steps(
        &self,
        relay: &Relay,
        vm: &str,
        want: &str,
        build: Vec<u8>,
        agent: &AgentInfo,
    ) -> Result<(), String> {
        let have = agent.build.as_str();
        let target = Target::Lease(vm.to_string());
        // DC-232: a Developer ID agent is never replaced by a build that is
        // not; the agent says how it is signed in its hello (protocol 3).
        let present = self.agent_signature(relay, &target, agent)?;
        if let Some(reason) = downgrade_of(&present, &build)? {
            return Err(reason);
        }
        self.log(&format!(
            "relay: {vm}: the relay agent runs build {}, not this instance's {}; replacing it (RA-6)",
            have.get(..12).unwrap_or(have),
            &want[..12]
        ));
        let written = relay
            .broker
            .run(
                &target,
                &uuid::Uuid::new_v4().to_string(),
                Op::Write {
                    path: GUEST_EXECUTABLE.into(),
                    name: String::new(),
                    mode: Some(0o755),
                },
                Upload::bytes(build),
                relay.settings.agent_wait + Duration::from_secs(300),
            )
            .map_err(|e| format!("writing the build got no answer: {e:?}"))?;
        match &written.outcome {
            Outcome::Written(digest) if digest.sha256 == want => {}
            other => return Err(format!("writing the build failed: {other:?}")),
        }
        // The agent refuses a restart while anything runs, including calls
        // a restarted broker does not know of; wait for it to be idle.
        let restarted = loop {
            if !relay.broker.has_credential(&target, Role::Agent) {
                return Err("the lease ended".into());
            }
            match relay.broker.restart_agent(
                &target,
                Duration::from_secs(3600),
                relay.settings.register_timeout,
            ) {
                Ok(restarted) => break restarted,
                Err(RestartError::Busy) => std::thread::sleep(Duration::from_secs(2)),
                Err(other) => return Err(format!("the restart failed: {other:?}")),
            }
        };
        if restarted.build != want {
            return Err(format!(
                "after the restart it runs build {}",
                restarted.build.get(..12).unwrap_or(&restarted.build)
            ));
        }
        let _ = self.state.update(
            |data| {
                if let Some(record) = data.get_mut("vms").and_then(|vms| vms.get_mut(vm)) {
                    if let Some(object) =
                        record.get_mut("relay_agent").and_then(Value::as_object_mut)
                    {
                        object.insert("build".into(), json!(want));
                        match &restarted.signing {
                            Some(signing) => {
                                object.insert("signing".into(), json!(signing.kind));
                            }
                            None => {
                                object.remove("signing");
                            }
                        }
                    }
                }
                Ok(())
            },
            None::<fn(&serde_json::Map<String, Value>)>,
        );
        self.log(&format!(
            "relay: {vm}: relay agent replaced; it runs build {} (RA-6)",
            &want[..12]
        ));
        Ok(())
    }

    /// Set the end of the relay credentials of `vm` from its record: the end
    /// of its TTL plus its grace bounded by its TTL (RP-3,
    /// [`crate::ops::relay_credential_until`]). Called when the lease starts
    /// running and when it is renewed.
    pub(crate) fn relay_credentials_until(&self, vm: &str) {
        let Some(relay) = self.relay() else {
            return;
        };
        let Ok(record) = self.get_record(vm) else {
            return;
        };
        if record.get("link").and_then(Value::as_str) != Some(LINK_RELAY) {
            return;
        }
        let target = Target::Lease(vm.to_string());
        let until = credential_until(&record);
        for role in [Role::Agent, Role::Client] {
            relay.broker.credentials_until(&target, role, until);
        }
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

        // The build: the image's when it is the release's, else the
        // release's executable, uploaded (RA-6, DC-140), from a release
        // checked against the instance's trust now (DC-236).
        let build = relay
            .held_release()
            .map_err(|e| format!("the relay agent release is not trusted: {e}"))?
            .build(kind)
            .map_err(|e| format!("no relay agent to send: {e}"))?;
        let want = sha256_hex(&build);
        let (rc, have) = ssh(&inspect_command(), None, 60)?;
        let have = have.trim().lines().last().unwrap_or("").trim().to_string();
        if rc != 0 {
            return Err(format!(
                "inspecting the guest's relay agent failed ({rc}): {have}"
            ));
        }
        if have != want && have != "none" {
            // DC-232: a Developer ID agent is never replaced by a build that
            // is not; the lease is served over SSH instead (RA-8).
            let (rc, shown) = ssh(&signature_command(), None, 60)?;
            if rc != 0 {
                return Err(format!(
                    "reading the guest's relay agent signature failed ({rc}): {}",
                    shown.trim()
                ));
            }
            let present = guest_signature(kind, &shown)?;
            if let Some(reason) = downgrade_of(&present, &build)? {
                return Err(format!("the guest's relay agent is not replaced: {reason}"));
            }
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
                    Upload::bytes(upload),
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
        let mut fields = json!({
            "os": agent.os,
            "arch": agent.arch,
            "build": agent.build,
            "broker_url": broker_url,
        });
        if let Some(signing) = &agent.signing {
            fields["signing"] = json!(signing.kind);
        }
        Ok(fields)
    }

    /// Run one call on a `relay` lease and map the broker's errors.
    fn relay_run(
        &self,
        vm: &str,
        target: &Target,
        execution: &str,
        op: Op,
        upload: Upload,
        timeout_s: u64,
    ) -> OpResult<Answer> {
        let mut bytes = Vec::new();
        let mut answer =
            self.relay_run_into(vm, target, execution, op, upload, timeout_s, |data| {
                bytes.extend_from_slice(data);
                Ok(())
            })?;
        answer.bytes = bytes;
        Ok(answer)
    }

    /// [`Self::relay_run`], handing the answer's stream to `sink` as it
    /// comes, so a transfer of any size streams through (RP-15).
    #[allow(clippy::too_many_arguments)]
    fn relay_run_into(
        &self,
        vm: &str,
        target: &Target,
        execution: &str,
        op: Op,
        upload: Upload,
        timeout_s: u64,
        mut sink: impl FnMut(&[u8]) -> std::io::Result<()>,
    ) -> OpResult<Answer> {
        let relay = self
            .relay()
            .ok_or_else(|| OpError::new("the relay link is not running"))?;
        let bound =
            Duration::from_secs(timeout_s) + relay.settings.agent_wait + Duration::from_secs(60);
        let answer = relay
            .broker
            .run_into(target, execution, op, upload, bound, &mut sink);
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
            Err(RunError::Sink(error)) => Err(OpError::new(format!(
                "relay: the transfer could not be stored: {error}"
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
                Upload::bytes(script),
            ),
            (Some(command), None) => (
                Op::Exec {
                    command,
                    stdin: false,
                    timeout_s: timeout_s as f64,
                },
                Upload::None,
            ),
            (None, None) => return Err(OpError::new("need 'argv' or 'script'")),
        };
        let answer = self.relay_run(vm, target, &execution, op, upload, timeout_s)?;
        exit_answer(vm, &execution, &answer, timeout_s)
    }

    /// `push` on a `relay` lease: a file as `write`, a folder as
    /// `write_tree` with its tar stream (RP-13), placed as `scp -r` places
    /// it and replaced whole in one step.
    pub(crate) fn relay_push(
        &self,
        vm: &str,
        target: &Target,
        body: &Value,
        local: &Path,
        remote: &str,
    ) -> OpResult<Option<Value>> {
        if local.is_dir() {
            return self
                .relay_push_tree(vm, target, body, local, remote)
                .map(Some);
        }
        use std::os::unix::fs::PermissionsExt;
        // The file streams from the disk, from any offset (RP-15).
        let upload = Upload::file(local)
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
        let timeout = transfer_timeout(upload.digest().size);
        let answer = self.relay_run(vm, target, &execution, op, upload, timeout)?;
        match &answer.outcome {
            Outcome::Written(digest) => Ok(Some(json!({
                "vm": vm, "pushed": local.display().to_string(), "to": remote,
                "bytes": digest.size, "sha256": digest.sha256,
                "link": LINK_RELAY, "execution_id": execution,
            }))),
            other => Err(outcome_error(other)),
        }
    }

    fn relay_push_tree(
        &self,
        vm: &str,
        target: &Target,
        body: &Value,
        local: &Path,
        remote: &str,
    ) -> OpResult<Value> {
        // The tar stream is spooled to a temporary file and streams from
        // there, so a folder of any size needs no memory of its size.
        let spool = tempfile::NamedTempFile::new()
            .map_err(|e| OpError::new(format!("push failed: {e}")))?;
        byte_transfer::pack_into(local, std::io::BufWriter::new(spool.as_file()))
            .and_then(|mut writer| std::io::Write::flush(&mut writer).map_err(|e| e.to_string()))
            .map_err(|e| OpError::new(format!("push failed: {e}")))?;
        let stream =
            Upload::file(spool.path()).map_err(|e| OpError::new(format!("push failed: {e}")))?;
        let name = local
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let execution = execution_of(body)?;
        let op = Op::WriteTree {
            path: remote.to_string(),
            name,
        };
        let timeout = transfer_timeout(stream.digest().size);
        let answer = self.relay_run(vm, target, &execution, op, stream, timeout)?;
        drop(spool);
        match &answer.outcome {
            Outcome::Written(digest) => Ok(json!({
                "vm": vm, "pushed": local.display().to_string(), "to": remote,
                "folder": true, "bytes": digest.size, "sha256": digest.sha256,
                "link": LINK_RELAY, "execution_id": execution,
            })),
            other => Err(outcome_error(other)),
        }
    }

    /// `pull` on a `relay` lease: a file as `read`; a guest folder, which
    /// the read answers with `is_directory`, as `read_tree` (RP-13), placed
    /// as `scp -r` places it and replaced whole in one step.
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
        // The bytes stream to a temporary sibling and are renamed into
        // place once they match.
        let parent = local
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut file = tempfile::NamedTempFile::new_in(parent)
            .map_err(|e| OpError::new(format!("pull failed: {e}")))?;
        let answer = {
            let mut writer = std::io::BufWriter::new(file.as_file_mut());
            let answer =
                self.relay_run_into(vm, target, &execution, op, Upload::None, 300, |data| {
                    std::io::Write::write_all(&mut writer, data)
                })?;
            std::io::Write::flush(&mut writer)
                .map_err(|e| OpError::new(format!("pull failed: {e}")))?;
            answer
        };
        match &answer.outcome {
            Outcome::Read(digest) => {
                if !answer.end_matches() || digest.size != answer.stream.size {
                    return Err(OpError::new(
                        "pull failed: the bytes do not match their SHA-256; pull again",
                    ));
                }
                file.persist(local)
                    .map_err(|e| OpError::new(format!("pull failed: {e}")))?;
                Ok(Some(json!({
                    "vm": vm, "pulled": remote, "to": local.display().to_string(),
                    "bytes": digest.size, "sha256": digest.sha256,
                    "link": LINK_RELAY, "execution_id": execution,
                })))
            }
            Outcome::Failed { code, .. } if code == "is_directory" => self
                .relay_pull_tree(vm, target, &execution, remote, local)
                .map(Some),
            other => Err(outcome_error(other)),
        }
    }

    fn relay_pull_tree(
        &self,
        vm: &str,
        target: &Target,
        read_execution: &str,
        remote: &str,
        local: &Path,
    ) -> OpResult<Value> {
        // The read's ID already names the read that found a folder; the
        // folder read takes an ID derived from it, so a repeated request
        // derives the same one.
        let execution = format!("tree:{}", sha256_hex(read_execution.as_bytes()));
        let op = Op::ReadTree {
            path: remote.to_string(),
        };
        let name = Path::new(remote.trim_end_matches('/'))
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let destination = byte_transfer::tree_destination(local, &name)
            .map_err(|e| OpError::new(format!("pull failed: {e}")))?;
        let parent = destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let scratch = tempfile::Builder::new()
            .prefix(".machine-service-pull-")
            .tempdir_in(parent)
            .map_err(|e| OpError::new(format!("pull failed: {e}")))?;
        let archive = scratch.path().join("tree.tar");
        let answer = {
            let file = std::fs::File::create(&archive)
                .map_err(|e| OpError::new(format!("pull failed: {e}")))?;
            let mut writer = std::io::BufWriter::new(file);
            let answer =
                self.relay_run_into(vm, target, &execution, op, Upload::None, 300, |data| {
                    std::io::Write::write_all(&mut writer, data)
                })?;
            std::io::Write::flush(&mut writer)
                .map_err(|e| OpError::new(format!("pull failed: {e}")))?;
            answer
        };
        let digest = match &answer.outcome {
            Outcome::Read(digest) => digest.clone(),
            other => return Err(outcome_error(other)),
        };
        if !answer.end_matches() || digest.size != answer.stream.size {
            return Err(OpError::new(
                "pull failed: the folder's stream does not match its SHA-256; pull again",
            ));
        }
        place_pulled_tree(scratch.path(), &archive, &destination)
            .map_err(|e| OpError::new(format!("pull failed: {e}")))?;
        Ok(json!({
            "vm": vm, "pulled": remote, "to": destination.display().to_string(),
            "folder": true, "bytes": digest.size, "sha256": digest.sha256,
            "link": LINK_RELAY, "execution_id": execution,
        }))
    }
}

/// How long a transfer of `size` bytes may take: five minutes, and a
/// second for every MiB beyond, so a slow link still carries a large file.
fn transfer_timeout(size: u64) -> u64 {
    300 + (size >> 20)
}

/// Unpack a folder's tar stream at `archive`, in `scratch` beside
/// `destination`, and put it there in one step, replacing a folder that is
/// there whole.
fn place_pulled_tree(scratch: &Path, archive: &Path, destination: &Path) -> Result<(), String> {
    let staged = scratch.join("tree");
    byte_transfer::unpack(archive, &staged)?;
    // The staged folder sits one level below the destination's folder; a
    // rename within one file system keeps the placement atomic.
    byte_transfer::place_tree(&staged, destination).map_err(|e| e.to_string())
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
            (signature_command(), "signature"),
        ] {
            assert!(
                command.ends_with(&format!("{COMMAND_MARKER}-{step}")),
                "{command}"
            );
        }
        assert!(config_command().contains("chmod 600"));
        assert!(config_command().contains("mv -f"));
        assert!(upload_command().contains(GUEST_EXECUTABLE));
        assert!(signature_command().contains(GUEST_EXECUTABLE));
    }

    /// DC-239: a guest's signature check runs `/usr/bin/codesign`;
    /// a `codesign` that the guest's `PATH` finds first, which would claim
    /// any signature, is never run, at bootstrap or through the relay.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_guests_signature_check_never_runs_a_codesign_from_its_path() {
        let guest = tempfile::tempdir().unwrap();
        let bin = guest.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fake = bin.join("codesign");
        std::fs::write(
            &fake,
            "#!/bin/sh\necho 'designated => identifier \"com.wezzard.relay-agent\" and anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6] /* exists */ and certificate leaf[field.1.2.840.113635.100.6.1.13] /* exists */ and certificate leaf[subject.OU] = EXAMPLE123'\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let agent = guest.path().join(GUEST_EXECUTABLE);
        std::fs::create_dir_all(agent.parent().unwrap()).unwrap();
        std::fs::write(&agent, "an unsigned build\n").unwrap();
        for command in [signature_command(), relay_signature_command()] {
            assert!(command.contains(GUEST_CODESIGN), "{command}");
            let output = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(&command)
                .env("HOME", guest.path())
                .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
                .output()
                .unwrap();
            let shown = String::from_utf8_lossy(&output.stdout);
            assert_eq!(
                guest_signature("macos", &shown).unwrap(),
                Signature::Unsigned,
                "{command}: {shown}"
            );
        }
    }

    /// DC-232: the guest's agent's signature is read from `codesign`; a
    /// guest without it has no signed agent, except a macOS guest, whose
    /// signing then cannot be told. Only a Developer ID agent in place is
    /// protected, and the host's ad hoc build would downgrade it.
    #[test]
    fn the_guests_signature_decides_whether_a_build_downgrades_it() {
        let developer = "Executable=/x\ndesignated => identifier \"com.wezzard.relay-agent\" and anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6] /* exists */ and certificate leaf[field.1.2.840.113635.100.6.1.13] /* exists */ and certificate leaf[subject.OU] = EXAMPLE123\n";
        assert_eq!(
            guest_signature("macos", developer).unwrap(),
            Signature::DeveloperId {
                team: "EXAMPLE123".into()
            }
        );
        assert_eq!(
            guest_signature("linux", NO_CODESIGN).unwrap(),
            Signature::Unsigned
        );
        assert!(guest_signature("macos", NO_CODESIGN)
            .unwrap_err()
            .contains("DC-232"));
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("agent");
        std::fs::copy("/usr/bin/true", &path).unwrap();
        assert!(std::process::Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", "-"])
            .arg(&path)
            .status()
            .unwrap()
            .success());
        let adhoc = std::fs::read(&path).unwrap();
        assert_eq!(build_signature(&adhoc).unwrap(), Signature::AdHoc);
        assert_eq!(
            build_signature(b"#!/bin/sh\n").unwrap(),
            Signature::Unsigned
        );
        let present = guest_signature("macos", developer).unwrap();
        assert!(downgrade_of(&present, &adhoc)
            .unwrap()
            .unwrap()
            .contains("DC-232"));
        assert_eq!(downgrade_of(&Signature::AdHoc, &adhoc).unwrap(), None);
        assert_eq!(downgrade_of(&Signature::Unsigned, &adhoc).unwrap(), None);
    }

    #[test]
    fn the_start_scripts_target_the_desktop_session() {
        let mac = start_script("macos");
        assert!(mac.contains("<string>Aqua</string>"));
        assert!(mac.contains("/bin/launchctl bootstrap \"gui/$uid\""));
        assert!(mac.contains("/bin/launchctl kickstart -k"));
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
            session_host: "0.0.0.0".into(),
            relay_url: "wss://h:6241/relay".into(),
            guest_url: Some("wss://o:1/relay".into()),
            guest_port: None,
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
        let host = |_| Some(Ipv4Addr::new(198, 51, 100, 1));
        assert_eq!(
            guest_url_toward(&settings, "198.51.100.7", host).unwrap(),
            "wss://198.51.100.1:6241/relay"
        );
        // A test rig's chain in front of the broker takes the guests' port.
        settings.guest_port = Some(6244);
        assert_eq!(
            guest_url_toward(&settings, "198.51.100.7", host).unwrap(),
            "wss://198.51.100.1:6244/relay"
        );
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
