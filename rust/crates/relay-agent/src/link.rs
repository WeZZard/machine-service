//! The link the relay agent talks over: an ordered, reliable,
//! message-oriented duplex link carrying opaque payloads.
//!
//! The resumable WebSocket session of `docs/arch/remote-session.md` is one
//! such link, built separately. This module holds the trait, an in-memory
//! pair for tests, and a link over a byte stream (length-prefixed messages),
//! which the binary serves on its standard input and output.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::sync::{Arc, Condvar, Mutex};

use thiserror::Error;

use crate::protocol::MAX_MESSAGE;

/// Why a link can carry no more messages.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum LinkError {
    /// The link closed, from either side.
    #[error("the link is closed")]
    Closed,
    /// A message was larger than the link carries (TC-8).
    #[error("a message of {0} bytes exceeds the link's limit")]
    TooLarge(usize),
    /// The link failed.
    #[error("the link failed: {0}")]
    Failed(String),
}

/// An ordered, reliable, message-oriented duplex link with opaque payloads.
///
/// Messages arrive whole, in the order sent, each at most once, until the
/// link closes; after that every call fails. Any thread may send; one thread
/// receives.
pub trait Link: Send + Sync {
    /// Send one message.
    fn send(&self, payload: Vec<u8>) -> Result<(), LinkError>;
    /// Wait for the next message.
    fn receive(&self) -> Result<Vec<u8>, LinkError>;
    /// Close the link. A blocked `receive` on this side returns, and the
    /// other side sees [`LinkError::Closed`] after the messages already sent.
    fn close(&self);
    /// Wait up to `timeout` for the messages sent so far to leave this
    /// process, before the process ends or replaces itself.
    fn flush(&self, timeout: std::time::Duration) {
        let _ = timeout;
    }
}

// --------------------------------------------------------------- in memory

#[derive(Default)]
struct Shared {
    queues: [VecDeque<Vec<u8>>; 2],
    closed: bool,
}

/// One end of an in-memory link.
pub struct MemoryLink {
    shared: Arc<(Mutex<Shared>, Condvar)>,
    side: usize,
    limit: usize,
}

/// Two connected in-memory ends that refuse messages over [`MAX_MESSAGE`],
/// as a proxy on the transport may (TC-8).
pub fn memory_pair() -> (MemoryLink, MemoryLink) {
    let shared = Arc::new((Mutex::new(Shared::default()), Condvar::new()));
    let end = |side| MemoryLink {
        shared: Arc::clone(&shared),
        side,
        limit: MAX_MESSAGE,
    };
    (end(0), end(1))
}

impl MemoryLink {
    /// Whether the link has closed.
    pub fn is_closed(&self) -> bool {
        self.shared.0.lock().unwrap().closed
    }
}

impl Link for MemoryLink {
    fn send(&self, payload: Vec<u8>) -> Result<(), LinkError> {
        if payload.len() > self.limit {
            return Err(LinkError::TooLarge(payload.len()));
        }
        let (state, changed) = &*self.shared;
        let mut state = state.lock().unwrap();
        if state.closed {
            return Err(LinkError::Closed);
        }
        state.queues[1 - self.side].push_back(payload);
        changed.notify_all();
        Ok(())
    }

    fn receive(&self) -> Result<Vec<u8>, LinkError> {
        let (state, changed) = &*self.shared;
        let mut state = state.lock().unwrap();
        loop {
            if let Some(payload) = state.queues[self.side].pop_front() {
                return Ok(payload);
            }
            if state.closed {
                return Err(LinkError::Closed);
            }
            state = changed.wait(state).unwrap();
        }
    }

    fn close(&self) {
        let (state, changed) = &*self.shared;
        let mut state = state.lock().unwrap();
        state.closed = true;
        // This side's own queue is abandoned, so its blocked receive returns.
        state.queues[self.side].clear();
        changed.notify_all();
    }
}

impl Drop for MemoryLink {
    fn drop(&mut self) {
        self.close();
    }
}

// ------------------------------------------------------------ byte stream

/// A link over a byte stream: each message is a 4-byte big-endian length
/// and that many bytes. Integrity is the stream's own; the stream must be
/// reliable, such as a pipe.
pub struct StreamLink<R, W> {
    reader: Mutex<Option<R>>,
    writer: Mutex<Option<W>>,
}

impl<R: Read + Send, W: Write + Send> StreamLink<R, W> {
    /// A link reading messages from `reader` and writing them to `writer`.
    pub fn new(reader: R, writer: W) -> StreamLink<R, W> {
        StreamLink {
            reader: Mutex::new(Some(reader)),
            writer: Mutex::new(Some(writer)),
        }
    }
}

impl<R: Read + Send, W: Write + Send> Link for StreamLink<R, W> {
    fn send(&self, payload: Vec<u8>) -> Result<(), LinkError> {
        if payload.len() > MAX_MESSAGE {
            return Err(LinkError::TooLarge(payload.len()));
        }
        let mut writer = self.writer.lock().unwrap_or_else(|p| p.into_inner());
        let stream = writer.as_mut().ok_or(LinkError::Closed)?;
        let written = stream
            .write_all(&(payload.len() as u32).to_be_bytes())
            .and_then(|()| stream.write_all(&payload))
            .and_then(|()| stream.flush());
        written.map_err(|error| {
            *writer = None;
            stream_error(error)
        })
    }

    fn receive(&self) -> Result<Vec<u8>, LinkError> {
        let mut reader = self.reader.lock().unwrap_or_else(|p| p.into_inner());
        let stream = reader.as_mut().ok_or(LinkError::Closed)?;
        let mut length = [0u8; 4];
        let received = stream.read_exact(&mut length).and_then(|()| {
            let length = u32::from_be_bytes(length) as usize;
            if length > MAX_MESSAGE {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("a message of {length} bytes exceeds the limit"),
                ));
            }
            let mut payload = vec![0u8; length];
            stream.read_exact(&mut payload).map(|()| payload)
        });
        received.map_err(|error| {
            *reader = None;
            stream_error(error)
        })
    }

    /// Closes the writing half. A receive blocked on the stream returns only
    /// when the stream itself ends.
    fn close(&self) {
        *self.writer.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }
}

fn stream_error(error: io::Error) -> LinkError {
    match error.kind() {
        io::ErrorKind::UnexpectedEof | io::ErrorKind::BrokenPipe => LinkError::Closed,
        _ => LinkError::Failed(error.to_string()),
    }
}
