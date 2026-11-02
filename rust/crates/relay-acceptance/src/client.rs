//! A Rust client of the relay protocol (`docs/arch/relay-protocol.md`), as a
//! real client of TL-7: it dials the instance's advertised `relay_url`
//! itself, through whatever the network between it and the broker is.
//!
//! One reader thread takes every message the broker sends and files it under
//! its call; a call can keep its bytes or only hash them, so a 2 GB read
//! never sits in memory. The client keeps the session's link events with
//! their times, so a scenario can tell how often and how fast it resumed, and
//! counts every `result` per call number, so a result that arrives twice is
//! seen.

use std::collections::HashMap;
use std::io::Read;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use relay_agent::protocol::{
    Call, CallResult, ExecutionState, Message, Op, Outcome, Pending, Ready, MAX_CHUNK,
};
use remote_session::{ClientConfig, LinkEvent, Resolver, Session, SessionEnd, Timing};
use sha2::{Digest as _, Sha256};

/// How a client reaches the broker.
#[derive(Clone)]
pub struct ClientOptions {
    /// The broker's URL, as discovery or the acquisition answered it.
    pub url: String,
    /// The bearer credential.
    pub credential: String,
    /// `client`, or `agent` for a scenario that checks an agent credential.
    pub role: String,
    /// Authorities to trust beside the public roots, PEM.
    pub roots_pem: Vec<String>,
    /// Where the URL's host name leads on this network: the rig's chain.
    pub resolve_to: Option<SocketAddr>,
    /// A shorter session grace period to ask for (RS-5).
    pub grace_ms: Option<u64>,
    /// The session's timing.
    pub timing: Timing,
}

impl ClientOptions {
    fn config(&self) -> Result<ClientConfig, String> {
        let roots: Vec<&[u8]> = self.roots_pem.iter().map(|pem| pem.as_bytes()).collect();
        let tls = remote_session::tls::client_config(&roots).map_err(|e| e.to_string())?;
        let mut config = ClientConfig::new(&self.url, &self.credential);
        let mut attributes = serde_json::json!({"role": self.role});
        if let Some(grace) = self.grace_ms {
            attributes["grace_ms"] = serde_json::json!(grace);
        }
        config.attributes = attributes;
        config.tls = Some(tls);
        config.timing = self.timing.clone();
        if let Some(chain) = self.resolve_to {
            let resolver: Resolver = Arc::new(move |_host: &str, _port: u16| Ok(vec![chain]));
            config.resolver = Some(resolver);
        }
        Ok(config)
    }
}

/// Everything one call received.
#[derive(Debug, Clone, Default)]
pub struct Answer {
    /// The bytes, when the call keeps them.
    pub bytes: Vec<u8>,
    /// How many bytes arrived.
    pub size: u64,
    /// The SHA-256 of the bytes that arrived, hex.
    pub sha256: String,
    /// The stream's `end`.
    pub end: Option<(u64, String)>,
    /// The call's result.
    pub result: Option<CallResult>,
    /// A query's answer.
    pub query: Option<ExecutionState>,
    /// Every `pending` the broker sent.
    pub pending: Vec<Pending>,
    /// When each chunk arrived, from the call's start, and its length.
    pub chunks: Vec<(Duration, usize)>,
    /// How long the call took until its result.
    pub elapsed: Duration,
}

impl Answer {
    /// Whether the bytes match the stream's `end`.
    pub fn end_matches(&self) -> bool {
        self.end
            .as_ref()
            .is_some_and(|(size, sha)| *size == self.size && *sha == self.sha256)
    }

    /// The outcome, or why there is none.
    pub fn outcome(&self) -> Result<&Outcome, String> {
        self.result
            .as_ref()
            .map(|r| &r.outcome)
            .ok_or_else(|| "no result".to_string())
    }

    /// The exit status of an exec or script, or why it has none.
    pub fn rc(&self) -> Result<Option<i32>, String> {
        match self.outcome()? {
            Outcome::Exit { rc, .. } => Ok(*rc),
            other => Err(format!("not an exit: {other:?}")),
        }
    }

    /// The kept bytes as text.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
}

struct Open {
    keep: bool,
    started: Instant,
    hasher: Sha256,
    answer: Answer,
}

#[derive(Default)]
struct Calls {
    open: HashMap<u64, Open>,
    done: HashMap<u64, Answer>,
    results: HashMap<u64, u32>,
    revoked: Option<String>,
    ended: Option<String>,
}

struct Shared {
    calls: Mutex<Calls>,
    changed: Condvar,
    links: Mutex<Vec<(Duration, LinkEvent)>>,
}

/// A client session of the relay protocol.
pub struct RelayClient {
    session: Session,
    /// The broker's `ready`.
    pub ready: Ready,
    shared: Arc<Shared>,
    next: AtomicU64,
    opened: Instant,
    reader: Option<JoinHandle<()>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl RelayClient {
    /// Open a session and take the broker's `ready`.
    pub fn connect(options: &ClientOptions) -> Result<RelayClient, String> {
        let session = remote_session::connect(options.config()?).map_err(|e| e.to_string())?;
        let opened = Instant::now();
        let body = session
            .recv_timeout(Duration::from_secs(60))
            .map_err(|e| e.to_string())?
            .ok_or("no ready message in 60 s")?;
        let ready = match Message::decode(&body).map_err(|e| e.to_string())? {
            Message::Ready(ready) => ready,
            other => return Err(format!("expected ready, got {other:?}")),
        };
        let shared = Arc::new(Shared {
            calls: Mutex::default(),
            changed: Condvar::new(),
            links: Mutex::default(),
        });
        if let Some(events) = session.take_events() {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || {
                for event in events {
                    lock(&shared.links).push((opened.elapsed(), event));
                }
            });
        }
        let reader = {
            let session = session.clone();
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || read_loop(session, shared))
        };
        Ok(RelayClient {
            session,
            ready,
            shared,
            next: AtomicU64::new(1),
            opened,
            reader: Some(reader),
        })
    }

    /// The session.
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// The session's link events so far, with their times from its start.
    pub fn links(&self) -> Vec<(Duration, LinkEvent)> {
        lock(&self.shared.links).clone()
    }

    /// How many times the session resumed on a new connection.
    pub fn resumes(&self) -> usize {
        self.links()
            .iter()
            .filter(|(_, e)| matches!(e, LinkEvent::Connected { resumed: true, .. }))
            .count()
    }

    /// The `revoked` reason, once the broker sent one.
    pub fn revoked(&self) -> Option<String> {
        lock(&self.shared.calls).revoked.clone()
    }

    /// Why the session ended, once it did.
    pub fn ended(&self) -> Option<String> {
        lock(&self.shared.calls).ended.clone()
    }

    /// How many `result` messages arrived for a call number.
    pub fn results_for(&self, call: u64) -> u32 {
        lock(&self.shared.calls)
            .results
            .get(&call)
            .copied()
            .unwrap_or(0)
    }

    /// How long the session has been open.
    pub fn age(&self) -> Duration {
        self.opened.elapsed()
    }

    fn open(&self, keep: bool) -> u64 {
        let call = self.next.fetch_add(1, Ordering::SeqCst);
        lock(&self.shared.calls).open.insert(
            call,
            Open {
                keep,
                started: Instant::now(),
                hasher: Sha256::new(),
                answer: Answer::default(),
            },
        );
        call
    }

    fn send(&self, message: &Message) -> Result<(), String> {
        self.session
            .send(message.encode())
            .map(drop)
            .map_err(|e| e.to_string())
    }

    /// Send a call with its upload read from `upload`, and return its number.
    pub fn call_from(
        &self,
        execution: &str,
        op: Op,
        upload: &mut dyn Read,
        keep: bool,
    ) -> Result<u64, String> {
        let uploads = op.kind().uploads();
        let call = self.open(keep);
        self.send(&Message::Call(Call {
            call,
            execution: execution.into(),
            op,
        }))?;
        if uploads {
            let mut hasher = Sha256::new();
            let mut size = 0u64;
            let mut buffer = vec![0u8; MAX_CHUNK];
            loop {
                let mut filled = 0;
                while filled < buffer.len() {
                    match upload.read(&mut buffer[filled..]) {
                        Ok(0) => break,
                        Ok(n) => filled += n,
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(e) => return Err(format!("read the upload: {e}")),
                    }
                }
                if filled == 0 {
                    break;
                }
                hasher.update(&buffer[..filled]);
                size += filled as u64;
                self.send(&Message::Chunk {
                    call,
                    data: buffer[..filled].to_vec(),
                })?;
                if filled < buffer.len() {
                    break;
                }
            }
            self.send(&Message::End {
                call,
                execution: execution.into(),
                size,
                sha256: hex(&hasher.finalize()),
            })?;
        }
        Ok(call)
    }

    /// Send a call with `upload`, and return its number.
    pub fn call(&self, execution: &str, op: Op, upload: &[u8], keep: bool) -> Result<u64, String> {
        self.call_from(execution, op, &mut &upload[..], keep)
    }

    /// Ask what is known of an execution, and return the query's number.
    pub fn send_query(&self, execution: &str) -> Result<u64, String> {
        let call = self.open(false);
        self.send(&Message::Query {
            call,
            execution: execution.into(),
        })?;
        Ok(call)
    }

    /// What arrived for `call` so far, without waiting.
    pub fn seen(&self, call: u64) -> Answer {
        let calls = lock(&self.shared.calls);
        calls
            .open
            .get(&call)
            .map(|open| open.answer.clone())
            .or_else(|| calls.done.get(&call).cloned())
            .unwrap_or_default()
    }

    /// Wait for `call`'s result or query answer.
    pub fn wait(&self, call: u64, timeout: Duration) -> Result<Answer, String> {
        let deadline = Instant::now() + timeout;
        let mut calls = lock(&self.shared.calls);
        loop {
            if let Some(answer) = calls.done.remove(&call) {
                return Ok(answer);
            }
            if let Some(ended) = &calls.ended {
                return Err(format!(
                    "the session ended before call {call} was answered: {ended}"
                ));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(format!("no answer to call {call} in {timeout:?}"));
            }
            calls = self
                .shared
                .changed
                .wait_timeout(calls, left.min(Duration::from_millis(500)))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// Run a command line and wait for it.
    pub fn exec(&self, command: &str, timeout_s: f64, wait: Duration) -> Result<Answer, String> {
        let call = self.call(
            &new_id("exec"),
            exec_op(command, false, timeout_s),
            b"",
            true,
        )?;
        self.wait(call, wait)
    }

    /// Run a command line, fail unless it exits 0, and return its output.
    pub fn run(&self, command: &str) -> Result<String, String> {
        let answer = self.exec(command, 600.0, Duration::from_secs(900))?;
        match answer.rc()? {
            Some(0) => Ok(answer.text()),
            rc => Err(format!(
                "`{command}` exited {rc:?}: {}",
                answer.text().trim()
            )),
        }
    }

    /// Ask about an execution and wait for the answer.
    pub fn query(&self, execution: &str, wait: Duration) -> Result<ExecutionState, String> {
        let call = self.send_query(execution)?;
        self.wait(call, wait)?
            .query
            .ok_or_else(|| "no query answer".to_string())
    }

    /// End the session on purpose.
    pub fn close(mut self) {
        self.session.close();
        let _ = self.session.wait_ended(Duration::from_secs(5));
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl Drop for RelayClient {
    fn drop(&mut self) {
        self.session.close();
    }
}

fn read_loop(session: Session, shared: Arc<Shared>) {
    loop {
        let body = match session.recv_timeout(Duration::from_millis(200)) {
            Ok(Some(body)) => body,
            Ok(None) => continue,
            Err(error) => {
                let why = match session.status().ended {
                    Some(SessionEnd::Expired(reason)) => format!("expired ({reason:?})"),
                    Some(end) => format!("{end:?}"),
                    None => error.to_string(),
                };
                lock(&shared.calls).ended = Some(why);
                shared.changed.notify_all();
                return;
            }
        };
        let message = match Message::decode(&body) {
            Ok(message) => message,
            Err(error) => {
                lock(&shared.calls).ended = Some(format!("an undecodable message: {error}"));
                shared.changed.notify_all();
                return;
            }
        };
        let mut calls = lock(&shared.calls);
        match message {
            Message::Chunk { call, data } => {
                if let Some(open) = calls.open.get_mut(&call) {
                    open.hasher.update(&data);
                    open.answer.size += data.len() as u64;
                    open.answer
                        .chunks
                        .push((open.started.elapsed(), data.len()));
                    if open.keep {
                        open.answer.bytes.extend_from_slice(&data);
                    }
                }
            }
            Message::End {
                call, size, sha256, ..
            } => {
                if let Some(open) = calls.open.get_mut(&call) {
                    open.answer.end = Some((size, sha256));
                }
            }
            Message::Result(result) => {
                let call = result.call;
                *calls.results.entry(call).or_default() += 1;
                if let Some(mut open) = calls.open.remove(&call) {
                    open.answer.sha256 = hex(&open.hasher.finalize());
                    open.answer.elapsed = open.started.elapsed();
                    open.answer.result = Some(result);
                    calls.done.insert(call, open.answer);
                }
            }
            Message::QueryResult { call, state, .. } => {
                if let Some(mut open) = calls.open.remove(&call) {
                    open.answer.elapsed = open.started.elapsed();
                    open.answer.query = Some(state);
                    calls.done.insert(call, open.answer);
                }
            }
            Message::Pending(pending) => {
                if let Some(open) = calls.open.get_mut(&pending.call) {
                    open.answer.pending.push(pending);
                }
            }
            Message::Revoked { reason } => calls.revoked = Some(reason),
            _ => {}
        }
        drop(calls);
        shared.changed.notify_all();
    }
}

/// An exec operation.
pub fn exec_op(command: &str, stdin: bool, timeout_s: f64) -> Op {
    Op::Exec {
        command: command.into(),
        stdin,
        timeout_s,
    }
}

/// A fresh execution ID, named for what it is.
pub fn new_id(what: &str) -> String {
    format!("acc-{what}-{}", uuid::Uuid::new_v4())
}

/// Lower-case hexadecimal.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The SHA-256 of `bytes`, hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// Try a hello and report how the broker answered it: `Ok` with the
/// session's `ready`, or the refusal. A connection that the network loses
/// before the broker answers says nothing about the credential, so the
/// hello is tried again, for up to two minutes, as a client would (TC-2).
pub fn try_hello(options: &ClientOptions) -> Result<Ready, remote_session::Error> {
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        let result = hello_once(options);
        let lost = matches!(
            result,
            Err(remote_session::Error::Connect(_)) | Err(remote_session::Error::Timeout)
        );
        if !lost || std::time::Instant::now() >= deadline {
            return result;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

fn hello_once(options: &ClientOptions) -> Result<Ready, remote_session::Error> {
    let config = options.config().map_err(remote_session::Error::Config)?;
    let session = remote_session::connect(config)?;
    let ready =
        session
            .recv_timeout(Duration::from_secs(30))?
            .and_then(|body| match Message::decode(&body) {
                Ok(Message::Ready(ready)) => Some(ready),
                _ => None,
            });
    session.close();
    ready.ok_or_else(|| remote_session::Error::Protocol("no ready message".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_matches_its_end_only_when_size_and_hash_agree() {
        let mut answer = Answer {
            size: 3,
            sha256: sha256_hex(b"abc"),
            ..Answer::default()
        };
        assert!(!answer.end_matches());
        answer.end = Some((3, sha256_hex(b"abc")));
        assert!(answer.end_matches());
        answer.end = Some((4, sha256_hex(b"abc")));
        assert!(!answer.end_matches());
    }

    #[test]
    fn options_ask_for_the_role_and_a_shorter_grace() {
        let options = ClientOptions {
            url: "wss://broker.example:6241/relay".into(),
            credential: "c".into(),
            role: "client".into(),
            roots_pem: Vec::new(),
            resolve_to: Some("127.0.0.1:9".parse().unwrap()),
            grace_ms: Some(2000),
            timing: Timing::default(),
        };
        let config = options.config().unwrap();
        assert_eq!(config.attributes["role"], "client");
        assert_eq!(config.attributes["grace_ms"], 2000);
        let resolve = config.resolver.unwrap();
        assert_eq!(
            resolve("broker.example", 6241).unwrap(),
            vec!["127.0.0.1:9".parse::<SocketAddr>().unwrap()]
        );
        assert_eq!(new_id("x").len(), "acc-x-".len() + 36);
    }
}
