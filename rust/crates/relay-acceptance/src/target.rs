//! The real target of a cell: a machine-service instance with the relay
//! link on, a lease on it, and the relay agent in the lease's desktop
//! session (`docs/arch/test-layers.md`, TL-7).
//!
//! The client finds the broker only through discovery: the instance's
//! advertised `relay_url` is the URL it dials, and the rig in front of the
//! broker answers for that URL's host name (AC-68). The relay agents of the
//! instance's guests dial the agent chain on the target's host, which the
//! suite starts with the cell's profile through [`HostControl`] before it
//! acquires the lease.

use std::net::{SocketAddr, ToSocketAddrs};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use network_rig::cell::{Cell, Endpoint, Stop, Target, TargetKind};
use network_rig::{ClientSide, RigError};
use serde_json::{json, Map, Value};

use crate::client::{ClientOptions, RelayClient};
use crate::guest::{shell_quote, Os};
use crate::instance::{url_host_port, Discovery, Instance};

/// How the suite reaches the target's host: a command that runs the host
/// script there, such as `ssh <host> <dir>/relay-acceptance-host.sh`. The
/// host names come from the operator at run time, never from the suite.
#[derive(Debug, Clone)]
pub struct HostControl {
    /// The command and its first arguments, such as `ssh -o BatchMode=yes h`.
    pub shell: Vec<String>,
    /// The host script's path there.
    pub script: String,
}

impl HostControl {
    /// Run the host script with `args`, and return its output.
    pub fn run(&self, args: &[&str]) -> Result<String, String> {
        let (program, first) = self.shell.split_first().ok_or("the host shell is empty")?;
        let line = std::iter::once(shell_quote(&self.script))
            .chain(args.iter().map(|a| shell_quote(a)))
            .collect::<Vec<_>>()
            .join(" ");
        let output = Command::new(program)
            .args(first)
            .arg(&line)
            .output()
            .map_err(|e| format!("{program}: {e}"))?;
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        if !output.status.success() {
            return Err(format!(
                "host `{}` failed ({}): {}{}",
                args.join(" "),
                output.status,
                text.trim(),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(text)
    }
}

/// What a cell's target is.
#[derive(Debug, Clone)]
pub struct TargetSettings {
    /// The instance's base URL, `http://<name>:<port>`.
    pub instance: String,
    /// The image of a virtual machine target.
    pub image: Option<String>,
    /// The profile the cell runs under, for the agent chain.
    pub profile: String,
    /// The seed of the cell's schedule; the agent chain plays the next one.
    pub seed: u64,
    /// The target's host, to run the agent chain and restart the broker.
    pub host: Option<HostControl>,
    /// Whether this run starts and stops the agent chain itself; when cells
    /// that share an instance run at once, the operator runs one chain for
    /// all of them.
    pub manage_agent_rig: bool,
    /// The lease's purpose.
    pub purpose: String,
}

/// A lease the suite holds.
pub struct Lease {
    /// The machine's name.
    pub name: String,
    /// The acquisition's answer, without its credentials.
    pub record: Value,
    /// The lease's client credential (`relay.credential`).
    pub credential: String,
    /// The broker URL the acquisition answered.
    pub url: String,
    /// The broker's authority, PEM.
    pub ca_pem: String,
    /// How long the acquisition took.
    pub acquired_in: Duration,
    /// The relay agent's credential, read from its configuration.
    pub agent_credential: Option<String>,
    /// The guest's system.
    pub os: Os,
    /// Which relay agent serves the lease.
    pub agent: AgentPlace,
}

/// Where a lease's relay agent is: its LaunchAgent label and the folder
/// its own files lie under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPlace {
    /// The LaunchAgent label (macOS).
    pub label: String,
    /// The agent's root, as a shell word: `~` in a guest, the path the
    /// instance reports on a durable Mac.
    pub root: String,
}

impl AgentPlace {
    /// A guest's agent: the image's label under the guest user's home
    /// (pilot-images RA-I1, RA-I2).
    pub fn guest() -> AgentPlace {
        AgentPlace {
            label: "com.wezzard.relay-agent".into(),
            root: "~".into(),
        }
    }

    /// The agent a lease record names in `relay_agent` (`label`, `root`), as
    /// a durable Mac's instance derives it from its state folder
    /// (DC-213); a record without them names a guest's agent.
    pub fn of_record(record: &Value) -> AgentPlace {
        let agent = &record["relay_agent"];
        match (agent["label"].as_str(), agent["root"].as_str()) {
            (Some(label), Some(root)) => AgentPlace {
                label: label.into(),
                root: shell_quote(root),
            },
            _ => AgentPlace::guest(),
        }
    }

    /// The agent's configuration file, as a shell word.
    pub fn config(&self) -> String {
        format!("{}/.config/relay-agent/config.json", self.root)
    }
}

/// The real target.
pub struct RealTarget {
    /// What it is.
    pub settings: TargetSettings,
    instance: Instance,
    kind: Option<TargetKind>,
    /// Discovery's answer, after start.
    pub discovery: Option<Discovery>,
    /// The broker's address, as this machine resolves the advertised name.
    pub broker_addr: Option<SocketAddr>,
    /// The lease, once acquired.
    pub lease: Option<Lease>,
    /// Why the lease cannot serve the suite, once known.
    pub blocked: Option<Stop>,
    client: Option<Arc<RelayClient>>,
    /// Facts found while setting the lease up, for the scenarios that judge
    /// them.
    pub facts: Map<String, Value>,
    /// Credentials of the released lease, for AC-60 and AC-66.
    pub released: Option<(String, Option<String>)>,
    log: Vec<Value>,
    started: Instant,
}

impl RealTarget {
    /// A target that has not started.
    pub fn new(settings: TargetSettings) -> RealTarget {
        RealTarget {
            instance: Instance::new(&settings.instance),
            settings,
            kind: None,
            discovery: None,
            broker_addr: None,
            lease: None,
            blocked: None,
            client: None,
            facts: Map::new(),
            released: None,
            log: Vec::new(),
            started: Instant::now(),
        }
    }

    /// The instance.
    pub fn instance(&self) -> &Instance {
        &self.instance
    }

    /// Note what happened, for the report.
    pub fn note(&mut self, what: &str, detail: Value) {
        self.log.push(json!({
            "at_s": self.started.elapsed().as_secs_f64(),
            "what": what,
            "detail": detail,
        }));
    }

    /// The client options for this lease through the rig.
    pub fn options(&self, rig: &ClientSide) -> Result<ClientOptions, String> {
        let lease = self.lease.as_ref().ok_or("no lease")?;
        Ok(self.options_for(rig, &lease.credential, "client"))
    }

    /// Client options with another credential or role, through the rig.
    pub fn options_for(&self, rig: &ClientSide, credential: &str, role: &str) -> ClientOptions {
        let edge = std::fs::read_to_string(&rig.ca_bundle).unwrap_or_default();
        let url = self
            .lease
            .as_ref()
            .map(|lease| lease.url.clone())
            .or_else(|| self.discovery.as_ref().map(|d| d.relay_url.clone()))
            .unwrap_or_default();
        ClientOptions {
            url,
            credential: credential.into(),
            role: role.into(),
            roots_pem: vec![edge],
            resolve_to: Some(rig.connect),
            grace_ms: None,
            timing: remote_session::Timing::default(),
        }
    }

    /// The lease's client session, acquiring the lease and setting it up on
    /// first use.
    pub fn client(&mut self, rig: &ClientSide) -> Result<Arc<RelayClient>, Stop> {
        if let Some(stop) = &self.blocked {
            return Err(stop.clone());
        }
        if self.lease.is_none() {
            if let Err(stop) = self.acquire(rig) {
                self.blocked = Some(stop.clone());
                return Err(stop);
            }
        }
        if self.client.as_ref().is_none_or(|c| c.ended().is_some()) {
            let options = self.options(rig)?;
            self.client = Some(Arc::new(RelayClient::connect(&options)?));
        }
        Ok(Arc::clone(self.client.as_ref().expect("a client")))
    }

    /// The current client session, if one is open.
    pub fn current_client(&self) -> Option<Arc<RelayClient>> {
        self.client.as_ref().map(Arc::clone)
    }

    /// Drop the current session; the next [`RealTarget::client`] opens a new
    /// one.
    pub fn new_session(&mut self) {
        if let Some(client) = self.client.take() {
            if let Ok(client) = Arc::try_unwrap(client) {
                client.close();
            }
        }
    }

    /// Where the lease's relay agent is.
    pub fn agent(&self) -> AgentPlace {
        self.lease
            .as_ref()
            .map_or_else(AgentPlace::guest, |lease| lease.agent.clone())
    }

    /// The lease's guest system.
    pub fn os(&self) -> Os {
        self.lease.as_ref().map_or(Os::Macos, |lease| lease.os)
    }

    /// The host control, or why a scenario cannot run without it.
    pub fn host(&self) -> Result<&HostControl, Stop> {
        self.settings
            .host
            .as_ref()
            .ok_or_else(|| Stop::Failed("this run has no host control (--host-shell)".into()))
    }

    fn acquire(&mut self, rig: &ClientSide) -> Result<(), Stop> {
        let started = Instant::now();
        let durable = self.kind == Some(TargetKind::DurableMac);
        let answer = if durable {
            let name = self
                .discovery
                .as_ref()
                .and_then(|d| d.health.get("instance"))
                .and_then(Value::as_str)
                .ok_or("discovery names no machine")?
                .to_string();
            self.instance
                .acquire_durable(&name, &self.settings.purpose)?
        } else {
            let image = self
                .settings
                .image
                .clone()
                .ok_or("a virtual machine target needs --image")?;
            self.instance.acquire(&image, &self.settings.purpose, 6.0)?
        };
        let acquired_in = started.elapsed();
        let name = answer
            .get("vm")
            .or_else(|| answer.get("machine"))
            .or_else(|| answer.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mut record = answer.clone();
        let relay = record
            .as_object_mut()
            .and_then(|r| r.remove("relay"))
            .unwrap_or(Value::Null);
        self.note(
            "acquired",
            json!({"name": name, "seconds": acquired_in.as_secs_f64(), "link": record.get("link")}),
        );
        let field = |key: &str| relay.get(key).and_then(Value::as_str).map(str::to_string);
        let (Some(credential), Some(url), Some(ca_pem)) =
            (field("credential"), field("url"), field("ca_pem"))
        else {
            // Keep what was acquired, so that it is released.
            self.lease = Some(Lease {
                name,
                record: record.clone(),
                credential: String::new(),
                url: String::new(),
                ca_pem: String::new(),
                acquired_in,
                agent_credential: None,
                os: Os::Macos,
                agent: AgentPlace::guest(),
            });
            return Err(Stop::Failed(format!(
                "the lease is not served by the relay link: link {:?}, {}",
                record.get("link"),
                record.get("link_reason").cloned().unwrap_or(Value::Null)
            )));
        };
        let agent = AgentPlace::of_record(&record);
        let link = (
            record.get("link").cloned(),
            record.get("link_reason").cloned(),
        );
        self.lease = Some(Lease {
            name: name.clone(),
            record,
            credential,
            url,
            ca_pem,
            acquired_in,
            agent_credential: None,
            os: Os::Macos,
            agent,
        });
        let options = self.options(rig)?;
        let mut client = RelayClient::connect(&options)?;
        // A durable Mac's agent was up before the cell started its chain,
        // and dials it again on its own backoff, at most 5 s (DC-327): it
        // gets a minute to have its session before its absence counts.
        if durable {
            let deadline = Instant::now() + Duration::from_secs(60);
            while client.ready.agent.is_none() && Instant::now() < deadline {
                drop(client);
                std::thread::sleep(Duration::from_secs(2));
                client = RelayClient::connect(&options)?;
            }
        }
        let agent = client.ready.agent.clone();
        self.facts.insert("ready_agent".into(), json!(agent));
        let client = Arc::new(client);
        let Some(agent) = agent else {
            self.client = Some(client);
            // Every instance with durable exposure on brings its own relay
            // agent up (DC-213), so a missing one is a failure, with
            // the instance's own reason.
            return Err(Stop::Failed(format!(
                "the lease's relay agent has no session with the broker: link {:?}, {}",
                link.0,
                link.1.unwrap_or(Value::Null)
            )));
        };
        let os = Os::from_agent(&agent.os);
        if let Some(lease) = self.lease.as_mut() {
            lease.os = os;
        }
        self.client = Some(client);
        self.set_up(os, durable)
    }

    /// Turn sshd off in a VM and gather the facts the scenarios judge.
    fn set_up(&mut self, os: Os, durable: bool) -> Result<(), Stop> {
        let client = Arc::clone(self.client.as_ref().expect("a client"));
        if !durable {
            let off = client.exec(os.sshd_off(), 120.0, Duration::from_secs(600))?;
            let text = off.text();
            if off.rc()? != Some(0) || !text.contains("ssh-off") {
                return Err(Stop::Failed(format!(
                    "sshd could not be turned off: {}",
                    text.trim()
                )));
            }
            self.facts.insert("sshd".into(), json!("off"));
        }
        let place = self.agent();
        let pid = client.run(&os.agent_pid(&place.label))?.trim().to_string();
        let listening = client.run(&format!("pid={pid}; {}", os.listening_of_pid()))?;
        let config = client.exec(
            &format!("cat {}", place.config()),
            60.0,
            Duration::from_secs(300),
        )?;
        let config: Value = serde_json::from_slice(&config.bytes).unwrap_or(Value::Null);
        let agent_credential = config
            .get("agent_credential")
            .and_then(Value::as_str)
            .map(str::to_string);
        let broker_url = config.get("broker_url").cloned().unwrap_or(Value::Null);
        self.facts.insert("agent_pid".into(), json!(pid));
        self.facts
            .insert("agent_listening".into(), json!(listening.trim()));
        self.facts.insert("agent_broker_url".into(), broker_url);
        if let Some(lease) = self.lease.as_mut() {
            lease.agent_credential = agent_credential;
        }
        let facts = Value::Object(self.facts.clone());
        self.note("set up", facts);
        Ok(())
    }

    /// Release the lease, keeping its credentials for the refusal checks.
    pub fn release(&mut self) -> Result<Value, String> {
        let lease = self.lease.take().ok_or("no lease")?;
        let answer = self
            .instance
            .release(&lease.name, release_body(&lease.record));
        self.note("released", json!({"name": lease.name, "answer": answer}));
        // A durable Mac's agent credential is the machine's and outlives
        // the lease (RA-4); only a virtual machine's ends with it.
        let agent = (self.kind != Some(TargetKind::DurableMac))
            .then(|| lease.agent_credential.clone())
            .flatten();
        self.released = Some((lease.credential.clone(), agent));
        answer
    }

    /// The lease's machine name and lease ID, while the suite holds one.
    pub fn lease_id(&self) -> Option<(String, String)> {
        let lease = self.lease.as_ref()?;
        let id = lease.record.get("lease_id")?.as_str()?;
        Some((lease.name.clone(), id.to_string()))
    }

    /// Whether the target is a durable Mac.
    pub fn durable(&self) -> bool {
        self.kind == Some(TargetKind::DurableMac)
    }

    /// Acquire a lease of the shortest TTL, 0.1 hours, beside the suite's
    /// own, and return it with the end of its relay credentials as the
    /// instance derives it: the end of the TTL plus the instance's grace
    /// bounded by the TTL (RP-3, DC-214).
    pub fn short_lease(&mut self) -> Result<ShortLease, Stop> {
        let grace = self.instance.machines()?["limits"]["grace_hours"]
            .as_f64()
            .ok_or("GET /machines names no limits.grace_hours")?;
        let purpose = format!("{}-short", self.settings.purpose);
        let answer = if self.durable() {
            let name = self
                .discovery
                .as_ref()
                .and_then(|d| d.health.get("instance"))
                .and_then(Value::as_str)
                .ok_or("discovery names no machine")?
                .to_string();
            self.instance
                .acquire_durable_for(&name, &purpose, SHORT_TTL_HOURS)?
        } else {
            let image = self
                .settings
                .image
                .clone()
                .ok_or("a virtual machine target needs --image")?;
            self.instance.acquire(&image, &purpose, SHORT_TTL_HOURS)?
        };
        let name = answer
            .get("vm")
            .or_else(|| answer.get("machine"))
            .or_else(|| answer.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let short = ShortLease {
            name,
            release: release_body(&answer),
            credential: answer["relay"]["credential"].as_str().map(str::to_string),
            until: answer["ttl_expires_at"]
                .as_f64()
                .map(|ttl| credential_until(ttl, SHORT_TTL_HOURS, grace)),
            link: answer.get("link").cloned().unwrap_or(Value::Null),
        };
        self.note(
            "short lease acquired",
            json!({"name": short.name, "until": short.until, "grace_hours": grace, "link": short.link}),
        );
        Ok(short)
    }

    /// Release a lease [`RealTarget::short_lease`] acquired; one that GC
    /// reclaimed at its expiry is already gone.
    pub fn release_short(&mut self, short: &ShortLease) {
        let answer = self.instance.release(&short.name, short.release.clone());
        self.note(
            "short lease released",
            json!({"name": short.name, "answer": format!("{answer:?}")}),
        );
    }
}

impl RealTarget {
    /// A durable cell's record of what the instance exposes, from its host:
    /// durable exposure off loopback must name this cell (DC-243),
    /// and the run's evidence keeps it as `host-exposure.json`.
    fn record_exposure(&mut self, cell: Cell, run_dir: &Path) -> Result<(), String> {
        let Some(control) = self.settings.host.as_ref() else {
            // A test environment, which a lab job reaches without host
            // control, says the same in its `/health` (DC-273).
            let health = self
                .discovery
                .as_ref()
                .map(|discovery| discovery.health.clone())
                .or_else(|| {
                    self.instance
                        .discover()
                        .ok()
                        .map(|discovery| discovery.health)
                })
                .ok_or("a durable cell needs --host-shell or the instance's /health")?;
            let exposure = exposure_of_health(&health, self.instance.base());
            let text = serde_json::to_string_pretty(&exposure).map_err(|e| e.to_string())?;
            std::fs::write(run_dir.join("host-exposure.json"), &text).map_err(|e| e.to_string())?;
            self.note("host exposure", exposure.clone());
            return exposure_admits(&exposure, &cell.id());
        };
        let text = control.run(&["exposure"])?;
        let exposure: Value = serde_json::from_str(text.trim())
            .map_err(|e| format!("the host's exposure is not JSON ({e}): {}", text.trim()))?;
        std::fs::write(run_dir.join("host-exposure.json"), &text).map_err(|e| e.to_string())?;
        self.note("host exposure", exposure.clone());
        exposure_admits(&exposure, &cell.id())
    }

    /// The lease's relay agent, when the suite restarts or stops it: on a
    /// durable Mac only a second instance's own agent, never the installed
    /// one's (DC-249).
    pub fn agent_job(&self) -> Result<AgentPlace, Stop> {
        let place = self.agent();
        if self.durable() && !second_instance::owned_agent_label(&place.label) {
            return Err(Stop::Failed(format!(
                "the lease names relay agent {}, which is not a second instance's; the suite \
never restarts the installed instance's agent",
                place.label
            )));
        }
        Ok(place)
    }
}

/// What an instance exposes, from its `/health` and the URL it was reached
/// at: durable exposure when it offers a durable machine, and the cells its
/// launcher named.
pub fn exposure_of_health(health: &Value, base: &str) -> Value {
    let durable = health["kinds"]
        .as_array()
        .is_some_and(|kinds| kinds.iter().any(|kind| kind == "durable"));
    let host = base
        .split_once("://")
        .map_or(base, |(_, rest)| rest)
        .rsplit_once(':')
        .map_or(base, |(host, _)| host);
    let loopback = host == "localhost"
        || host
            .trim_matches(|c| c == '[' || c == ']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    json!({
        "source": "health",
        "durable": durable,
        "loopback": loopback,
        "durable_cell": health["second_instance"]["durable_cell"],
    })
}

/// Whether a durable cell may run against an instance exposing `exposure`:
/// durable exposure on, and, off loopback, for this cell.
fn exposure_admits(exposure: &Value, cell: &str) -> Result<(), String> {
    if exposure["durable"] != true {
        return Err("the instance has durable exposure off".into());
    }
    let named = exposure["durable_cell"]
        .as_str()
        .is_some_and(|cells| second_instance::names_cell(cells, cell));
    if exposure["loopback"] != true && !named {
        return Err(format!(
            "the instance exposes its Mac on {} for cell {}, not for {cell}",
            exposure["bind"], exposure["durable_cell"]
        ));
    }
    Ok(())
}

/// The port of an `http://host:port` base URL; 80 when it names none.
pub fn base_port(base: &str) -> u16 {
    let rest = base.split_once("://").map_or(base, |(_, rest)| rest);
    let authority = rest.split('/').next().unwrap_or_default();
    authority
        .rsplit_once(':')
        .filter(|(host, _)| !host.ends_with('[') && (!host.contains(':') || host.ends_with(']')))
        .and_then(|(_, port)| port.parse().ok())
        .unwrap_or(80)
}

/// The TTL of AC-66's short lease, the shortest an acquisition takes.
pub const SHORT_TTL_HOURS: f64 = 0.1;

/// A lease of the shortest TTL, for AC-66's expired credential.
pub struct ShortLease {
    /// The machine's name.
    pub name: String,
    /// Its release request.
    pub release: Value,
    /// Its client credential, when the relay link serves it.
    pub credential: Option<String>,
    /// When its relay credentials end, in seconds since the epoch.
    pub until: Option<f64>,
    /// Its `link`.
    pub link: Value,
}

/// The end of a lease's relay credentials: the end of its TTL plus the
/// instance's grace, bounded by the TTL (RP-3).
pub fn credential_until(ttl_expires_at: f64, ttl_hours: f64, grace_hours: f64) -> f64 {
    ttl_expires_at + grace_hours.min(ttl_hours).max(0.0) * 3600.0
}

/// A release request: a durable lease names its `lease_id` (DM).
fn release_body(record: &Value) -> Value {
    let mut body = json!({"reason": "relay acceptance done"});
    if let Some(id) = record.get("lease_id").filter(|id| !id.is_null()) {
        body["lease_id"] = id.clone();
    }
    body
}

fn resolve(host: &str, port: u16) -> Result<SocketAddr, String> {
    let addrs: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("resolve {host}: {e}"))?
        .collect();
    addrs
        .iter()
        .find(|a| a.is_ipv4())
        .or_else(|| addrs.first())
        .copied()
        .ok_or_else(|| format!("{host} has no address"))
}

impl Target for RealTarget {
    fn start(&mut self, cell: Cell, run_dir: &Path) -> Result<Endpoint, RigError> {
        let fail = |why: String| RigError::Process {
            name: "target".into(),
            reason: why,
        };
        self.kind = Some(cell.target);
        let discovery = self.instance.discover().map_err(fail)?;
        // Never the installed instance: only one that reports itself as a
        // second instance, off the installed ports (DC-245).
        let id = second_instance::guard(base_port(self.instance.base()), &discovery.health)
            .map_err(fail)?;
        self.note("second instance", json!(id));
        if cell.target == TargetKind::DurableMac {
            self.record_exposure(cell, run_dir).map_err(fail)?;
        }
        let (host, port) = url_host_port(&discovery.relay_url).map_err(fail)?;
        // The rig, as the network, finds where the name leads; the client
        // is only ever told the name.
        let addr = resolve(&host, port).map_err(fail)?;
        if let Some(control) = self
            .settings
            .host
            .as_ref()
            .filter(|_| self.settings.manage_agent_rig)
        {
            // A durable Mac's own relay agent dials the broker through the
            // same chain as a guest's, so its connection can be faulted
            // too (DC-327).
            let seed = self.settings.seed.wrapping_add(1).to_string();
            let started = control
                .run(&["rig-start", &self.settings.profile, &seed])
                .map_err(fail)?;
            std::fs::write(run_dir.join("agent-rig.json"), &started)
                .map_err(|e| fail(e.to_string()))?;
            self.note("agent chain started", json!(started.trim()));
        }
        self.broker_addr = Some(addr);
        let endpoint = Endpoint {
            broker_name: host.to_ascii_lowercase(),
            addr,
            ca_pem: discovery.ca_pem.clone(),
        };
        self.note(
            "discovered",
            json!({"relay_url": discovery.relay_url, "relay_ca_sha256": discovery.relay_ca_sha256}),
        );
        self.discovery = Some(discovery);
        Ok(endpoint)
    }

    fn finish(&mut self) -> Value {
        self.new_session();
        if self.lease.is_some() {
            let _ = self.release();
        }
        if let Some(control) = self
            .settings
            .host
            .as_ref()
            .filter(|_| self.settings.manage_agent_rig)
        {
            let stopped = control.run(&["rig-stop"]);
            self.note("agent chain stopped", json!(format!("{stopped:?}")));
        }
        json!({
            "instance": self.instance.base(),
            "image": self.settings.image,
            "facts": self.facts,
            "log": self.log,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_durable_record_names_its_relay_agent() {
        let durable = AgentPlace::of_record(&json!({"relay_agent": {
            "label": "com.wezzard.relay-agent.0123456789ab",
            "root": "/state dir/relay/agent",
        }}));
        assert_eq!(durable.label, "com.wezzard.relay-agent.0123456789ab");
        assert_eq!(
            durable.config(),
            "'/state dir/relay/agent'/.config/relay-agent/config.json"
        );
        let guest = AgentPlace::of_record(&json!({"relay_agent": {"os": "linux"}}));
        assert_eq!(guest, AgentPlace::guest());
        assert_eq!(guest.config(), "~/.config/relay-agent/config.json");
    }

    #[test]
    fn a_base_url_names_its_port() {
        assert_eq!(base_port("http://mac.local:6262"), 6262);
        assert_eq!(base_port("http://mac.local:6240/"), 6240);
        assert_eq!(base_port("http://[2001:db8::1]:6262"), 6262);
        assert_eq!(base_port("http://mac.local"), 80);
    }

    #[test]
    fn a_durable_cell_runs_only_where_its_exposure_names_it() {
        let lan = json!({"durable": true, "loopback": false, "bind": "192.0.2.4", "durable_cell": "macos-client.durable-mac"});
        assert!(exposure_admits(&lan, "macos-client.durable-mac").is_ok());
        assert!(exposure_admits(&lan, "ubuntu-client.durable-mac").is_err());
        let local = json!({"durable": true, "loopback": true, "durable_cell": null});
        assert!(exposure_admits(&local, "macos-client.durable-mac").is_ok());
        assert!(exposure_admits(&json!({"durable": false, "loopback": true}), "x").is_err());
        // A durable Mac's test environment serves both durable cells (DC-273).
        let lab = json!({"durable": true, "loopback": false,
            "durable_cell": "macos-client.durable-mac,ubuntu-client.durable-mac"});
        assert!(exposure_admits(&lab, "ubuntu-client.durable-mac").is_ok());
        assert!(exposure_admits(&lab, "macos-client.macos-vm").is_err());
    }

    #[test]
    fn a_test_environments_exposure_comes_from_its_health() {
        let health = json!({"kinds": ["durable", "virtual"],
            "second_instance": {"id": "r42", "durable_cell": "macos-client.durable-mac"}});
        let exposure = exposure_of_health(&health, "http://mac-b:6272");
        assert_eq!(exposure["durable"], true);
        assert_eq!(exposure["loopback"], false);
        assert!(exposure_admits(&exposure, "macos-client.durable-mac").is_ok());
        assert!(exposure_admits(&exposure, "ubuntu-client.durable-mac").is_err());
        let local = exposure_of_health(
            &json!({"kinds": ["durable"], "second_instance": {"id": "x"}}),
            "http://127.0.0.1:6250",
        );
        assert_eq!(local["loopback"], true);
        let off = exposure_of_health(&json!({"kinds": ["virtual"]}), "http://mac-a:6272");
        assert!(exposure_admits(&off, "macos-client.durable-mac").is_err());
    }

    #[test]
    fn a_release_names_the_lease_id_when_the_lease_has_one() {
        let body = release_body(&json!({"lease_id": "abc"}));
        assert_eq!(body["lease_id"], "abc");
        assert!(release_body(&json!({"vm": "x"})).get("lease_id").is_none());
    }

    #[test]
    fn a_credential_ends_within_one_ttl_of_its_lease() {
        // The grace is the instance's, bounded by the TTL.
        assert_eq!(credential_until(1000.0, 0.1, 6.0), 1000.0 + 360.0);
        assert_eq!(credential_until(1000.0, 24.0, 6.0), 1000.0 + 6.0 * 3600.0);
        assert_eq!(credential_until(1000.0, 0.1, 0.0), 1000.0);
    }
}
