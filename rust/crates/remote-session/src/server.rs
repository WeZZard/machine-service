//! The server side: accept WebSocket connections at one path, authenticate the
//! hello's bearer credential, create or resume sessions, and expire sessions
//! that stay detached past their grace period (RS-1, RS-2, RS-5, BK-1).
//!
//! Sessions live in memory. A server that restarts has no record of earlier
//! sessions, so it answers their resume with `expired` and reason `unknown`;
//! a session it dropped itself is answered with reason `expired` for an hour.

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rustls::{ServerConnection, StreamOwned};
use tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tungstenite::handshake::HandshakeError;
use tungstenite::http::StatusCode;

use crate::link::{self, Exit, Stream, Ws};
use crate::session::{Session, SessionEnd, Shared, DEFAULT_MAX_BUFFERED};
use crate::table::{Resolve, SessionTable};
use crate::timing::Timing;
use crate::wire::{Control, ExpiryReason, Hello, RejectCode, Welcome, PROTOCOL_VERSION};
use crate::Error;

/// The path the broker serves on machine-service's session listener (BK-1).
pub const RELAY_PATH: &str = "/relay";

/// The environment variable that names the session listener's port (RS-1).
pub const SESSION_PORT_ENV: &str = "MACHINE_SERVICE_SESSION_PORT";

/// The session listener's port when the variable is unset (RS-1).
pub const DEFAULT_SESSION_PORT: u16 = 6241;

/// The default grace period of a detached session (RS-5).
pub const DEFAULT_GRACE: Duration = Duration::from_secs(600);

/// What an authenticator sees of a hello.
#[derive(Debug)]
pub struct AuthRequest<'a> {
    pub credential: &'a str,
    pub attributes: &'a serde_json::Value,
    pub peer: SocketAddr,
    /// The session ID the client asks to resume, if any.
    pub resuming: Option<&'a str>,
}

/// Why an authenticator refused a hello.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    pub code: RejectCode,
    pub message: String,
}

impl Rejection {
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self {
            code: RejectCode::Unauthorized,
            message: message.into(),
        }
    }
}

/// Decides who a hello comes from. It runs on every connection, so a resume
/// is authenticated again, and a session resumes only for the principal that
/// opened it.
pub trait Authenticator: Send + Sync + 'static {
    fn authenticate(&self, request: &AuthRequest<'_>) -> Result<String, Rejection>;
}

impl<F> Authenticator for F
where
    F: Fn(&AuthRequest<'_>) -> Result<String, Rejection> + Send + Sync + 'static,
{
    fn authenticate(&self, request: &AuthRequest<'_>) -> Result<String, Rejection> {
        self(request)
    }
}

/// One bearer token for one principal, compared in constant time.
pub struct StaticToken {
    token: Vec<u8>,
    principal: String,
}

impl StaticToken {
    pub fn new(token: impl Into<String>, principal: impl Into<String>) -> Self {
        Self {
            token: token.into().into_bytes(),
            principal: principal.into(),
        }
    }
}

impl Authenticator for StaticToken {
    fn authenticate(&self, request: &AuthRequest<'_>) -> Result<String, Rejection> {
        let given = request.credential.as_bytes();
        let same = given.len() == self.token.len()
            && given
                .iter()
                .zip(&self.token)
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0;
        same.then(|| self.principal.clone())
            .ok_or_else(|| Rejection::unauthorized("bad credential"))
    }
}

/// How a server serves sessions.
#[derive(Clone)]
pub struct ServerConfig {
    /// The WebSocket path; other paths get 404 (default [`RELAY_PATH`]).
    pub path: String,
    /// TLS; `None` serves plain WebSocket, and only on loopback.
    pub tls: Option<Arc<rustls::ServerConfig>>,
    pub timing: Timing,
    /// How long a detached session waits for its client (default 10 min).
    pub grace: Duration,
    /// Bytes held unacknowledged per session before [`Session::send`] waits.
    pub max_buffered: usize,
    pub authenticator: Arc<dyn Authenticator>,
}

impl ServerConfig {
    pub fn new(authenticator: impl Authenticator) -> Self {
        Self {
            path: RELAY_PATH.to_string(),
            tls: None,
            timing: Timing::default(),
            grace: DEFAULT_GRACE,
            max_buffered: DEFAULT_MAX_BUFFERED,
            authenticator: Arc::new(authenticator),
        }
    }
}

struct ServerInner {
    config: ServerConfig,
    table: Mutex<SessionTable<Arc<Shared>>>,
    incoming_tx: Mutex<Sender<Session>>,
    incoming_rx: Mutex<Receiver<Session>>,
    connections: Mutex<Vec<TcpStream>>,
    shutdown: AtomicBool,
}

/// A session server. Clones share it. It is mounted either by
/// [`SessionServer::listen`] on a listener of its own, or by handing each
/// accepted connection to [`SessionServer::handle_connection`], which is how
/// machine-service's session listener can carry it next to other paths.
#[derive(Clone)]
pub struct SessionServer {
    inner: Arc<ServerInner>,
}

/// A listener thread started by [`SessionServer::listen`].
pub struct Listening {
    addr: SocketAddr,
    thread: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}

impl Listening {
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Stop accepting connections. Live sessions are not touched.
    pub fn stop(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Listening {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

impl SessionServer {
    pub fn new(config: ServerConfig) -> Result<Self, Error> {
        config.timing.validate()?;
        if !config.path.starts_with('/') {
            return Err(Error::Config("path must start with /".into()));
        }
        let (tx, rx) = mpsc::channel();
        let inner = Arc::new(ServerInner {
            table: Mutex::new(SessionTable::new(config.grace)),
            config,
            incoming_tx: Mutex::new(tx),
            incoming_rx: Mutex::new(rx),
            connections: Mutex::new(Vec::new()),
            shutdown: AtomicBool::new(false),
        });
        let weak = Arc::downgrade(&inner);
        std::thread::Builder::new()
            .name("remote-session-reaper".into())
            .spawn(move || reap(weak))
            .map_err(|e| Error::Config(format!("spawn: {e}")))?;
        Ok(Self { inner })
    }

    /// Accept connections on `listener` in a thread of their own. Without TLS
    /// the listener must be on loopback.
    pub fn listen(&self, listener: TcpListener) -> Result<Listening, Error> {
        let addr = listener
            .local_addr()
            .map_err(|e| Error::Config(e.to_string()))?;
        if self.inner.config.tls.is_none() && !addr.ip().is_loopback() {
            return Err(Error::Config(format!(
                "plain WebSocket is allowed only on loopback, not {addr}; configure TLS"
            )));
        }
        listener
            .set_nonblocking(true)
            .map_err(|e| Error::Config(e.to_string()))?;
        let stop = Arc::new(AtomicBool::new(false));
        let server = self.clone();
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name("remote-session-listener".into())
            .spawn(move || {
                while !flag.load(Ordering::SeqCst) && !server.inner.shutdown.load(Ordering::SeqCst)
                {
                    match listener.accept() {
                        Ok((tcp, _)) => {
                            let server = server.clone();
                            let _ = std::thread::Builder::new()
                                .name("remote-session-conn".into())
                                .spawn(move || server.handle_connection(tcp));
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(20)),
                    }
                }
            })
            .map_err(|e| Error::Config(format!("spawn: {e}")))?;
        Ok(Listening {
            addr,
            thread: Some(thread),
            stop,
        })
    }

    /// The next new session. Resumed sessions are not announced again.
    pub fn accept(&self) -> Result<Session, Error> {
        loop {
            if let Some(s) = self.accept_timeout(Duration::from_secs(3600))? {
                return Ok(s);
            }
        }
    }

    pub fn accept_timeout(&self, timeout: Duration) -> Result<Option<Session>, Error> {
        let rx = self
            .inner
            .incoming_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match rx.recv_timeout(timeout) {
            Ok(s) => Ok(Some(s)),
            Err(RecvTimeoutError::Timeout) if !self.is_shut_down() => Ok(None),
            Err(_) => Err(Error::Ended(SessionEnd::Shutdown)),
        }
    }

    fn is_shut_down(&self) -> bool {
        self.inner.shutdown.load(Ordering::SeqCst)
    }

    /// End every session with [`SessionEnd::Shutdown`] and drop every
    /// connection, without a bye, as a crash or restart would.
    pub fn shutdown(&self) {
        self.inner.shutdown.store(true, Ordering::SeqCst);
        let sessions: Vec<Arc<Shared>> = {
            let mut table = self.inner.lock_table();
            let all = table.values().cloned().collect();
            table.retain(|_| false);
            all
        };
        for s in sessions {
            s.finish(SessionEnd::Shutdown);
        }
        for c in self
            .inner
            .connections
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            let _ = c.shutdown(std::net::Shutdown::Both);
        }
    }

    /// Serve one accepted TCP connection on the calling thread until it ends:
    /// TLS, the WebSocket upgrade at the configured path, hello, and the
    /// session's traffic.
    pub fn handle_connection(&self, tcp: TcpStream) {
        let Ok(peer) = tcp.peer_addr() else { return };
        let local_ok = tcp.local_addr().is_ok_and(|a| a.ip().is_loopback());
        let config = &self.inner.config;
        if self.is_shut_down() || (config.tls.is_none() && !local_ok) {
            return;
        }
        let _ = tcp.set_nonblocking(false);
        let _ = tcp.set_nodelay(true);
        if let Ok(clone) = tcp.try_clone() {
            self.inner.track(clone);
        }
        let stream = match &config.tls {
            Some(tls) => match ServerConnection::new(tls.clone()) {
                Ok(conn) => Stream::Server(Box::new(StreamOwned::new(conn, tcp))),
                Err(_) => return,
            },
            None => Stream::Plain(tcp),
        };
        if stream
            .set_timeouts(config.timing.handshake_timeout)
            .is_err()
        {
            return;
        }
        let path = config.path.clone();
        // tungstenite's upgrade callback fixes this error type.
        #[allow(clippy::result_large_err)]
        let check_path = move |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
            if req.uri().path() == path {
                Ok(resp)
            } else {
                let mut not_found = ErrorResponse::new(Some("no such path".into()));
                *not_found.status_mut() = StatusCode::NOT_FOUND;
                Err(not_found)
            }
        };
        let mut ws = match tungstenite::accept_hdr_with_config(
            stream,
            check_path,
            Some(link::ws_config()),
        ) {
            Ok(ws) => ws,
            Err(HandshakeError::Interrupted(_) | HandshakeError::Failure(_)) => return,
        };
        if ws
            .get_ref()
            .set_timeouts(config.timing.poll_interval)
            .is_err()
        {
            return;
        }
        if let Some((shared, generation)) = self.handshake(&mut ws, peer) {
            let exit = link::run(&mut ws, &shared, generation, &config.timing);
            self.after(&shared, generation, exit);
        }
        self.inner.untrack(peer);
    }

    fn refuse(ws: &mut Ws, code: RejectCode, message: impl Into<String>) {
        let _ = link::send_control(
            ws,
            &Control::Error {
                code,
                message: message.into(),
            },
        );
        let _ = ws.close(None);
        let _ = ws.flush();
    }

    /// Answer the hello. Returns the session and this connection's
    /// generation when the connection carries a session.
    fn handshake(&self, ws: &mut Ws, peer: SocketAddr) -> Option<(Arc<Shared>, u64)> {
        let config = &self.inner.config;
        let deadline = Instant::now() + config.timing.handshake_timeout;
        let hello: Hello = match link::read_control(ws, deadline) {
            Ok(Control::Hello(h)) => h,
            Ok(other) => {
                Self::refuse(
                    ws,
                    RejectCode::Protocol,
                    format!("expected hello, got {other:?}"),
                );
                return None;
            }
            Err(_) => return None,
        };
        if hello.version != PROTOCOL_VERSION {
            Self::refuse(
                ws,
                RejectCode::UnsupportedVersion,
                format!("protocol version {} is not served", hello.version),
            );
            return None;
        }
        let principal = match config.authenticator.authenticate(&AuthRequest {
            credential: &hello.credential,
            attributes: &hello.attributes,
            peer,
            resuming: hello.session.as_deref(),
        }) {
            Ok(p) => p,
            Err(r) => {
                Self::refuse(ws, r.code, r.message);
                return None;
            }
        };
        let welcome = |session: &str, resumed: bool, received: u64| {
            Control::Welcome(Welcome {
                session: session.to_string(),
                resumed,
                received,
                heartbeat_ms: millis(config.timing.heartbeat),
                grace_ms: millis(config.grace),
            })
        };
        match hello.session {
            None => {
                if hello.received != 0 {
                    Self::refuse(
                        ws,
                        RejectCode::Protocol,
                        "a new session has received nothing",
                    );
                    return None;
                }
                let id = uuid::Uuid::new_v4().to_string();
                let (shared, events) = Shared::new(id.clone(), config.max_buffered);
                let generation = {
                    let mut inner = shared.lock();
                    let _ = inner.seq.resume(0);
                    inner.attach(false, peer)
                };
                self.inner
                    .lock_table()
                    .insert(id.clone(), principal.clone(), shared.clone());
                if link::send_control(ws, &welcome(&id, false, 0)).is_err() {
                    // The client never learned the ID, so nothing can resume it.
                    self.inner.lock_table().remove(&id);
                    shared.finish(SessionEnd::Closed { by_peer: true });
                    return None;
                }
                let session =
                    Session::new(shared.clone(), events, Some(principal), hello.attributes);
                let _ = self
                    .inner
                    .incoming_tx
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .send(session);
                Some((shared, generation))
            }
            Some(id) => {
                let now = Instant::now();
                let resolved = {
                    let mut table = self.inner.lock_table();
                    let expired = table.expire_due(now);
                    let resolved = match table.resume(&id, &principal) {
                        Resolve::Resumed(shared) => {
                            let mut inner = shared.lock();
                            match inner.seq.resume(hello.received) {
                                _ if inner.end.is_some() => Err(Control::Expired {
                                    session: id.clone(),
                                    reason: ExpiryReason::Expired,
                                }),
                                Ok(()) => {
                                    let received = inner.seq.received();
                                    let generation = inner.attach(true, peer);
                                    drop(inner);
                                    shared.changed.notify_all();
                                    Ok((shared.clone(), generation, received))
                                }
                                Err(e) => Err(Control::Error {
                                    code: RejectCode::Protocol,
                                    message: e.to_string(),
                                }),
                            }
                        }
                        Resolve::Gone(reason) => Err(Control::Expired {
                            session: id.clone(),
                            reason,
                        }),
                        Resolve::Forbidden => Err(Control::Error {
                            code: RejectCode::Forbidden,
                            message: "the session belongs to another principal".into(),
                        }),
                    };
                    drop(table);
                    for s in expired {
                        s.finish(SessionEnd::Expired(ExpiryReason::Expired));
                    }
                    resolved
                };
                match resolved {
                    Ok((shared, generation, received)) => {
                        if link::send_control(ws, &welcome(&id, true, received)).is_err() {
                            self.after(&shared, generation, Exit::Lost("welcome not sent".into()));
                            return None;
                        }
                        Some((shared, generation))
                    }
                    Err(answer) => {
                        let _ = link::send_control(ws, &answer);
                        let _ = ws.close(None);
                        let _ = ws.flush();
                        None
                    }
                }
            }
        }
    }

    /// Settle the session after its connection stopped.
    fn after(&self, shared: &Arc<Shared>, generation: u64, exit: Exit) {
        let end = match exit {
            Exit::Superseded | Exit::Ended => return,
            Exit::LocalClose => Some(SessionEnd::Closed { by_peer: false }),
            Exit::PeerBye => Some(SessionEnd::Closed { by_peer: true }),
            Exit::Lost(_) | Exit::Dead | Exit::Stalled | Exit::Protocol(_) => None,
        };
        let mut table = self.inner.lock_table();
        match end {
            Some(end) => {
                table.remove(&shared.id);
                drop(table);
                shared.finish(end);
            }
            None => {
                if shared.lock().detach(generation, exit.reason()) {
                    table.detach(&shared.id, Instant::now());
                }
            }
        }
    }
}

impl ServerInner {
    fn lock_table(&self) -> std::sync::MutexGuard<'_, SessionTable<Arc<Shared>>> {
        self.table.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn track(&self, tcp: TcpStream) {
        let mut conns = self.connections.lock().unwrap_or_else(|e| e.into_inner());
        conns.push(tcp);
    }

    fn untrack(&self, peer: SocketAddr) {
        let mut conns = self.connections.lock().unwrap_or_else(|e| e.into_inner());
        conns.retain(|c| c.peer_addr().is_ok_and(|p| p != peer));
    }
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// Expire detached sessions and settle sessions closed while detached, until
/// the server is dropped or shut down.
fn reap(server: Weak<ServerInner>) {
    loop {
        let Some(inner) = server.upgrade() else {
            return;
        };
        if inner.shutdown.load(Ordering::SeqCst) {
            return;
        }
        let tick =
            (inner.config.grace / 4).clamp(Duration::from_millis(10), Duration::from_secs(1));
        let (expired, closed) = {
            let mut table = inner.lock_table();
            let expired = table.expire_due(Instant::now());
            let mut closed = Vec::new();
            table.retain(|s| {
                let i = s.lock();
                let done = i.end.is_some() || (i.close_requested && !i.connected);
                if done {
                    closed.push(s.clone());
                }
                !done
            });
            (expired, closed)
        };
        for s in expired {
            s.finish(SessionEnd::Expired(ExpiryReason::Expired));
        }
        for s in closed {
            s.finish(SessionEnd::Closed { by_peer: false });
        }
        drop(inner);
        std::thread::sleep(tick);
    }
}
