//! The session handle that both sides hand to their payload layer.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::state::SeqState;
use crate::wire::{ExpiryReason, RejectCode, MAX_BODY};
use crate::Error;

/// The default bound on bytes a side holds unacknowledged before
/// [`Session::send`] waits.
pub const DEFAULT_MAX_BUFFERED: usize = 64 << 20;

/// How a session ended. After the end, no message is sent or received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEnd {
    /// The session is gone; the payload layer starts a new one and asks about
    /// what it had in flight (RS-5).
    Expired(ExpiryReason),
    /// One side closed the session on purpose.
    Closed { by_peer: bool },
    /// The server refused this side's credential on a reconnect.
    Rejected { code: RejectCode, message: String },
    /// The peer broke the protocol on resume, so the session cannot continue.
    Protocol(String),
    /// The local server shut down.
    Shutdown,
}

/// A change in the session's connection, for observation and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkEvent {
    /// A connection carries the session; `resumed` is false for the first one.
    Connected { resumed: bool, peer: SocketAddr },
    /// The connection was lost; a client reconnects and a server waits.
    Disconnected { reason: String },
    /// The session ended.
    Ended(SessionEnd),
}

/// A snapshot of a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub connected: bool,
    pub ended: Option<SessionEnd>,
    /// Bodies sent but not yet acknowledged by the peer.
    pub unacked: usize,
    /// The highest sequence number received in order.
    pub received: u64,
}

pub(crate) struct Inner {
    pub seq: SeqState,
    pub inbox: VecDeque<Vec<u8>>,
    pub end: Option<SessionEnd>,
    pub close_requested: bool,
    /// Bumped for every new connection; a connection loop whose generation is
    /// no longer current stops.
    pub generation: u64,
    pub connected: bool,
    events: Sender<LinkEvent>,
}

impl Inner {
    pub fn emit(&self, event: LinkEvent) {
        let _ = self.events.send(event);
    }

    pub fn finish(&mut self, end: SessionEnd) {
        if self.end.is_none() {
            self.connected = false;
            self.emit(LinkEvent::Ended(end.clone()));
            self.end = Some(end);
        }
    }

    pub fn attach(&mut self, resumed: bool, peer: SocketAddr) -> u64 {
        self.generation += 1;
        self.connected = true;
        self.emit(LinkEvent::Connected { resumed, peer });
        self.generation
    }

    pub fn detach(&mut self, generation: u64, reason: String) -> bool {
        let current = self.generation == generation && self.end.is_none();
        if current && self.connected {
            self.connected = false;
            self.emit(LinkEvent::Disconnected { reason });
        }
        current
    }
}

pub(crate) struct Shared {
    pub id: String,
    pub max_buffered: usize,
    inner: Mutex<Inner>,
    pub changed: Condvar,
}

impl Shared {
    pub fn new(id: String, max_buffered: usize) -> (Arc<Self>, Receiver<LinkEvent>) {
        let (events, rx) = mpsc::channel();
        let shared = Arc::new(Self {
            id,
            max_buffered,
            inner: Mutex::new(Inner {
                seq: SeqState::default(),
                inbox: VecDeque::new(),
                end: None,
                close_requested: false,
                generation: 0,
                connected: false,
                events,
            }),
            changed: Condvar::new(),
        });
        (shared, rx)
    }

    pub fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn finish(&self, end: SessionEnd) {
        self.lock().finish(end);
        self.changed.notify_all();
    }

    /// Wait up to `timeout` or until the session changes.
    pub fn wait<'a>(
        &self,
        guard: MutexGuard<'a, Inner>,
        timeout: Duration,
    ) -> MutexGuard<'a, Inner> {
        self.changed
            .wait_timeout(guard, timeout)
            .unwrap_or_else(|e| e.into_inner())
            .0
    }
}

struct Handle {
    shared: Arc<Shared>,
    principal: Option<String>,
    attributes: serde_json::Value,
    events: Mutex<Option<Receiver<LinkEvent>>>,
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.shared.lock().close_requested = true;
        self.shared.changed.notify_all();
    }
}

/// One end of a resumable session: an ordered, reliable, message-oriented
/// duplex link with opaque bodies, which survives broken connections.
///
/// Clones share the session, so one thread may send while another receives.
/// Dropping the last clone closes the session.
#[derive(Clone)]
pub struct Session {
    handle: Arc<Handle>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("id", &self.handle.shared.id)
            .field("principal", &self.handle.principal)
            .finish()
    }
}

impl Session {
    pub(crate) fn new(
        shared: Arc<Shared>,
        events: Receiver<LinkEvent>,
        principal: Option<String>,
        attributes: serde_json::Value,
    ) -> Self {
        Self {
            handle: Arc::new(Handle {
                shared,
                principal,
                attributes,
                events: Mutex::new(Some(events)),
            }),
        }
    }

    /// The session ID, which identifies the session across connections (TC-4).
    pub fn id(&self) -> &str {
        &self.handle.shared.id
    }

    /// On a server, who the authenticator said the client is.
    pub fn principal(&self) -> Option<&str> {
        self.handle.principal.as_deref()
    }

    /// On a server, the attributes the client's first hello carried, such as
    /// the role of BK-1.
    pub fn attributes(&self) -> &serde_json::Value {
        &self.handle.attributes
    }

    /// Queue `body` for ordered, exactly-once delivery, and return its
    /// sequence number. Waits while the replay buffer is full.
    pub fn send(&self, body: Vec<u8>) -> Result<u64, Error> {
        self.send_inner(body, None)
    }

    /// Like [`Self::send`], but fails with [`Error::BufferFull`] instead of
    /// waiting.
    pub fn try_send(&self, body: Vec<u8>) -> Result<u64, Error> {
        self.send_inner(body, Some(Instant::now()))
    }

    fn send_inner(&self, body: Vec<u8>, deadline: Option<Instant>) -> Result<u64, Error> {
        if body.len() > MAX_BODY {
            return Err(Error::TooLarge {
                size: body.len(),
                max: MAX_BODY,
            });
        }
        let shared = &self.handle.shared;
        let mut inner = shared.lock();
        loop {
            if let Some(end) = &inner.end {
                return Err(Error::Ended(end.clone()));
            }
            let fits = inner.seq.outbox_bytes() + body.len() <= shared.max_buffered;
            if fits || inner.seq.unacked() == 0 {
                let seq = inner.seq.push(body);
                drop(inner);
                shared.changed.notify_all();
                return Ok(seq);
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                return Err(Error::BufferFull);
            }
            inner = shared.wait(inner, Duration::from_secs(1));
        }
    }

    /// The next body from the peer, waiting as long as it takes. Bodies that
    /// arrived before the session ended are still returned.
    pub fn recv(&self) -> Result<Vec<u8>, Error> {
        loop {
            if let Some(body) = self.recv_timeout(Duration::from_secs(3600))? {
                return Ok(body);
            }
        }
    }

    /// The next body from the peer, or `None` after `timeout`.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Option<Vec<u8>>, Error> {
        let shared = &self.handle.shared;
        let deadline = Instant::now() + timeout;
        let mut inner = shared.lock();
        loop {
            if let Some(body) = inner.inbox.pop_front() {
                return Ok(Some(body));
            }
            if let Some(end) = &inner.end {
                return Err(Error::Ended(end.clone()));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            inner = shared.wait(inner, deadline - now);
        }
    }

    /// Wait until the peer acknowledged every body sent so far.
    pub fn flush(&self, timeout: Duration) -> Result<(), Error> {
        let shared = &self.handle.shared;
        let deadline = Instant::now() + timeout;
        let mut inner = shared.lock();
        loop {
            if inner.seq.unacked() == 0 {
                return Ok(());
            }
            if let Some(end) = &inner.end {
                return Err(Error::Ended(end.clone()));
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(Error::Timeout);
            }
            inner = shared.wait(inner, deadline - now);
        }
    }

    /// Close the session on purpose: neither side resumes it. A connected
    /// side sends `bye`, so the peer ends at once; a side closed while
    /// disconnected ends alone, and the peer's session expires after its
    /// grace period. Unacknowledged bodies are dropped; call [`Self::flush`]
    /// first to keep them.
    pub fn close(&self) {
        let shared = &self.handle.shared;
        shared.lock().close_requested = true;
        shared.changed.notify_all();
    }

    /// Wait until the session ended, or `timeout` passed.
    pub fn wait_ended(&self, timeout: Duration) -> Option<SessionEnd> {
        let shared = &self.handle.shared;
        let deadline = Instant::now() + timeout;
        let mut inner = shared.lock();
        loop {
            if let Some(end) = &inner.end {
                return Some(end.clone());
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            inner = shared.wait(inner, deadline - now);
        }
    }

    pub fn status(&self) -> Status {
        let inner = self.handle.shared.lock();
        Status {
            connected: inner.connected,
            ended: inner.end.clone(),
            unacked: inner.seq.unacked(),
            received: inner.seq.received(),
        }
    }

    /// The session's connection events. The first call takes the receiver;
    /// later calls return `None`.
    pub fn take_events(&self) -> Option<Receiver<LinkEvent>> {
        self.handle
            .events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }
}
