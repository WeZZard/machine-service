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
use relay_agent::protocol::{CallResult, Digest, ExecutionState, Message, Op, Pending, Ready};
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
}

impl Default for Options {
    fn default() -> Self {
        Options {
            grace: Duration::from_secs(30),
            agent_wait: Duration::from_secs(10),
            tls: None,
            port: 0,
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
    pub revoked: Mutex<Option<String>>,
}

pub fn client_config(url: &str, credential: &str) -> ClientConfig {
    let mut config = ClientConfig::new(url, credential);
    config.attributes = serde_json::json!({"role": "client"});
    config.timing = fast_timing();
    config
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
            revoked: Mutex::default(),
        })
    }

    pub fn send(&self, message: &Message) {
        self.session.send(message.encode()).unwrap();
    }

    /// Send a call with its upload and end, when its operation uploads.
    pub fn call(&self, call: u64, execution: &str, op: Op, upload: &[u8]) {
        let uploads = op.kind().uploads();
        self.send(&Message::Call(relay_agent::protocol::Call {
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

    pub fn query(&self, call: u64, execution: &str) {
        self.send(&Message::Query {
            call,
            execution: execution.into(),
        });
    }

    fn pump(&self, timeout: Duration) -> Result<bool, remote_session::Error> {
        let Some(body) = self.session.recv_timeout(timeout)? else {
            return Ok(false);
        };
        let mut calls = self.calls.lock().unwrap();
        match Message::decode(&body).unwrap() {
            Message::Chunk { call, data } => calls.entry(call).or_default().bytes.extend(data),
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
