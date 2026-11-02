//! One execution record (BK-3, RP-7): an execution ID of one lease, the
//! operation it names, what the agent sent for it so far, and who waits for
//! it. The record outlives every session that asked for it, and a repeated
//! execution ID attaches to it instead of running again.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use relay_agent::protocol::{CallResult, Digest, ExecutionState, Op, OpKind, Outcome, MAX_CHUNK};

use crate::agent::AgentEvent;
use crate::credentials::Target;
use crate::{Inner, Unavailable};

/// What a caller of one execution receives, in order: possibly `Pending`,
/// then the stream's chunks and end, then `Done`, the last event.
#[derive(Debug, Clone, PartialEq)]
pub enum JobEvent {
    /// The relay agent has no session; the call waits for it (RP-8).
    Pending {
        /// `agent_not_connected`.
        code: String,
        /// For people.
        message: String,
        /// How long the call waits at most.
        wait_ms: u64,
    },
    /// Bytes of the call's stream.
    Chunk(Vec<u8>),
    /// The end of the stream.
    End(Digest),
    /// The outcome.
    Done {
        /// Whether the execution ID had run before this caller asked.
        replayed: bool,
        /// The outcome.
        outcome: Outcome,
    },
}

struct Subscriber {
    tx: Sender<JobEvent>,
    /// The caller that made the record, whose answer is not a replay.
    origin: bool,
    /// Whether it receives the stream's bytes; a read's late callers do not
    /// (RP-7).
    bytes: bool,
}

#[derive(Default)]
struct State {
    /// The stream of an exec or script, while it fits the bound.
    output: Vec<u8>,
    /// Whether `output` holds every byte delivered so far.
    output_kept: bool,
    /// Bytes of the stream delivered to callers so far, which a resend's
    /// replay skips.
    delivered: u64,
    end: Option<Digest>,
    /// The outcome, and whether it was a replay for the origin caller.
    done: Option<(bool, Outcome)>,
    subscribers: Vec<Subscriber>,
    /// Whether the call has been sent to an agent session.
    handed_on: bool,
    pending_told: bool,
    finished_at: Option<Instant>,
    upload: Option<Arc<Vec<u8>>>,
}

pub(crate) struct Job {
    pub target: Target,
    pub execution: String,
    pub op: Op,
    /// A replay for one late caller, kept out of the records.
    pub transient: bool,
    max_kept: usize,
    state: Mutex<State>,
}

fn send_all(subscribers: &mut Vec<Subscriber>, event: &JobEvent, bytes_only: bool) {
    subscribers.retain(|s| (bytes_only && !s.bytes) || s.tx.send(event.clone()).is_ok());
}

impl Job {
    pub fn new(
        target: Target,
        execution: String,
        op: Op,
        upload: Vec<u8>,
        transient: bool,
        max_kept: usize,
    ) -> Job {
        Job {
            target,
            execution,
            op,
            transient,
            max_kept,
            state: Mutex::new(State {
                output_kept: true,
                upload: Some(Arc::new(upload)),
                ..State::default()
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn kind(&self) -> OpKind {
        self.op.kind()
    }

    /// Attach a caller. The origin caller gets everything; a later caller
    /// gets an exec's or script's whole stream so far, then the rest. `None`
    /// means the kept stream was dropped for its size, so the caller needs a
    /// replay from the agent.
    pub fn subscribe(&self, origin: bool) -> Option<Receiver<JobEvent>> {
        let (tx, rx) = mpsc::channel();
        let streams = self.kind().streams_output();
        let mut state = self.lock();
        if !origin && streams && !state.output_kept {
            return None;
        }
        if !origin && streams {
            for piece in state.output.chunks(MAX_CHUNK) {
                let _ = tx.send(JobEvent::Chunk(piece.to_vec()));
            }
        }
        if let Some(end) = &state.end {
            if !origin {
                let _ = tx.send(JobEvent::End(end.clone()));
            }
        }
        if let Some((_, outcome)) = &state.done {
            let _ = tx.send(JobEvent::Done {
                replayed: true,
                outcome: outcome.clone(),
            });
            return Some(rx);
        }
        state.subscribers.push(Subscriber {
            tx,
            origin,
            bytes: origin || streams,
        });
        Some(rx)
    }

    /// The record as a query answer.
    pub fn state(&self) -> ExecutionState {
        let state = self.lock();
        match &state.done {
            Some((_, outcome)) => ExecutionState::Completed {
                op: self.kind(),
                outcome: outcome.clone(),
            },
            None => ExecutionState::Running { op: self.kind() },
        }
    }

    pub fn expired(&self, now: Instant, retention: std::time::Duration) -> bool {
        self.lock()
            .finished_at
            .is_some_and(|at| now.duration_since(at) >= retention)
    }

    fn tell_pending(&self, wait: std::time::Duration) {
        let mut state = self.lock();
        if state.pending_told {
            return;
        }
        state.pending_told = true;
        let event = JobEvent::Pending {
            code: "agent_not_connected".into(),
            message: format!("the relay agent of {} has no session", self.target),
            wait_ms: u64::try_from(wait.as_millis()).unwrap_or(u64::MAX),
        };
        send_all(&mut state.subscribers, &event, false);
    }

    /// Mark the call handed to an agent session; returns whether it had been
    /// handed on before.
    fn hand_on(&self) -> (bool, Arc<Vec<u8>>) {
        let mut state = self.lock();
        state.pending_told = false;
        let before = state.handed_on;
        state.handed_on = true;
        let upload = state.upload.clone().unwrap_or_default();
        (before, upload)
    }

    fn on_chunk(&self, seen: &mut u64, data: &[u8]) {
        let start = *seen;
        *seen += data.len() as u64;
        let mut state = self.lock();
        if *seen <= state.delivered {
            return;
        }
        let skip = usize::try_from(state.delivered.saturating_sub(start)).unwrap_or(usize::MAX);
        let fresh = &data[skip.min(data.len())..];
        if self.kind().streams_output() && state.output_kept {
            if state.output.len() + fresh.len() <= self.max_kept {
                state.output.extend_from_slice(fresh);
            } else {
                state.output_kept = false;
                state.output = Vec::new();
            }
        }
        state.delivered += fresh.len() as u64;
        send_all(
            &mut state.subscribers,
            &JobEvent::Chunk(fresh.to_vec()),
            true,
        );
    }

    fn on_end(&self, end: Digest) {
        let mut state = self.lock();
        state.end = Some(end.clone());
        send_all(&mut state.subscribers, &JobEvent::End(end), false);
    }

    /// Settle the record. Returns whether the record must be forgotten,
    /// because nothing ran and the ID may be sent again (RP-6).
    fn settle(&self, outcome: Outcome, replayed_for_origin: bool) -> bool {
        let mut state = self.lock();
        state.done = Some((replayed_for_origin, outcome.clone()));
        state.finished_at = Some(Instant::now());
        state.upload = None;
        for subscriber in state.subscribers.drain(..) {
            let _ = subscriber.tx.send(JobEvent::Done {
                replayed: !(subscriber.origin && !self.transient) || replayed_for_origin,
                outcome: outcome.clone(),
            });
        }
        matches!(
            outcome,
            Outcome::Failed {
                committed: false,
                ..
            }
        )
    }

    fn on_result(&self, result: CallResult, handed_before: bool) -> bool {
        // A replay of a call this record had already handed on is the
        // origin's own run, answered again after a lost session.
        self.settle(result.outcome, result.replayed && !handed_before)
    }

    fn handed_on(&self) -> bool {
        self.lock().handed_on
    }
}

#[cfg(test)]
impl Job {
    pub(crate) fn hand_on_for_test(&self) -> (bool, Arc<Vec<u8>>) {
        self.hand_on()
    }
    pub(crate) fn on_chunk_for_test(&self, seen: &mut u64, data: &[u8]) {
        self.on_chunk(seen, data)
    }
    pub(crate) fn on_end_for_test(&self, end: Digest) {
        self.on_end(end)
    }
    pub(crate) fn on_result_for_test(&self, result: CallResult, handed_before: bool) -> bool {
        self.on_result(result, handed_before)
    }
}

/// Run one record to its outcome: wait for the lease's relay agent, hand it
/// the call, follow its answer, and hand it again to the agent's next
/// session if this one ends first. The agent never runs an execution ID
/// twice, so a resend is always safe.
pub(crate) fn drive(inner: Arc<Inner>, job: Arc<Job>) {
    let wait = inner.config.agent_wait;
    loop {
        let conn = match inner.wait_agent(&job.target, wait, &|left| job.tell_pending(left)) {
            Ok(conn) => conn,
            Err(why) => {
                let handed = job.handed_on();
                let outcome = match why {
                    Unavailable::Revoked(reason) => Outcome::Failed {
                        code: "revoked".into(),
                        message: format!("the lease was {reason}"),
                        committed: handed,
                    },
                    Unavailable::Timeout if handed => Outcome::failed(
                        "agent_lost",
                        "the relay agent went away after it was handed the call; \
the outcome is uncertain: query it later",
                    ),
                    Unavailable::Timeout => Outcome::refused(
                        "agent_not_connected",
                        format!("the relay agent of {} did not register in time", job.target),
                    ),
                };
                if job.settle(outcome, false) && !job.transient {
                    inner.forget(&job);
                }
                return;
            }
        };
        let (handed_before, upload) = job.hand_on();
        let events = conn.start(&job.execution, &job.op, &upload);
        drop(upload);
        let mut seen = 0u64;
        let result = loop {
            match events.recv() {
                Ok(AgentEvent::Chunk(data)) => job.on_chunk(&mut seen, &data),
                Ok(AgentEvent::End(end)) => job.on_end(end),
                Ok(AgentEvent::Result(result)) => break Some(result),
                Ok(AgentEvent::Query(_)) => {}
                Ok(AgentEvent::Lost) | Err(_) => break None,
            }
        };
        match result {
            Some(result) => {
                if job.on_result(result, handed_before) && !job.transient {
                    inner.forget(&job);
                }
                return;
            }
            None => (inner.log)(&format!(
                "relay: {}: the agent session ended while {} was out; sending it again to the next one",
                job.target, job.execution
            )),
        }
    }
}
