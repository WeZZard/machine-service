//! One connection carrying a session: the stream under the WebSocket, the
//! handshake helpers, and the loop that moves envelopes, acknowledgements and
//! heartbeats until the connection ends.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use rustls::{ClientConnection, ServerConnection, StreamOwned};
use tungstenite::protocol::WebSocketConfig;
use tungstenite::{Message, WebSocket};

use crate::flow::{FlowWindow, RttMeter};
use crate::session::Shared;
use crate::state::{Accepted, ProtocolError};
use crate::timing::Timing;
use crate::wire::{Control, Envelope, MAX_MESSAGE};

/// The stream under a WebSocket: plain TCP on loopback, or TLS.
pub(crate) enum Stream {
    Plain(TcpStream),
    Client(Box<StreamOwned<ClientConnection, TcpStream>>),
    Server(Box<StreamOwned<ServerConnection, TcpStream>>),
}

impl Stream {
    pub fn tcp(&self) -> &TcpStream {
        match self {
            Stream::Plain(s) => s,
            Stream::Client(s) => &s.sock,
            Stream::Server(s) => &s.sock,
        }
    }

    pub fn set_timeouts(&self, timeout: Duration) -> io::Result<()> {
        self.tcp().set_read_timeout(Some(timeout))?;
        self.tcp().set_write_timeout(Some(timeout))
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read(buf),
            Stream::Client(s) => s.read(buf),
            Stream::Server(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.write(buf),
            Stream::Client(s) => s.write(buf),
            Stream::Server(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Plain(s) => s.flush(),
            Stream::Client(s) => s.flush(),
            Stream::Server(s) => s.flush(),
        }
    }
}

pub(crate) type Ws = WebSocket<Stream>;

/// The cap on bytes queued in the WebSocket's write buffer before the loop
/// stops taking envelopes from the session.
const MAX_WRITE_BUFFER: usize = 4 * MAX_MESSAGE;

pub(crate) fn ws_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE))
        .max_write_buffer_size(MAX_WRITE_BUFFER)
}

pub(crate) fn would_block(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(io) if matches!(io.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut))
}

pub(crate) fn send_control(ws: &mut Ws, control: &Control) -> tungstenite::Result<()> {
    ws.send(Message::Text(control.to_text().into()))
}

/// Read the next control message before `deadline`, with the socket's read
/// timeout set to a short poll interval.
pub(crate) fn read_control(ws: &mut Ws, deadline: Instant) -> Result<Control, String> {
    loop {
        if Instant::now() >= deadline {
            return Err("handshake timed out".into());
        }
        match ws.read() {
            Ok(Message::Text(text)) => return Control::parse(&text).map_err(|e| e.to_string()),
            Ok(Message::Close(_)) => return Err("connection closed during handshake".into()),
            Ok(Message::Binary(_)) => return Err("envelope before handshake".into()),
            Ok(_) => {}
            Err(e) if would_block(&e) => {}
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// Why a connection loop stopped.
#[derive(Debug)]
pub(crate) enum Exit {
    /// The connection broke or the peer closed the WebSocket.
    Lost(String),
    /// Nothing arrived for `dead_after`.
    Dead,
    /// Bodies were in flight on this connection and the peer's
    /// acknowledgement did not advance for `dead_after`, although the peer
    /// was heard from. The flow window keeps a healthy link far from this.
    Stalled,
    /// A newer connection took over the session.
    Superseded,
    /// The session ended elsewhere, for example by expiry or shutdown.
    Ended,
    /// This side closed the session; a bye went out.
    LocalClose,
    /// The peer closed the session with a bye.
    PeerBye,
    /// The peer broke the protocol.
    Protocol(ProtocolError),
}

impl Exit {
    pub fn reason(&self) -> String {
        match self {
            Exit::Lost(e) => format!("connection lost: {e}"),
            Exit::Dead => "no traffic within the dead-connection bound".into(),
            Exit::Stalled => "no acknowledgement within the dead-connection bound".into(),
            Exit::Superseded => "superseded by a newer connection".into(),
            Exit::Ended => "session ended".into(),
            Exit::LocalClose => "closed".into(),
            Exit::PeerBye => "closed by peer".into(),
            Exit::Protocol(e) => format!("protocol error: {e}"),
        }
    }
}

/// The most envelopes taken from the session, or messages read, per turn.
const BATCH: usize = 64;

/// Run one connection of a session until it ends. The socket's read timeout
/// must be the poll interval.
pub(crate) fn run(ws: &mut Ws, shared: &Shared, generation: u64, timing: &Timing) -> Exit {
    let mut last_rx = Instant::now();
    let mut last_tx = Instant::now();
    let mut blocked = false;
    // The peer acknowledges on receipt, independently of its payload layer, so
    // acknowledgements that stop while bodies are in flight mean the
    // connection lost them; resuming on a new one replays them. Liveness is
    // the acknowledgement's progress, never the age of the oldest body: the
    // flow window keeps that age within a third of the bound on a healthy
    // link of any speed.
    let mut acked = (0, Instant::now());
    let mut flow = FlowWindow::new(timing.dead_after, shared.max_buffered);
    let mut rtt = RttMeter::default();
    loop {
        // Outbound: envelopes, then an acknowledgement or a heartbeat.
        let (frames, ack, closing) = {
            let mut inner = shared.lock();
            if inner.generation != generation {
                return Exit::Superseded;
            }
            if inner.end.is_some() {
                return Exit::Ended;
            }
            let window = flow.window();
            let frames: Vec<Envelope> = if blocked || inner.close_requested {
                Vec::new()
            } else {
                std::iter::from_fn(|| inner.seq.next_within(window))
                    .take(BATCH)
                    .collect()
            };
            let heartbeat_due = !blocked && last_tx.elapsed() >= timing.heartbeat;
            let ack = (frames.is_empty() && (inner.seq.needs_ack() || heartbeat_due))
                .then(|| inner.seq.mark_ack_sent());
            (frames, ack, inner.close_requested)
        };
        if closing {
            let _ = send_control(ws, &Control::Bye);
            let _ = ws.close(None);
            let _ = ws.flush();
            return Exit::LocalClose;
        }
        for frame in frames {
            let seq = frame.seq;
            match ws.write(Message::Binary(frame.encode().into())) {
                Ok(()) => {
                    last_tx = Instant::now();
                    rtt.wrote(seq, last_tx);
                }
                // The frame was refused; it goes out again once the buffer drains.
                Err(tungstenite::Error::WriteBufferFull(_)) => {
                    shared.lock().seq.rewind(seq);
                    rtt.rewind(seq);
                    break;
                }
                // The frame is queued in the write buffer; the flush below decides.
                Err(e) if would_block(&e) => {
                    last_tx = Instant::now();
                    rtt.wrote(seq, last_tx);
                }
                Err(e) => return Exit::Lost(e.to_string()),
            }
        }
        if let Some(ack) = ack {
            match ws.write(Message::Text(Control::Ack { ack }.to_text().into())) {
                Ok(()) => last_tx = Instant::now(),
                Err(e) if would_block(&e) => {}
                Err(tungstenite::Error::WriteBufferFull(_)) => {}
                Err(e) => return Exit::Lost(e.to_string()),
            }
        }
        // While the stream takes nothing, no new envelopes or heartbeats are
        // queued; silence on the read side then ends the connection.
        match ws.flush() {
            Ok(()) => blocked = false,
            Err(e) if would_block(&e) => blocked = true,
            Err(e) => return Exit::Lost(e.to_string()),
        }

        // Inbound: drain what is there, waiting at most one poll interval.
        for _ in 0..BATCH {
            match ws.read() {
                Ok(Message::Binary(bytes)) => {
                    last_rx = Instant::now();
                    if let Err(e) = Envelope::decode(&bytes).and_then(|env| deliver(shared, env)) {
                        return Exit::Protocol(e);
                    }
                }
                Ok(Message::Text(text)) => {
                    last_rx = Instant::now();
                    match Control::parse(&text) {
                        Ok(Control::Ack { ack }) => {
                            let result = shared.lock().seq.ack(ack);
                            shared.changed.notify_all();
                            if let Err(e) = result {
                                return Exit::Protocol(e);
                            }
                        }
                        Ok(Control::Bye) => return Exit::PeerBye,
                        Ok(other) => {
                            return Exit::Protocol(ProtocolError::Unexpected(format!(
                                "{other:?} after the handshake"
                            )))
                        }
                        Err(e) => return Exit::Protocol(e),
                    }
                }
                Ok(Message::Close(_)) => return Exit::Lost("peer closed the WebSocket".into()),
                Ok(_) => last_rx = Instant::now(),
                Err(e) if would_block(&e) => break,
                Err(e) => return Exit::Lost(e.to_string()),
            }
        }
        if last_rx.elapsed() >= timing.dead_after {
            return Exit::Dead;
        }
        let now = Instant::now();
        let (now_acked, awaiting) = {
            let mut inner = shared.lock();
            let (now_acked, awaiting) = (inner.seq.acked(), inner.seq.awaiting_ack());
            if let Some(sample) = rtt.acked(now_acked, now) {
                inner.counters.rtt.sample(sample);
            }
            let busy = inner.seq.unacked() > 0;
            flow.observe(now, inner.seq.acked_bytes(), busy, inner.counters.rtt.min);
            (now_acked, awaiting)
        };
        if now_acked != acked.0 || !awaiting {
            acked = (now_acked, Instant::now());
        } else if acked.1.elapsed() >= timing.dead_after {
            return Exit::Stalled;
        }
    }
}

fn deliver(shared: &Shared, envelope: Envelope) -> Result<(), ProtocolError> {
    let mut inner = shared.lock();
    if let Accepted::Deliver(body) = inner.seq.accept(envelope)? {
        inner.deliver(body);
    }
    drop(inner);
    shared.changed.notify_all();
    Ok(())
}
