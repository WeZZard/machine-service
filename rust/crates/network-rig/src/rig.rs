//! The chain: the reset stage's front hop, Toxiproxy, mitmproxy with the edge
//! script, the reset stage's back hop, then the broker.
//!
//! ```text
//! client --wss://<name>:<port>--> front hop --> Toxiproxy --> mitmdump + edge.py --> back hop --TLS--> broker
//!          (dials 127.0.0.1:<port>)  (resets)    (TCP faults)  (TLS ends, name routing,  (resets,
//!                                                               idle close, message limit) source address)
//! ```
//!
//! Every process listens on loopback. A client is told only the chain's one
//! port and the broker's host name ([`ClientSide`]); it resolves the name to
//! the chain's port itself, the way `curl --resolve` does, so no hosts file,
//! DNS server or other system setting changes. The edge serves only the names
//! it routes, so a client that dials by address is refused (TC-6).

use std::fs;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::json;

use crate::process::{self, Child};
use crate::profile::{Profile, Stream, Toxic, ToxicKind};
use crate::schedule::{Event, Fault, Schedule};
use crate::stage::{AddressChange, Connection, Select, Side, Stage};
use crate::tls::Authority;
use crate::tools::{self, Tool};
use crate::toxiproxy::Api;
use crate::RigError;

/// The broker's name on the rig. `.test` is reserved for testing (RFC 6761),
/// so the name can never reach a real host.
pub const DEFAULT_BROKER_NAME: &str = "broker.relay-rig.test";

/// The path of the broker's WebSocket listener ([BK](../../../docs/arch/broker.md)).
pub const RELAY_PATH: &str = "/relay";

/// The edge script, checked into this crate and written into each run.
pub const EDGE_SCRIPT: &str = include_str!("../edge/edge.py");

/// The span a schedule covers unless a test says otherwise: longer than any
/// suite run, so the schedule never runs out.
pub const DEFAULT_HORIZON_MS: u64 = 6 * 3_600_000;

const PROXY: &str = "relay";
const READY: Duration = Duration::from_secs(90);

/// How one rig is set up.
#[derive(Debug, Clone)]
pub struct RigConfig {
    /// What the chain does.
    pub profile: Profile,
    /// The broker's host name, the only name the edge serves.
    pub broker_name: String,
    /// Where the broker listens. Only the edge is told.
    pub upstream: SocketAddr,
    /// The authority that signed the broker's certificate, PEM.
    pub upstream_ca_pem: String,
    /// The schedule's seed.
    pub seed: u64,
    /// The span the schedule covers.
    pub horizon_ms: u64,
    /// The run's directory for configuration, logs and the run record.
    pub run_dir: PathBuf,
    /// A test-only idle close in seconds, in place of the profile's.
    pub idle_close_s: Option<u64>,
    /// Where the chain's one port listens: a free loopback port unless set.
    /// A relay agent's chain on a VM host listens where its guests reach it.
    pub listen: SocketAddr,
    /// Who the chain's one port admits: anyone, or only the host's guests
    /// for a relay agent's chain that listens on every interface.
    pub admit: crate::admit::Admit,
    /// The authority the edge signs its certificates with, instead of one
    /// made for the run: the authority a client already trusts, such as the
    /// broker's own for a relay agent that trusts only that one.
    pub edge_authority: Option<EdgeAuthority>,
    /// Addresses a client may dial in place of the broker's name, which the
    /// edge's certificate names beside it: a relay agent dials its host's
    /// address on the guest network (BK-5).
    pub edge_addresses: Vec<String>,
}

/// A certificate authority given to the edge, PEM.
#[derive(Debug, Clone)]
pub struct EdgeAuthority {
    /// The common name of the authority's certificate.
    pub common_name: String,
    /// The authority's private key.
    pub key_pem: String,
    /// The authority's certificate.
    pub cert_pem: String,
}

impl RigConfig {
    /// A configuration with the default name and horizon and the seed of
    /// [`seed_from_env`].
    pub fn new(
        profile: Profile,
        upstream: SocketAddr,
        upstream_ca_pem: &str,
        run_dir: &Path,
    ) -> RigConfig {
        RigConfig {
            profile,
            broker_name: DEFAULT_BROKER_NAME.into(),
            upstream,
            upstream_ca_pem: upstream_ca_pem.into(),
            seed: seed_from_env(),
            horizon_ms: DEFAULT_HORIZON_MS,
            run_dir: run_dir.into(),
            idle_close_s: None,
            listen: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            admit: crate::admit::Admit::Any,
            edge_authority: None,
            edge_addresses: Vec::new(),
        }
    }
}

/// `NETWORK_RIG_SEED` when set, so a recorded run replays; otherwise a fresh
/// seed, which the run record keeps.
pub fn seed_from_env() -> u64 {
    if let Some(seed) = std::env::var("NETWORK_RIG_SEED")
        .ok()
        .and_then(|s| s.trim().parse().ok())
    {
        return seed;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    crate::prng::SplitMix64::new(nanos as u64 ^ u64::from(std::process::id())).next_u64()
}

/// What a client is told about the broker on this rig.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientSide {
    /// The broker's host name.
    pub host: String,
    /// The chain's one port.
    pub port: u16,
    /// The broker's URL, `wss://<host>:<port>/relay`.
    pub url: String,
    /// The address the client resolves the host name to: the chain's port.
    pub connect: SocketAddr,
    /// The PEM bundle the client trusts: the edge's authority alone.
    pub ca_bundle: PathBuf,
}

impl ClientSide {
    /// The resolution override in `curl --resolve` form, `host:port:address`.
    pub fn resolve(&self) -> String {
        format!("{}:{}:{}", self.host, self.port, self.connect.ip())
    }

    /// The environment of a client process on the rig. `SSL_CERT_FILE` makes
    /// OpenSSL-based clients trust the edge.
    pub fn env(&self) -> Vec<(String, String)> {
        vec![
            ("NETWORK_RIG_URL".into(), self.url.clone()),
            ("NETWORK_RIG_RESOLVE".into(), self.resolve()),
            (
                "NETWORK_RIG_CA_BUNDLE".into(),
                self.ca_bundle.display().to_string(),
            ),
            ("SSL_CERT_FILE".into(), self.ca_bundle.display().to_string()),
        ]
    }
}

/// The rig's control: set or clear a fault now, through Toxiproxy's API and
/// the reset stage.
#[derive(Debug, Clone)]
pub struct Control {
    api: Api,
    stage: Arc<Stage>,
}

impl Control {
    /// Add any toxic to the chain.
    pub fn add_toxic(&self, toxic: &Toxic) -> Result<(), RigError> {
        self.api.add_toxic(PROXY, toxic)
    }

    /// Remove a toxic by name.
    pub fn remove_toxic(&self, name: &str) -> Result<(), RigError> {
        self.api.remove_toxic(PROXY, name)
    }

    /// The names of the chain's toxics, the profile's and the faults'.
    pub fn toxics(&self) -> Result<Vec<String>, RigError> {
        self.api.toxics(PROXY)
    }

    /// Arm a reset: each connection that carries data in `stream` is reset.
    pub fn reset_peer(&self, stream: Stream) -> Result<(), RigError> {
        self.apply(&Fault::Reset {
            stream,
            window_ms: 0,
        })
    }

    /// Hold all data in `stream` without closing anything.
    pub fn blackhole(&self, stream: Stream) -> Result<(), RigError> {
        self.apply(&Fault::Blackhole {
            stream,
            duration_ms: 0,
        })
    }

    /// Close every connection through the chain.
    pub fn drop_connections(&self) -> Result<(), RigError> {
        self.api.set_enabled(PROXY, false)?;
        self.api.set_enabled(PROXY, true)
    }

    /// Close every connection through the chain, so each is dialled again and
    /// reaches the broker from a new source address and port, or a new port
    /// only ([`Control::address_change`]).
    pub fn change_address(&self) -> Result<(), RigError> {
        self.stage.change_address();
        self.drop_connections()
    }

    /// What an address change changes on this machine.
    pub fn address_change(&self) -> AddressChange {
        self.stage.address_change()
    }

    /// The connections open through the chain, in the order they were opened.
    pub fn connections(&self) -> Vec<Connection> {
        self.stage.connections()
    }

    /// Reset the selected open connections with a TCP RST on `side`, and
    /// return the IDs of the connections cut (DC-137).
    pub fn cut(&self, select: Select, side: Side) -> Vec<u64> {
        self.stage.cut(select, side)
    }

    /// Set `fault` now. A fault stays until [`Control::revert`].
    pub fn apply(&self, fault: &Fault) -> Result<(), RigError> {
        match *fault {
            Fault::AddressChange => self.change_address(),
            Fault::Cut { side, pick } => {
                self.cut(Select::Pick(pick), side);
                Ok(())
            }
            _ => match fault_toxic(fault) {
                Some(toxic) => self.add_toxic(&toxic),
                None => Ok(()),
            },
        }
    }

    /// Clear `fault`.
    pub fn revert(&self, fault: &Fault) -> Result<(), RigError> {
        match fault_toxic(fault) {
            Some(toxic) => self.remove_toxic(&toxic.name),
            None => Ok(()),
        }
    }
}

fn fault_toxic(fault: &Fault) -> Option<Toxic> {
    let (stream, kind) = match *fault {
        Fault::Reset { stream, .. } => (stream, ToxicKind::ResetPeer { timeout: 0 }),
        Fault::Blackhole { stream, .. } => (stream, ToxicKind::Timeout { timeout: 0 }),
        Fault::LimitData { stream, bytes, .. } => (stream, ToxicKind::LimitData { bytes }),
        Fault::AddressChange | Fault::Cut { .. } => return None,
    };
    Some(Toxic {
        name: format!("fault-{}-{}", fault.kind(), stream.as_str()),
        stream,
        toxicity: 1.0,
        kind,
    })
}

/// A step of a played schedule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// The fault is on.
    Applied,
    /// The fault is off again.
    Reverted,
    /// The rig could not set or clear the fault.
    Failed(String),
}

/// What the player did, and when, measured from the start of the play.
#[derive(Debug, Clone)]
pub struct Note {
    /// The event.
    pub event: Event,
    /// What happened to it.
    pub phase: Phase,
    /// When.
    pub at: Duration,
}

struct Player {
    stop: Sender<()>,
    thread: JoinHandle<()>,
}

/// A running chain.
pub struct Rig {
    // Fields drop in order: the player stops before the processes do.
    player: Option<Player>,
    mitmdump: Option<Child>,
    edge: Edge,
    toxiproxy: Child,
    control: Control,
    client: ClientSide,
    schedule: Schedule,
    profile: Profile,
    run_dir: PathBuf,
}

impl std::fmt::Debug for Rig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rig")
            .field("client", &self.client)
            .finish_non_exhaustive()
    }
}

impl Rig {
    /// Fetch the tools if needed, start the chain and install the profile.
    pub fn start(config: RigConfig) -> Result<Rig, RigError> {
        let run_dir = config.run_dir.clone();
        let mitm_dir = run_dir.join("mitmproxy");
        fs::create_dir_all(&mitm_dir)
            .map_err(RigError::io(format!("create {}", mitm_dir.display())))?;
        let write = |path: &Path, text: &str| {
            fs::write(path, text).map_err(RigError::io(format!("write {}", path.display())))
        };

        let authority = match &config.edge_authority {
            Some(given) => {
                Authority::from_parts(&given.common_name, &given.key_pem, &given.cert_pem)?
            }
            None => Authority::new("network-rig edge authority")?,
        };
        write(
            &mitm_dir.join("mitmproxy-ca.pem"),
            &authority.mitmproxy_pem(),
        )?;
        let client_bundle = run_dir.join("client-ca.pem");
        write(&client_bundle, authority.cert_pem())?;
        let broker_ca = run_dir.join("broker-ca.pem");
        write(&broker_ca, &config.upstream_ca_pem)?;
        let edge_script = run_dir.join("edge.py");
        write(&edge_script, EDGE_SCRIPT)?;

        let toxiproxy_bin = tools::ensure(Tool::Toxiproxy)?;
        let mitmdump_bin = tools::ensure(Tool::Mitmdump)?;
        let loopback = |port| SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let api_addr = loopback(process::free_port()?);
        let mitm_addr = loopback(process::free_port()?);
        let toxiproxy_addr = loopback(process::free_port()?);
        let pairs = run_dir.join("edge-pairs.jsonl");
        let stage = Arc::new(Stage::start_on(
            config.listen,
            config.admit,
            toxiproxy_addr,
            config.upstream,
            &pairs,
        )?);
        let broker_side = stage.broker_side();

        let mut command = Command::new(&toxiproxy_bin);
        command
            .args(["-host", "127.0.0.1", "-port", &api_addr.port().to_string()])
            .env("LOG_LEVEL", "info");
        let mut toxiproxy = Child::spawn("toxiproxy", command, &run_dir)?;
        let api = Api::new(api_addr);
        toxiproxy.wait_until(READY, || api.version().is_ok())?;

        let edge_rules = config.profile.edge;
        let idle = config.idle_close_s.unwrap_or(edge_rules.idle_close_s);
        let name = config.broker_name.to_ascii_lowercase();
        let mut edge = Edge {
            bin: mitmdump_bin,
            name: name.clone(),
            broker_side,
            listen: mitm_addr,
            script: edge_script,
            mitm_dir,
            broker_ca,
            pairs,
            idle_close_s: idle,
            max_message_bytes: edge_rules.max_message_bytes,
            authority,
            run_dir: run_dir.clone(),
            launches: 0,
            addresses: Vec::new(),
        };
        let mitmdump = edge.launch(&config.edge_addresses)?;

        api.create_proxy(PROXY, toxiproxy_addr, mitm_addr)?;
        for toxic in &config.profile.toxics {
            api.add_toxic(PROXY, toxic)?;
        }

        let schedule = match &config.profile.chaos {
            Some(chaos) => Schedule::generate(chaos, config.seed, config.horizon_ms),
            None => Schedule::empty(config.seed, config.horizon_ms),
        };
        let chain_addr = stage.addr();
        let client = ClientSide {
            host: name.clone(),
            port: chain_addr.port(),
            url: format!("wss://{name}:{}{RELAY_PATH}", chain_addr.port()),
            connect: chain_addr,
            ca_bundle: client_bundle,
        };
        let record = json!({
            "profile": config.profile,
            "seed": config.seed,
            "listen": config.listen.to_string(),
            "edge_addresses": config.edge_addresses,
            "idle_close_s": idle,
            "broker_name": name,
            "toxiproxy": tools::TOXIPROXY_VERSION,
            "mitmproxy": tools::MITMPROXY_VERSION,
            "address_change": stage.address_change(),
            "schedule": schedule,
        });
        write(&run_dir.join("rig.json"), &format!("{:#}\n", record))?;

        Ok(Rig {
            player: None,
            mitmdump: Some(mitmdump),
            edge,
            toxiproxy,
            control: Control { api, stage },
            client,
            schedule,
            profile: config.profile,
            run_dir,
        })
    }

    /// What a client is told.
    pub fn client_side(&self) -> &ClientSide {
        &self.client
    }

    /// The control for faults at an exact moment.
    pub fn control(&self) -> &Control {
        &self.control
    }

    /// The run's schedule.
    pub fn schedule(&self) -> &Schedule {
        &self.schedule
    }

    /// The run's profile.
    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    /// The run's directory.
    pub fn run_dir(&self) -> &Path {
        &self.run_dir
    }

    /// The log files of the chain's processes.
    pub fn logs(&self) -> Vec<&Path> {
        std::iter::once(self.toxiproxy.log())
            .chain(self.mitmdump.as_ref().map(Child::log))
            .collect()
    }

    /// The addresses the edge serves in place of the name.
    pub fn edge_addresses(&self) -> &[String] {
        &self.edge.addresses
    }

    /// Serve `addresses` in place of the name from now on: the edge starts
    /// again with a certificate that names them, which closes the
    /// connections through it, as an edge that is redeployed does.
    pub fn set_edge_addresses(&mut self, addresses: &[String]) -> Result<(), RigError> {
        // The old edge holds the port; it stops before the new one starts.
        self.mitmdump = None;
        self.mitmdump = Some(self.edge.launch(addresses)?);
        Ok(())
    }

    /// Play the schedule from now, on its own thread. Each fault is applied at
    /// its offset and reverted when it ends, and every step is reported on
    /// the returned channel. Playing again restarts the schedule.
    pub fn play(&mut self) -> Receiver<Note> {
        self.stop_playing();
        let (notes, receiver) = mpsc::channel();
        let (stop, stopped) = mpsc::channel();
        let control = self.control.clone();
        let events = self.schedule.events.clone();
        let thread = std::thread::spawn(move || play(control, events, notes, stopped));
        self.player = Some(Player { stop, thread });
        receiver
    }

    /// Stop playing the schedule, reverting the fault in progress.
    pub fn stop_playing(&mut self) {
        if let Some(player) = self.player.take() {
            let _ = player.stop.send(());
            let _ = player.thread.join();
        }
    }
}

/// How the edge is started, so that it can start again.
#[derive(Debug)]
struct Edge {
    bin: PathBuf,
    name: String,
    broker_side: SocketAddr,
    listen: SocketAddr,
    script: PathBuf,
    mitm_dir: PathBuf,
    broker_ca: PathBuf,
    pairs: PathBuf,
    idle_close_s: u64,
    max_message_bytes: u64,
    authority: Authority,
    run_dir: PathBuf,
    launches: u32,
    addresses: Vec<String>,
}

impl Edge {
    /// Start mitmdump with the edge script. With `addresses`, the edge
    /// presents one certificate of its authority for the name and them.
    fn launch(&mut self, addresses: &[String]) -> Result<Child, RigError> {
        let name = &self.name;
        let mut command = Command::new(&self.bin);
        command
            .arg("--mode")
            .arg(format!(
                "reverse:https://{name}:{}@{}",
                self.broker_side.port(),
                self.listen.port()
            ))
            .args(["--listen-host", "127.0.0.1"])
            .arg("--scripts")
            .arg(&self.script)
            .arg("--set")
            .arg(format!("confdir={}", self.mitm_dir.display()))
            .arg("--set")
            .arg(format!(
                "ssl_verify_upstream_trusted_ca={}",
                self.broker_ca.display()
            ))
            .args(["--set", "connection_strategy=lazy"])
            .args(["--set", "keep_host_header=true"])
            .args(["--set", "upstream_cert=false"])
            .args(["--set", "flow_detail=1"])
            .arg("--set")
            .arg(format!("edge_routes={name}={}", self.broker_side))
            .arg("--set")
            .arg(format!("edge_pairs_file={}", self.pairs.display()))
            .arg("--set")
            .arg(format!("edge_idle_close_s={}", self.idle_close_s))
            .arg("--set")
            .arg(format!("edge_max_message_bytes={}", self.max_message_bytes))
            .env_remove("SSL_CERT_FILE")
            .env_remove("HTTPS_PROXY")
            .env_remove("HTTP_PROXY");
        if !addresses.is_empty() {
            let mut names = vec![name.as_str()];
            names.extend(addresses.iter().map(String::as_str));
            let leaf = self.authority.issue(&names)?;
            let path = self.mitm_dir.join("edge-cert.pem");
            fs::write(&path, format!("{}{}", leaf.key_pem, leaf.cert_pem))
                .map_err(RigError::io(format!("write {}", path.display())))?;
            command
                .arg("--set")
                .arg(format!("certs=*={}", path.display()))
                .arg("--set")
                .arg(format!("edge_addresses={}", addresses.join(",")));
        }
        self.launches += 1;
        let log = if self.launches == 1 {
            "mitmdump".to_string()
        } else {
            format!("mitmdump-{}", self.launches)
        };
        let mut child = Child::spawn(&log, command, &self.run_dir)?;
        let listen = self.listen;
        child.wait_until(READY, || process::accepts(listen))?;
        self.addresses = addresses.to_vec();
        Ok(child)
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.stop_playing();
    }
}

fn play(control: Control, events: Vec<Event>, notes: Sender<Note>, stopped: Receiver<()>) {
    let start = Instant::now();
    // Sleep until `offset`; false when asked to stop.
    let wait_until = |offset: Duration| {
        let left = offset.saturating_sub(start.elapsed());
        matches!(stopped.recv_timeout(left), Err(RecvTimeoutError::Timeout))
    };
    let note = |event: &Event, phase: Phase| {
        let _ = notes.send(Note {
            event: *event,
            phase,
            at: start.elapsed(),
        });
    };
    for event in &events {
        if !wait_until(Duration::from_millis(event.at_ms)) {
            return;
        }
        match control.apply(&event.fault) {
            Ok(()) => note(event, Phase::Applied),
            Err(error) => {
                note(event, Phase::Failed(error.to_string()));
                continue;
            }
        }
        let end = Duration::from_millis(event.at_ms + event.fault.duration_ms());
        let go_on = wait_until(end);
        match control.revert(&event.fault) {
            Ok(()) => note(event, Phase::Reverted),
            Err(error) => note(event, Phase::Failed(error.to_string())),
        }
        if !go_on {
            return;
        }
    }
}
