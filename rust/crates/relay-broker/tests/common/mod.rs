//! Fixtures for the broker's TL-2 tests: a broker on a loopback listener,
//! relay agents run in-process over real sessions, and a test client that
//! speaks the relay protocol (`docs/arch/relay-protocol.md`).

#![allow(dead_code)]

use std::collections::HashMap;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use relay_agent::config::Config;
use relay_agent::protocol::{
    Call, CallResult, Digest, ExecutionState, Message, Op, Pending, Ready, MAX_CHUNK, UPLOAD_WINDOW,
};
use relay_agent::session_link::{self, DialOptions};
use relay_agent::{Agent, AgentOptions};
use relay_broker::tls::Authority;
use relay_broker::{Broker, BrokerConfig, Role, Target};
use remote_session::{ClientConfig, Listening, ServerConfig, Session, SessionServer, Timing};

/// Fast session timing: heartbeat 100 ms, dead after 1 s.
pub fn fast_timing() -> Timing {
    Timing {
        heartbeat: Duration::from_millis(100),
        dead_after: Duration::from_secs(1),
        handshake_timeout: Duration::from_secs(5),
        poll_interval: Duration::from_millis(5),
    }
}

pub struct Options {
    pub grace: Duration,
    pub agent_wait: Duration,
    pub tls: Option<Arc<Authority>>,
    pub port: u16,
    /// The session's replay buffer bound, per side.
    pub max_buffered: Option<usize>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            grace: Duration::from_secs(30),
            agent_wait: Duration::from_secs(10),
            tls: None,
            port: 0,
            max_buffered: None,
        }
    }
}

/// A broker serving one loopback listener.
pub struct Harness {
    pub broker: Broker,
    pub server: SessionServer,
    pub listening: Option<Listening>,
    pub addr: SocketAddr,
    pub log: Arc<Mutex<Vec<String>>>,
}

impl Harness {
    pub fn start(options: Options) -> Harness {
        let log: Arc<Mutex<Vec<String>>> = Arc::default();
        let sink = Arc::clone(&log);
        let broker = Broker::new(
            BrokerConfig {
                agent_wait: options.agent_wait,
                ..BrokerConfig::default()
            },
            move |line| {
                eprintln!("{line}");
                sink.lock().unwrap().push(line.to_string());
            },
        );
        let mut config = ServerConfig::new(broker.authenticator());
        config.timing = fast_timing();
        config.grace = options.grace;
        if let Some(max) = options.max_buffered {
            config.max_buffered = max;
        }
        if let Some(authority) = &options.tls {
            config.tls = Some(
                relay_broker::tls::server_config(
                    Arc::clone(authority),
                    relay_broker::tls::host_names,
                )
                .unwrap(),
            );
        }
        let server = SessionServer::new(config).unwrap();
        let listener = bind(options.port);
        let addr = listener.local_addr().unwrap();
        let listening = server.listen(listener).unwrap();
        broker.serve(server.clone());
        Harness {
            broker,
            server,
            listening: Some(listening),
            addr,
            log,
        }
    }

    pub fn url(&self, scheme: &str) -> String {
        format!("{scheme}://127.0.0.1:{}/relay", self.addr.port())
    }

    /// Stop as a crash would: every session dropped without a bye.
    pub fn crash(mut self) -> u16 {
        if let Some(listening) = self.listening.take() {
            listening.stop();
        }
        self.server.shutdown();
        self.addr.port()
    }

    pub fn logged(&self, needle: &str) -> bool {
        self.log.lock().unwrap().iter().any(|l| l.contains(needle))
    }
}

fn bind(port: u16) -> TcpListener {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match TcpListener::bind(("127.0.0.1", port)) {
            Ok(listener) => return listener,
            Err(error) if Instant::now() < deadline => {
                let _ = error;
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => panic!("bind 127.0.0.1:{port}: {error}"),
        }
    }
}

/// A lease's credentials.
pub struct Lease {
    pub target: Target,
    pub agent: String,
    pub client: String,
}

pub fn lease(broker: &Broker, name: &str) -> Lease {
    let target = Target::Lease(name.to_string());
    Lease {
        agent: broker.issue(&target, Role::Agent, None),
        client: broker.issue(&target, Role::Client, None),
        target,
    }
}

/// A relay agent core in this process, dialling the broker over a real
/// session.
pub struct TestAgent {
    pub agent: Agent,
    pub home: PathBuf,
    _dir: Option<tempfile::TempDir>,
    thread: Option<JoinHandle<()>>,
}

pub fn agent_options(home: &Path) -> AgentOptions {
    AgentOptions {
        shell: PathBuf::from("/bin/sh"),
        home: home.to_path_buf(),
        state: home.join(".local/state/relay-agent"),
        build: "test-build".into(),
        heartbeat: Duration::from_secs(5),
        retention: Duration::from_secs(3600),
        ownership: guest_actions::ownership::Ownership::detect(),
        executable: std::env::current_exe().unwrap(),
        signing: None,
    }
}

impl TestAgent {
    pub fn start(url: &str, credential: &str, ca_pem: Option<String>) -> TestAgent {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let mut agent = Self::start_in(&home, url, credential, ca_pem, DialOptions::default());
        agent._dir = Some(dir);
        agent
    }

    /// Start an agent on an existing home, whose records it keeps.
    pub fn start_in(
        home: &Path,
        url: &str,
        credential: &str,
        ca_pem: Option<String>,
        mut dial: DialOptions,
    ) -> TestAgent {
        dial.timing.get_or_insert_with(fast_timing);
        let agent = Agent::open(agent_options(home)).unwrap();
        let config = Config {
            broker_url: url.to_string(),
            agent_credential: credential.to_string(),
            broker_ca_pem: ca_pem,
        };
        let serving = agent.clone();
        let thread = std::thread::spawn(move || {
            relay_agent::dial::run(
                &serving,
                &config,
                |c| session_link::dial(c, &dial),
                |e| eprintln!("test agent: {e}"),
            )
        });
        TestAgent {
            agent,
            home: home.to_path_buf(),
            _dir: None,
            thread: Some(thread),
        }
    }

    pub fn stop(mut self) {
        self.agent.shutdown(Duration::from_secs(1));
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for TestAgent {
    fn drop(&mut self) {
        self.agent.shutdown(Duration::from_secs(1));
    }
}

/// Everything a call received.
#[derive(Debug, Default, Clone)]
pub struct Collected {
    pub pending: Vec<Pending>,
    pub bytes: Vec<u8>,
    pub end: Option<Digest>,
    pub result: Option<CallResult>,
    pub query: Option<ExecutionState>,
    /// Every committed offset the agent reported for an upload.
    pub progress: Vec<u64>,
}

impl Collected {
    pub fn end_matches(&self) -> bool {
        self.end.as_ref().is_some_and(|end| {
            end.size == self.bytes.len() as u64
                && end.sha256 == guest_actions::sha256_hex(&self.bytes)
        })
    }
}

/// A client of the relay protocol.
pub struct TestClient {
    pub session: Session,
    pub ready: Ready,
    calls: Mutex<HashMap<u64, Collected>>,
    /// Calls whose bytes are only counted and hashed, not kept.
    hashing: Mutex<HashMap<u64, (u64, sha2::Sha256)>>,
    pub revoked: Mutex<Option<String>>,
}

pub fn client_config(url: &str, credential: &str) -> ClientConfig {
    let mut config = ClientConfig::new(url, credential);
    config.attributes = serde_json::json!({"role": "client"});
    config.timing = fast_timing();
    config
}

/// A client that speaks protocol 2: resumable uploads in a window (RP-15).
pub fn client_config_v2(url: &str, credential: &str) -> ClientConfig {
    let mut config = client_config(url, credential);
    config.attributes = serde_json::json!({"role": "client", "protocol": 2});
    config
}

/// A stream an upload reads from: `read(offset, buffer)` fills the buffer
/// with the stream's bytes from `offset`.
pub trait Source {
    fn size(&self) -> u64;
    fn sha256(&self) -> String;
    fn read(&self, offset: u64, buffer: &mut [u8]);
}

impl Source for [u8] {
    fn size(&self) -> u64 {
        self.len() as u64
    }
    fn sha256(&self) -> String {
        guest_actions::sha256_hex(self)
    }
    fn read(&self, offset: u64, buffer: &mut [u8]) {
        let at = offset as usize;
        buffer.copy_from_slice(&self[at..at + buffer.len()]);
    }
}

/// How a protocol-2 upload of one call went.
#[derive(Debug, PartialEq)]
pub enum Streamed {
    /// Every byte and the end went out; the call's chunks started here.
    Sent { from: u64 },
    /// The bytes up to `at` went out and the sender stopped, as a lost
    /// client would.
    Stopped { from: u64, at: u64 },
    /// The call's result came before its stream ended.
    Answered { from: u64 },
}

impl TestClient {
    pub fn connect(url: &str, credential: &str) -> Result<TestClient, remote_session::Error> {
        Self::connect_with(client_config(url, credential))
    }

    pub fn connect_with(config: ClientConfig) -> Result<TestClient, remote_session::Error> {
        let session = remote_session::connect(config)?;
        let body = session
            .recv_timeout(Duration::from_secs(10))?
            .expect("a ready message");
        let ready = match Message::decode(&body).unwrap() {
            Message::Ready(ready) => ready,
            other => panic!("expected ready, got {other:?}"),
        };
        Ok(TestClient {
            session,
            ready,
            calls: Mutex::default(),
            hashing: Mutex::default(),
            revoked: Mutex::default(),
        })
    }

    pub fn send(&self, message: &Message) {
        self.session.send(message.encode()).unwrap();
    }

    /// Upload `source` under protocol 2 as call `call`, in the window beyond
    /// the agent's progress. A call that resumes waits for the agent's
    /// committed offset and continues there. With `stop_at`, the sender
    /// stops once that many bytes of the stream went out, without its end.
    pub fn stream(
        &self,
        call: u64,
        execution: &str,
        op: Op,
        source: &(impl Source + ?Sized),
        resume: bool,
        stop_at: Option<u64>,
    ) -> Streamed {
        self.send(&Message::Call(Call {
            lease: None,
            resume,
            offset: 0,
            call,
            execution: execution.into(),
            op,
        }));
        let progressed = |client: &TestClient| {
            let calls = client.calls.lock().unwrap();
            let seen = calls.get(&call);
            (
                seen.and_then(|c| c.progress.last().copied()),
                seen.is_some_and(|c| c.result.is_some()),
            )
        };
        let from = if resume {
            loop {
                match progressed(self) {
                    (_, true) => return Streamed::Answered { from: 0 },
                    (Some(at), _) => break at,
                    _ => {
                        self.pump(Duration::from_millis(200)).unwrap();
                    }
                }
            }
        } else {
            0
        };
        let size = source.size();
        let mut sent = from;
        let mut buffer = vec![0u8; MAX_CHUNK];
        while sent < size {
            if let Some(stop) = stop_at {
                if sent >= stop {
                    return Streamed::Stopped { from, at: sent };
                }
            }
            loop {
                let (committed, answered) = progressed(self);
                if answered {
                    return Streamed::Answered { from };
                }
                if sent < committed.unwrap_or(from) + UPLOAD_WINDOW {
                    break;
                }
                self.pump(Duration::from_millis(200)).unwrap();
            }
            while self.pump(Duration::ZERO).unwrap() {}
            let mut want = (size - sent).min(MAX_CHUNK as u64);
            if let Some(stop) = stop_at {
                want = want.min(stop - sent);
            }
            let piece = &mut buffer[..want as usize];
            source.read(sent, piece);
            self.send(&Message::Chunk {
                call,
                data: piece.to_vec(),
            });
            sent += want;
        }
        if stop_at.is_some() {
            return Streamed::Stopped { from, at: sent };
        }
        self.send(&Message::End {
            call,
            execution: execution.into(),
            size,
            sha256: source.sha256(),
        });
        Streamed::Sent { from }
    }

    /// Send a call with its upload and end, when its operation uploads.
    pub fn call(&self, call: u64, execution: &str, op: Op, upload: &[u8]) {
        let uploads = op.kind().uploads();
        self.send(&Message::Call(relay_agent::protocol::Call {
            lease: None,
            resume: false,
            offset: 0,
            call,
            execution: execution.into(),
            op,
        }));
        if uploads {
            for data in upload.chunks(relay_agent::protocol::MAX_CHUNK) {
                self.send(&Message::Chunk {
                    call,
                    data: data.to_vec(),
                });
            }
            self.send(&Message::End {
                call,
                execution: execution.into(),
                size: upload.len() as u64,
                sha256: guest_actions::sha256_hex(upload),
            });
        }
    }

    /// Count and hash `call`'s bytes instead of keeping them.
    pub fn hash_only(&self, call: u64) {
        self.hashing
            .lock()
            .unwrap()
            .insert(call, (0, sha2::Digest::new()));
    }

    /// The size and SHA-256 of a hashed call's bytes.
    pub fn hashed(&self, call: u64) -> (u64, String) {
        let (size, hasher) = self.hashing.lock().unwrap().remove(&call).unwrap();
        (size, guest_actions::hex(&sha2::Digest::finalize(hasher)))
    }

    pub fn query(&self, call: u64, execution: &str) {
        self.send(&Message::Query {
            call,
            execution: execution.into(),
        });
    }

    pub fn pump(&self, timeout: Duration) -> Result<bool, remote_session::Error> {
        let Some(body) = self.session.recv_timeout(timeout)? else {
            return Ok(false);
        };
        let mut calls = self.calls.lock().unwrap();
        match Message::decode(&body).unwrap() {
            Message::Chunk { call, data } => {
                let mut hashing = self.hashing.lock().unwrap();
                match hashing.get_mut(&call) {
                    Some((size, hasher)) => {
                        *size += data.len() as u64;
                        sha2::Digest::update(hasher, &data);
                    }
                    None => calls.entry(call).or_default().bytes.extend(data),
                }
            }
            Message::End {
                call, size, sha256, ..
            } => calls.entry(call).or_default().end = Some(Digest { size, sha256 }),
            Message::Result(result) => {
                let call = result.call;
                calls.entry(call).or_default().result = Some(result)
            }
            Message::QueryResult { call, state, .. } => {
                calls.entry(call).or_default().query = Some(state)
            }
            Message::Pending(pending) => {
                calls.entry(pending.call).or_default().pending.push(pending)
            }
            Message::Progress {
                call, committed, ..
            } => calls.entry(call).or_default().progress.push(committed),
            Message::Revoked { reason } => *self.revoked.lock().unwrap() = Some(reason),
            other => panic!("unexpected message {other:?}"),
        }
        Ok(true)
    }

    /// Wait for the result (or query answer) of `call`.
    pub fn wait(&self, call: u64, timeout: Duration) -> Collected {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let mut calls = self.calls.lock().unwrap();
                let done = calls
                    .get(&call)
                    .is_some_and(|c| c.result.is_some() || c.query.is_some());
                if done {
                    return calls.remove(&call).unwrap();
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "no answer to call {call} in {timeout:?}");
            if let Err(error) = self.pump(left.min(Duration::from_millis(200))) {
                panic!("session failed while waiting for call {call}: {error}");
            }
        }
    }

    /// Read until the session ends or `timeout` passes; returns whether it
    /// ended.
    pub fn wait_end(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.pump(Duration::from_millis(100)).is_err() {
                return true;
            }
        }
        false
    }

    /// What arrived for `call` so far.
    pub fn seen(&self, call: u64) -> Collected {
        let _ = self.pump(Duration::from_millis(50));
        self.calls
            .lock()
            .unwrap()
            .get(&call)
            .cloned()
            .unwrap_or_default()
    }
}

pub fn exec(command: &str) -> Op {
    Op::Exec {
        command: command.into(),
        stdin: false,
        timeout_s: 60.0,
    }
}

pub fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    condition()
}
