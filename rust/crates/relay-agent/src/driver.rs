//! The broker's end of one link to a relay agent: send calls with their
//! uploads in chunks, and collect each call's chunks, end and result.

use std::collections::HashMap;
use std::sync::Arc;

use crate::link::{Link, LinkError};
use crate::protocol::{Call, CallResult, Digest, ExecutionState, Hello, Message, Op, MAX_CHUNK};

/// Everything the agent sent for one call.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Answer {
    /// The bytes of the call's chunks, in order.
    pub bytes: Vec<u8>,
    /// The size of each chunk.
    pub chunks: Vec<usize>,
    /// The end of the stream, when one came.
    pub end: Option<Digest>,
    /// The result.
    pub result: Option<CallResult>,
}

impl Answer {
    /// Whether the end matches the bytes received.
    pub fn end_matches(&self) -> bool {
        self.end.as_ref().is_some_and(|end| {
            end.size == self.bytes.len() as u64
                && end.sha256 == guest_actions::sha256_hex(&self.bytes)
        })
    }
}

/// The broker's end of a link.
pub struct Driver {
    link: Arc<dyn Link>,
    hello: Hello,
    next: u64,
    answers: HashMap<u64, Answer>,
    queries: HashMap<u64, ExecutionState>,
    /// Heartbeats received so far.
    pub heartbeats: u64,
}

impl Driver {
    /// Take over a link and wait for the agent's hello.
    pub fn open(link: Arc<dyn Link>) -> Result<Driver, LinkError> {
        let hello = loop {
            match Message::decode(&link.receive()?) {
                Ok(Message::Hello(hello)) => break hello,
                Ok(Message::Heartbeat) => {}
                Ok(other) => {
                    return Err(LinkError::Failed(format!(
                        "expected a hello, got {other:?}"
                    )))
                }
                Err(error) => return Err(LinkError::Failed(error.to_string())),
            }
        };
        Ok(Driver {
            link,
            hello,
            next: 1,
            answers: HashMap::new(),
            queries: HashMap::new(),
            heartbeats: 0,
        })
    }

    /// The agent's hello.
    pub fn hello(&self) -> &Hello {
        &self.hello
    }

    /// The link.
    pub fn link(&self) -> &Arc<dyn Link> {
        &self.link
    }

    /// A call number not used before on this link.
    pub fn next_call(&mut self) -> u64 {
        let call = self.next;
        self.next += 1;
        call
    }

    /// Send one message.
    pub fn send(&self, message: &Message) -> Result<(), LinkError> {
        self.link.send(message.encode())
    }

    /// Send a call, and for an operation that uploads, `upload` in chunks
    /// and its end. Returns the call's number.
    pub fn start(&mut self, execution: &str, op: Op, upload: &[u8]) -> Result<u64, LinkError> {
        let call = self.next_call();
        let uploads = op.kind().uploads();
        self.send(&Message::Call(Call {
            call,
            execution: execution.to_string(),
            op,
        }))?;
        if uploads {
            for data in upload.chunks(MAX_CHUNK) {
                self.send(&Message::Chunk {
                    call,
                    data: data.to_vec(),
                })?;
            }
            self.send(&Message::End {
                call,
                execution: execution.to_string(),
                size: upload.len() as u64,
                sha256: guest_actions::sha256_hex(upload),
            })?;
        }
        Ok(call)
    }

    /// What has arrived for `call` so far.
    pub fn received(&self, call: u64) -> Option<&Answer> {
        self.answers.get(&call)
    }

    /// Wait for the result of `call`.
    pub fn finish(&mut self, call: u64) -> Result<Answer, LinkError> {
        loop {
            if self
                .answers
                .get(&call)
                .is_some_and(|answer| answer.result.is_some())
            {
                return Ok(self.answers.remove(&call).unwrap_or_default());
            }
            self.pump()?;
        }
    }

    /// Run one call to its result.
    pub fn run(&mut self, execution: &str, op: Op, upload: &[u8]) -> Result<Answer, LinkError> {
        let call = self.start(execution, op, upload)?;
        self.finish(call)
    }

    /// Ask what the agent knows of `execution`.
    pub fn query(&mut self, execution: &str) -> Result<ExecutionState, LinkError> {
        let call = self.next_call();
        self.send(&Message::Query {
            call,
            execution: execution.to_string(),
        })?;
        loop {
            if let Some(state) = self.queries.remove(&call) {
                return Ok(state);
            }
            self.pump()?;
        }
    }

    /// Receive one message and file it under its call.
    pub fn pump(&mut self) -> Result<(), LinkError> {
        let message = Message::decode(&self.link.receive()?)
            .map_err(|error| LinkError::Failed(error.to_string()))?;
        match message {
            Message::Chunk { call, data } => {
                let answer = self.answers.entry(call).or_default();
                answer.chunks.push(data.len());
                answer.bytes.extend_from_slice(&data);
            }
            Message::End {
                call, size, sha256, ..
            } => {
                self.answers.entry(call).or_default().end = Some(Digest { size, sha256 });
            }
            Message::Result(result) => {
                let call = result.call;
                self.answers.entry(call).or_default().result = Some(result);
            }
            Message::QueryResult { call, state, .. } => {
                self.queries.insert(call, state);
            }
            Message::Heartbeat => self.heartbeats += 1,
            Message::Hello(_)
            | Message::Call(_)
            | Message::Query { .. }
            | Message::Ready(_)
            | Message::Pending(_)
            | Message::Revoked { .. } => {}
        }
        Ok(())
    }
}
