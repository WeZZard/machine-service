//! The broker's end of one relay agent session: send calls, their streams
//! and acknowledgements, and route what the agent sends back to the call
//! that waits for it. Many calls run at once on one session.
//!
//! The session speaks the lower of the agent's protocol, from its hello,
//! and the broker's, which the broker announces with `ready` right after
//! the hello. Protocol 2's fields and `progress` never reach an agent of
//! protocol 1.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Mutex;

use relay_agent::protocol::{
    AgentInfo, Call, CallResult, Digest, ExecutionState, Hello, Message, Op, Ready, PROTOCOL,
};
use remote_session::Session;

use crate::credentials::Target;

/// What the agent sent for one call or query.
#[derive(Debug)]
pub(crate) enum AgentEvent {
    Chunk(Vec<u8>),
    End(Digest),
    Result(CallResult),
    Query(ExecutionState),
    /// Bytes of the call's upload the agent holds (protocol 2).
    Progress(u64),
    /// The session ended before the call's result.
    Lost,
}

pub(crate) struct AgentConn {
    pub id: u64,
    pub target: Target,
    pub session: Session,
    pub hello: Hello,
    next_call: AtomicU64,
    routes: Mutex<HashMap<u64, Sender<AgentEvent>>>,
    alive: AtomicBool,
}

impl AgentConn {
    pub fn new(id: u64, target: Target, session: Session, hello: Hello) -> AgentConn {
        AgentConn {
            id,
            target,
            session,
            hello,
            next_call: AtomicU64::new(1),
            routes: Mutex::new(HashMap::new()),
            alive: AtomicBool::new(true),
        }
    }

    pub fn info(&self) -> AgentInfo {
        AgentInfo {
            os: self.hello.os.clone(),
            arch: self.hello.arch.clone(),
            build: self.hello.build.clone(),
            protocol: self.hello.protocol,
            signing: self.hello.signing.clone(),
        }
    }

    /// Whether the session speaks protocol 2: resumable uploads, stream
    /// offsets and `progress`.
    pub fn v2(&self) -> bool {
        self.hello.protocol.min(PROTOCOL) >= 2
    }

    /// Tell the agent the broker's protocol; the first message the broker
    /// sends on the session.
    pub fn announce(&self) -> bool {
        self.send(&Message::Ready(Ready {
            protocol: PROTOCOL,
            lease: self.target.name().to_string(),
            agent: None,
        }))
    }

    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    fn routes(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Sender<AgentEvent>>> {
        self.routes.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn route(&self) -> (u64, Receiver<AgentEvent>) {
        let call = self.next_call.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        if self.alive() {
            self.routes().insert(call, tx);
        } else {
            let _ = tx.send(AgentEvent::Lost);
        }
        (call, rx)
    }

    fn send(&self, message: &Message) -> bool {
        self.session.send(message.encode()).is_ok()
    }

    /// Send a call of `lease`, without its stream. `resume` and `offset`
    /// reach an agent of protocol 2 only. Events for the call arrive on the
    /// returned receiver, which ends with a result or [`AgentEvent::Lost`].
    pub fn open(
        &self,
        execution: &str,
        op: &Op,
        lease: &str,
        resume: bool,
        offset: u64,
    ) -> (u64, Receiver<AgentEvent>) {
        let (call, rx) = self.route();
        let v2 = self.v2();
        let sent = self.send(&Message::Call(Call {
            call,
            execution: execution.to_string(),
            op: op.clone(),
            lease: Some(lease.to_string()),
            resume: resume && v2,
            offset: if v2 { offset } else { 0 },
        }));
        if !sent {
            self.lose(call);
        }
        (call, rx)
    }

    /// Send the next bytes of `call`'s upload; false when the session ended.
    pub fn chunk(&self, call: u64, data: Vec<u8>) -> bool {
        let sent = self.send(&Message::Chunk { call, data });
        if !sent {
            self.lose(call);
        }
        sent
    }

    /// Send the end of `call`'s upload, naming the whole stream.
    pub fn end(&self, call: u64, execution: &str, end: &Digest) -> bool {
        let sent = self.send(&Message::End {
            call,
            execution: execution.to_string(),
            size: end.size,
            sha256: end.sha256.clone(),
        });
        if !sent {
            self.lose(call);
        }
        sent
    }

    /// Acknowledge `committed` bytes of the stream the agent sends for
    /// `call` (protocol 2).
    pub fn ack(&self, call: u64, execution: &str, committed: u64) {
        if self.v2() {
            let _ = self.send(&Message::Progress {
                call,
                execution: execution.to_string(),
                committed,
            });
        }
    }

    /// Send one call of `lease` that uploads nothing.
    pub fn start(&self, execution: &str, op: &Op, lease: &str) -> Receiver<AgentEvent> {
        let (call, rx) = self.open(execution, op, lease, false, 0);
        if op.kind().uploads() {
            self.end(
                call,
                execution,
                &Digest {
                    size: 0,
                    sha256: guest_actions::sha256_hex(b""),
                },
            );
        }
        rx
    }

    /// Ask what the agent knows of `execution`.
    pub fn query(&self, execution: &str) -> Receiver<AgentEvent> {
        let (call, rx) = self.route();
        if !self.send(&Message::Query {
            call,
            execution: execution.to_string(),
        }) {
            self.lose(call);
        }
        rx
    }

    /// Stop routing `call`: nobody waits for it any more.
    pub fn forget(&self, call: u64) {
        self.routes().remove(&call);
    }

    fn lose(&self, call: u64) {
        if let Some(tx) = self.routes().remove(&call) {
            let _ = tx.send(AgentEvent::Lost);
        }
    }

    fn deliver(&self, call: u64, event: AgentEvent, last: bool) {
        let mut routes = self.routes();
        let tx = if last {
            routes.remove(&call)
        } else {
            routes.get(&call).cloned()
        };
        drop(routes);
        if let Some(tx) = tx {
            let _ = tx.send(event);
        }
    }

    /// Read the session until it ends, routing each message, then tell every
    /// call still waiting that it was lost.
    pub fn read_until_end(&self, log: &dyn Fn(&str)) {
        while let Ok(body) = self.session.recv() {
            match Message::decode(&body) {
                Ok(Message::Chunk { call, data }) => {
                    self.deliver(call, AgentEvent::Chunk(data), false)
                }
                Ok(Message::End {
                    call, size, sha256, ..
                }) => self.deliver(call, AgentEvent::End(Digest { size, sha256 }), false),
                Ok(Message::Progress {
                    call, committed, ..
                }) => self.deliver(call, AgentEvent::Progress(committed), false),
                Ok(Message::Result(result)) => {
                    let call = result.call;
                    self.deliver(call, AgentEvent::Result(result), true)
                }
                Ok(Message::QueryResult { call, state, .. }) => {
                    self.deliver(call, AgentEvent::Query(state), true)
                }
                Ok(Message::Heartbeat) => {}
                Ok(other) => log(&format!(
                    "relay: {}: ignoring an unexpected message from the agent: {other:?}",
                    self.target
                )),
                Err(error) => {
                    log(&format!(
                        "relay: {}: closing the agent session: {error}",
                        self.target
                    ));
                    self.session.close();
                    break;
                }
            }
        }
        self.alive.store(false, Ordering::SeqCst);
        let routes: Vec<Sender<AgentEvent>> = self.routes().drain().map(|(_, tx)| tx).collect();
        for tx in routes {
            let _ = tx.send(AgentEvent::Lost);
        }
    }

    /// End the session; its reader then reports every waiting call lost.
    pub fn close(&self) {
        self.alive.store(false, Ordering::SeqCst);
        self.session.close();
    }
}

/// Wait for an agent's hello, the first message of its session.
pub(crate) fn read_hello(session: &Session, timeout: std::time::Duration) -> Result<Hello, String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Err("no hello in time".into());
        }
        match session.recv_timeout(left) {
            Ok(Some(body)) => match Message::decode(&body) {
                Ok(Message::Hello(hello)) => return Ok(hello),
                Ok(Message::Heartbeat) => {}
                Ok(other) => return Err(format!("expected a hello, got {other:?}")),
                Err(error) => return Err(error.to_string()),
            },
            Ok(None) => return Err("no hello in time".into()),
            Err(error) => return Err(error.to_string()),
        }
    }
}
