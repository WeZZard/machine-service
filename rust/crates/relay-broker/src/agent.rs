//! The broker's end of one relay agent session: send calls with their
//! uploads, and route what the agent sends back to the call that waits for
//! it. Many calls run at once on one session.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Mutex;

use relay_agent::protocol::{
    AgentInfo, Call, CallResult, Digest, ExecutionState, Hello, Message, Op, MAX_CHUNK,
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
        }
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

    /// Send one call with its upload. Events for it arrive on the returned
    /// receiver, which ends with a result or [`AgentEvent::Lost`].
    pub fn start(&self, execution: &str, op: &Op, upload: &[u8]) -> Receiver<AgentEvent> {
        let (call, rx) = self.route();
        let uploads = op.kind().uploads();
        let mut sent = self.send(&Message::Call(Call {
            call,
            execution: execution.to_string(),
            op: op.clone(),
        }));
        if uploads {
            for data in upload.chunks(MAX_CHUNK) {
                sent = sent
                    && self.send(&Message::Chunk {
                        call,
                        data: data.to_vec(),
                    });
            }
            sent = sent
                && self.send(&Message::End {
                    call,
                    execution: execution.to_string(),
                    size: upload.len() as u64,
                    sha256: guest_actions::sha256_hex(upload),
                });
        }
        if !sent {
            self.lose(call);
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
