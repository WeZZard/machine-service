//! One execution record (BK-3, RP-7): an execution ID of one lease, the
//! operation it names, what the agent sent for it so far, and who waits for
//! it. The record outlives every session that asked for it, and a repeated
//! execution ID attaches to it instead of running again.
//!
//! The broker passes streams through and keeps none of them whole
//! (RP-15). An upload's bytes go to the relay agent as they come, and the
//! sender keeps at most [`UPLOAD_WINDOW`] bytes beyond the agent's last
//! `progress`; the agent's own streams go out as their consumer takes them,
//! acknowledged with `progress` in the same window. So a record holds at
//! most about one window of a stream, whatever the stream's size. When the
//! agent's session ends during an upload, the bytes on the way are gone: an
//! upload the broker reads itself continues from the agent's committed
//! offset on the next session, and a client's upload ends with
//! `upload_incomplete`, which the client continues the same way.

use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvError, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use relay_agent::protocol::{
    CallResult, Digest, ExecutionState, Op, OpKind, Outcome, MAX_CHUNK, PROGRESS_STEP,
    UPLOAD_WINDOW,
};

use crate::agent::{AgentConn, AgentEvent};
use crate::credentials::Target;
use crate::{Inner, Unavailable};

/// What an in-process call uploads.
#[derive(Clone, Debug, Default)]
pub enum Upload {
    /// Nothing: an operation without an upload, or an exec without input.
    #[default]
    None,
    /// Bytes in memory.
    Bytes(Arc<Vec<u8>>),
    /// A file, read from any offset. Its size and SHA-256 are taken when
    /// the upload is made, and the file must not change until it ends.
    File {
        /// The file.
        path: PathBuf,
        /// Its size.
        size: u64,
        /// Its SHA-256, in lower-case hexadecimal.
        sha256: String,
    },
}

impl Upload {
    /// An upload of `bytes`.
    pub fn bytes(bytes: Vec<u8>) -> Upload {
        Upload::Bytes(Arc::new(bytes))
    }

    /// An upload of the file at `path`, which is hashed once now.
    pub fn file(path: impl Into<PathBuf>) -> std::io::Result<Upload> {
        let path = path.into();
        let (size, sha256) = byte_transfer::hash_file(&path)?;
        Ok(Upload::File { path, size, sha256 })
    }

    /// The size and SHA-256 of the whole stream.
    pub fn digest(&self) -> Digest {
        match self {
            Upload::None => Digest {
                size: 0,
                sha256: guest_actions::sha256_hex(b""),
            },
            Upload::Bytes(bytes) => Digest {
                size: bytes.len() as u64,
                sha256: guest_actions::sha256_hex(bytes),
            },
            Upload::File { size, sha256, .. } => Digest {
                size: *size,
                sha256: sha256.clone(),
            },
        }
    }

    /// The stream from byte `offset` on.
    fn open(&self, offset: u64) -> std::io::Result<Box<dyn Read + Send>> {
        match self {
            Upload::None => Ok(Box::new(std::io::empty())),
            Upload::Bytes(bytes) => {
                let mut cursor = std::io::Cursor::new(bytes.as_ref().clone());
                cursor.set_position(offset);
                Ok(Box::new(cursor))
            }
            Upload::File { path, .. } => {
                let mut file = std::fs::File::open(path)?;
                file.seek(SeekFrom::Start(offset))?;
                Ok(Box::new(file))
            }
        }
    }
}

/// What a caller of one execution receives, in order: possibly `Pending`,
/// for an upload `Progress`, then the stream's chunks and end, then `Done`,
/// the last event.
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
    /// Bytes of the caller's upload the relay agent holds; the first after
    /// a call that resumes is where its chunks start (RP-15).
    Progress(u64),
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

/// The events of one caller. The caller that made the record leads its
/// stream: what it takes is acknowledged to the agent, which sends no more
/// than the window beyond it, and once it is gone the stream is
/// acknowledged as it comes.
pub struct Events {
    rx: Receiver<JobEvent>,
    lead: Option<Arc<Job>>,
}

impl Events {
    fn new(rx: Receiver<JobEvent>, lead: Option<Arc<Job>>) -> Events {
        Events { rx, lead }
    }

    fn took(&self, event: &JobEvent) {
        if let (Some(job), JobEvent::Chunk(data)) = (&self.lead, event) {
            job.consumed(data.len() as u64);
        }
    }

    /// The next event, waiting as long as it takes.
    pub fn recv(&self) -> Result<JobEvent, RecvError> {
        let event = self.rx.recv()?;
        self.took(&event);
        Ok(event)
    }

    /// The next event, waiting up to `timeout`.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<JobEvent, RecvTimeoutError> {
        let event = self.rx.recv_timeout(timeout)?;
        self.took(&event);
        Ok(event)
    }
}

#[cfg(test)]
impl Events {
    /// Every event that arrived so far.
    pub(crate) fn drain(&self) -> Vec<JobEvent> {
        let events: Vec<JobEvent> = self.rx.try_iter().collect();
        for event in &events {
            self.took(event);
        }
        events
    }
}

impl Iterator for Events {
    type Item = JobEvent;

    fn next(&mut self) -> Option<JobEvent> {
        self.recv().ok()
    }
}

impl Drop for Events {
    fn drop(&mut self) {
        if let Some(job) = &self.lead {
            job.lead_gone();
        }
    }
}

/// How bytes of streams the broker holds right now add up, and the most
/// they ever did.
#[derive(Default)]
pub(crate) struct Held {
    now: AtomicI64,
    most: AtomicU64,
}

impl Held {
    fn change(&self, delta: i64) {
        let now = self.now.fetch_add(delta, Ordering::SeqCst) + delta;
        self.most
            .fetch_max(u64::try_from(now).unwrap_or(0), Ordering::SeqCst);
    }

    /// The most bytes of streams the broker held at once.
    pub fn most(&self) -> u64 {
        self.most.load(Ordering::SeqCst)
    }
}

/// Where an upload's bytes come from.
pub(crate) enum Feed {
    /// A client session feeds them as they arrive.
    Client,
    /// The record reads them itself, from any offset.
    Local(Upload),
}

/// What feeding a client's chunk did.
pub(crate) enum Fed {
    /// Passed on, or queued for the agent.
    Taken,
    /// The record no longer takes bytes; the chunk is dropped.
    Closed,
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
    /// The stream position up to which bytes were delivered to callers,
    /// which a resend's replay skips.
    delivered: u64,
    /// The stream position up to which the lead caller took the bytes.
    consumed: u64,
    /// The position last acknowledged to the agent.
    acked: u64,
    lead_gone: bool,
    end: Option<Digest>,
    /// The outcome, and whether it was a replay for the origin caller.
    done: Option<(bool, Outcome)>,
    subscribers: Vec<Subscriber>,
    /// Whether the call has been sent to an agent session.
    handed_on: bool,
    pending_told: bool,
    finished_at: Option<Instant>,
    /// The agent's call that carries the record now.
    attempt: Option<(Arc<AgentConn>, u64)>,
    /// A client's upload: chunks waiting for an agent session.
    queued: VecDeque<Vec<u8>>,
    /// The stream position the client's chunks continue at.
    fed: u64,
    /// The bytes of the upload the agent holds.
    committed: u64,
    /// The client's end, once it came.
    upload_end: Option<Digest>,
    /// Whether the end went to the current attempt.
    end_sent: bool,
    /// Whether `fed` counts from where the client's chunks start: the first
    /// byte, or for a call that resumes, the agent's first progress.
    based: bool,
    /// The bytes of streams this record holds now, as counted in `Held`.
    holding: u64,
}

pub(crate) struct Job {
    pub target: Target,
    /// The lease the call belongs to, which the agent's record keeps.
    pub lease: String,
    pub execution: String,
    /// The operation the relay agent runs.
    pub op: Op,
    /// When a launcher rewrote the client's operation, the client's kind
    /// and the launcher to tell when the record settles.
    pub launched: Option<Launched>,
    /// A replay for one late caller, kept out of the records.
    pub transient: bool,
    /// Whether the first call continues an upload the agent holds.
    pub resume: bool,
    /// Whether a client feeds the upload.
    client: bool,
    /// The upload the record reads itself.
    local: Option<Upload>,
    max_kept: usize,
    held: Arc<Held>,
    state: Mutex<State>,
}

fn send_all(subscribers: &mut Vec<Subscriber>, event: &JobEvent, bytes_only: bool) {
    subscribers.retain(|s| (bytes_only && !s.bytes) || s.tx.send(event.clone()).is_ok());
}

/// What a record needs to be made.
pub(crate) struct JobSpec {
    pub target: Target,
    pub lease: String,
    pub execution: String,
    pub op: Op,
    pub feed: Feed,
    pub resume: bool,
    pub offset: u64,
    pub transient: bool,
    /// Whether a client sent the call, so that the target's launcher, when
    /// it has one, starts its command (DC-1xx-LAUNCH-1).
    pub launch: bool,
}

/// A client's command that the target's launcher rewrote.
#[derive(Clone)]
pub(crate) struct Launched {
    /// The client's operation, which the record answers queries with.
    pub kind: OpKind,
    pub launcher: Arc<dyn crate::Launcher>,
}

impl Job {
    pub fn new(spec: JobSpec, max_kept: usize, held: Arc<Held>) -> Job {
        Job::launched(spec, None, max_kept, held)
    }

    /// A record whose operation `launched` rewrote, when it is `Some`.
    pub fn launched(
        spec: JobSpec,
        launched: Option<Launched>,
        max_kept: usize,
        held: Arc<Held>,
    ) -> Job {
        Job {
            target: spec.target,
            lease: spec.lease,
            execution: spec.execution,
            op: spec.op,
            launched,
            transient: spec.transient,
            resume: spec.resume,
            client: matches!(spec.feed, Feed::Client),
            local: match spec.feed {
                Feed::Local(upload) => Some(upload),
                Feed::Client => None,
            },
            max_kept,
            held,
            state: Mutex::new(State {
                output_kept: spec.offset == 0,
                delivered: spec.offset,
                consumed: spec.offset,
                acked: spec.offset,
                based: !spec.resume,
                ..State::default()
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The client's operation: a launched command answers as the exec or
    /// script it was sent as.
    fn kind(&self) -> OpKind {
        self.launched
            .as_ref()
            .map_or_else(|| self.op.kind(), |launched| launched.kind)
    }

    fn client_fed(&self) -> bool {
        self.client
    }

    /// Recount the bytes this record holds: a client's upload beyond the
    /// agent's committed offset, and the agent's stream beyond what its
    /// lead caller took.
    fn recount(&self, state: &mut State) {
        let upload = if self.client_fed() && state.done.is_none() {
            state.fed.saturating_sub(state.committed)
        } else {
            0
        };
        let stream = if state.lead_gone || state.done.is_some() {
            0
        } else {
            state.delivered.saturating_sub(state.consumed)
        };
        let now = upload + stream;
        self.held.change(now as i64 - state.holding as i64);
        state.holding = now;
    }

    /// Attach a caller. The origin caller gets everything; a later caller
    /// gets an exec's or script's whole stream so far, then the rest. `None`
    /// means the kept stream was dropped for its size, so the caller needs a
    /// replay from the agent.
    pub fn subscribe(self: &Arc<Self>, origin: bool) -> Option<Events> {
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
        let lead = origin.then(|| Arc::clone(self));
        if let Some((_, outcome)) = &state.done {
            let _ = tx.send(JobEvent::Done {
                replayed: true,
                outcome: outcome.clone(),
            });
            return Some(Events::new(rx, None));
        }
        state.subscribers.push(Subscriber {
            tx,
            origin,
            bytes: origin || streams,
        });
        Some(Events::new(rx, lead))
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

    /// Whether a client still feeds the record's upload, before its end.
    pub fn uploading(&self) -> bool {
        let state = self.lock();
        state.done.is_none() && state.upload_end.is_none() && self.client_fed()
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

    /// The lead caller took `count` more bytes of the agent's stream:
    /// acknowledge them to the agent every [`PROGRESS_STEP`].
    fn consumed(&self, count: u64) {
        let mut state = self.lock();
        state.consumed += count;
        self.recount(&mut state);
        self.acknowledge(&mut state, false);
    }

    /// The lead caller is gone: acknowledge the stream as it comes.
    fn lead_gone(&self) {
        let mut state = self.lock();
        state.lead_gone = true;
        self.recount(&mut state);
        self.acknowledge(&mut state, true);
    }

    fn acknowledge(&self, state: &mut State, now: bool) {
        let position = if state.lead_gone {
            state.delivered
        } else {
            state.consumed
        };
        if position > state.acked && (now || position >= state.acked + PROGRESS_STEP) {
            state.acked = position;
            if let Some((conn, call)) = &state.attempt {
                conn.ack(*call, &self.execution, position);
            }
        }
    }

    /// Take the next bytes of a client's upload: pass them to the agent's
    /// call, or queue them while the record waits for an agent session. A
    /// client that sends more than the window beyond the agent's progress
    /// is refused with `window_exceeded`, before it can fill the broker.
    pub fn feed(&self, data: Vec<u8>) -> Fed {
        let mut state = self.lock();
        if state.done.is_some() || state.upload_end.is_some() {
            return Fed::Closed;
        }
        let count = data.len() as u64;
        state.fed += count;
        let ahead = if state.based {
            state.fed.saturating_sub(state.committed)
        } else {
            state
                .queued
                .iter()
                .map(|data| data.len() as u64)
                .sum::<u64>()
                + count
        };
        if ahead > UPLOAD_WINDOW + MAX_CHUNK as u64 {
            drop(state);
            self.refuse(
                "window_exceeded",
                "the client sent more than the window beyond the agent's progress",
            );
            return Fed::Closed;
        }
        match state.attempt.clone() {
            Some((conn, call)) if state.queued.is_empty() => {
                conn.chunk(call, data);
                if !conn.v2() {
                    // An agent of protocol 1 reports nothing: what its
                    // session took counts as held.
                    self.synthesize(&mut state);
                }
            }
            _ => state.queued.push_back(data),
        }
        self.recount(&mut state);
        Fed::Taken
    }

    /// Report an agent of protocol 1's progress for it: the bytes passed on.
    fn synthesize(&self, state: &mut State) {
        let fed = state.fed;
        if fed >= state.committed + PROGRESS_STEP || fed == state.committed {
            state.committed = fed;
            self.tell_progress(state, fed);
        } else {
            state.committed = fed;
        }
    }

    fn tell_progress(&self, state: &mut State, committed: u64) {
        let event = JobEvent::Progress(committed);
        state
            .subscribers
            .retain(|s| !s.origin || s.tx.send(event.clone()).is_ok());
    }

    /// End the record before its upload's end, with a refusal.
    pub fn refuse(&self, code: &str, message: &str) {
        self.settle(Outcome::refused(code, message), false);
    }

    /// Whether the record ended without running, so its ID may be sent
    /// again (RP-6).
    pub fn unrun(&self) -> bool {
        matches!(
            self.lock().done,
            Some((
                _,
                Outcome::Failed {
                    committed: false,
                    ..
                }
            ))
        )
    }

    /// The client's end of its upload.
    pub fn finish(&self, end: Digest) {
        let mut state = self.lock();
        if state.done.is_some() || state.upload_end.is_some() {
            return;
        }
        state.upload_end = Some(end.clone());
        if let Some((conn, call)) = state.attempt.clone() {
            if state.queued.is_empty() {
                conn.end(call, &self.execution, &end);
                state.end_sent = true;
            }
        }
    }

    /// The client that fed the upload went away before its end: nothing
    /// can complete it. The agent keeps what it committed for the client to
    /// continue in a later session.
    pub fn abandon(&self) -> bool {
        if !self.uploading() {
            return false;
        }
        self.settle(
            Outcome::refused(
                "upload_incomplete",
                "the client's session ended before its upload's end",
            ),
            false,
        )
    }

    /// Mark the call handed to an agent session, open the agent's call for
    /// it, and pass on what a client already sent; returns whether it had
    /// been handed on before, the agent's call, and the stream position its
    /// answer starts at: where the callers' bytes end, for an agent that
    /// takes an offset, else the first byte, which [`Job::on_chunk`] skips.
    fn open_attempt(
        &self,
        conn: &Arc<AgentConn>,
        resume: bool,
    ) -> (bool, u64, Receiver<AgentEvent>, u64) {
        let mut state = self.lock();
        state.pending_told = false;
        let before = state.handed_on;
        state.handed_on = true;
        let offset = state.delivered;
        let (call, events) = conn.open(&self.execution, &self.op, &self.lease, resume, offset);
        state.attempt = Some((Arc::clone(conn), call));
        state.acked = offset;
        state.end_sent = false;
        if self.client_fed() {
            if !conn.v2() {
                // An agent of protocol 1 starts every upload at its first
                // byte, and reports nothing.
                state.committed = 0;
                self.tell_progress(&mut state, 0);
            }
            for data in std::mem::take(&mut state.queued) {
                conn.chunk(call, data);
            }
            if !conn.v2() {
                state.committed = state.fed;
            }
            if let Some(end) = state.upload_end.clone() {
                conn.end(call, &self.execution, &end);
                state.end_sent = true;
            }
            self.recount(&mut state);
        }
        let base = if conn.v2() { offset } else { 0 };
        (before, call, events, base)
    }

    fn on_progress(&self, committed: u64) {
        let mut state = self.lock();
        if !state.based {
            // A call that resumes: the client's chunks start here.
            state.based = true;
            let queued: u64 = state.queued.iter().map(|data| data.len() as u64).sum();
            state.fed = committed + queued;
        }
        state.committed = committed;
        self.tell_progress(&mut state, committed);
        self.recount(&mut state);
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
        if !state.subscribers.iter().any(|s| s.origin) {
            state.lead_gone = true;
        }
        self.recount(&mut state);
        let lead_gone = state.lead_gone;
        self.acknowledge(&mut state, lead_gone);
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
        if state.done.is_some() {
            return false;
        }
        state.done = Some((replayed_for_origin, outcome.clone()));
        state.finished_at = Some(Instant::now());
        state.queued.clear();
        if let Some((conn, call)) = state.attempt.take() {
            conn.forget(call);
        }
        self.recount(&mut state);
        for subscriber in state.subscribers.drain(..) {
            let _ = subscriber.tx.send(JobEvent::Done {
                replayed: !(subscriber.origin && !self.transient) || replayed_for_origin,
                outcome: outcome.clone(),
            });
        }
        drop(state);
        // Whatever the launcher left for the command is no longer needed:
        // the command ran, or it never will under this record.
        if let (Some(launched), false) = (&self.launched, self.transient) {
            launched.launcher.settled(&self.lease, &self.execution);
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

    /// Whether the record has its outcome.
    pub fn is_done(&self) -> bool {
        self.lock().done.is_some()
    }
}

#[cfg(test)]
impl Job {
    pub(crate) fn on_chunk_for_test(&self, seen: &mut u64, data: &[u8]) {
        self.on_chunk(seen, data)
    }
    pub(crate) fn on_end_for_test(&self, end: Digest) {
        self.on_end(end)
    }
    pub(crate) fn on_result_for_test(&self, result: CallResult, handed_before: bool) -> bool {
        self.on_result(result, handed_before)
    }
    pub(crate) fn mark_handed_for_test(&self) -> bool {
        let mut state = self.lock();
        let before = state.handed_on;
        state.handed_on = true;
        before
    }
}

/// How one agent call of a record ended.
enum Ended {
    /// The record has its outcome.
    Settled,
    /// The agent's session ended first.
    Lost,
    /// The agent is stopping or restarting; its next session takes the call.
    Stopping,
}

/// Run one record to its outcome: wait for the lease's relay agent, hand it
/// the call, follow its answer, and hand it again to the agent's next
/// session if this one ends first. The agent never runs an execution ID
/// twice, so a resend is always safe.
pub(crate) fn drive(inner: Arc<Inner>, job: Arc<Job>) {
    let wait = inner.config.agent_wait;
    let local = job.local.clone();
    let mut attempts = 0u32;
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
        if job.is_done() {
            return finished(&inner, &job);
        }
        // A later attempt of an upload continues what the agent holds.
        let resume = job.resume || attempts > 0;
        let (handed_before, call, events, seen) = job.open_attempt(&conn, resume);
        attempts += 1;
        inner.mark_handed(&job.target, &job.lease);
        let attempt = Attempt {
            job: &job,
            inner: &inner,
            events: &events,
            handed_before,
            seen,
        };
        let ended = match &local {
            Some(upload) if job.kind().uploads() => attempt.push(&conn, call, upload, resume),
            _ => attempt.follow(),
        };
        match ended {
            Ended::Settled => return finished(&inner, &job),
            Ended::Stopping => {
                (inner.log)(&format!(
                    "relay: {}: the agent is stopping; sending {} to its next session",
                    job.target, job.execution
                ));
                std::thread::sleep(Duration::from_millis(250));
            }
            Ended::Lost => {
                let interrupted = {
                    let state = job.lock();
                    job.client_fed() && state.upload_end.is_none()
                };
                if interrupted {
                    // The client's bytes on the way are gone; it continues
                    // from the agent's committed offset (RP-15).
                    let outcome = Outcome::refused(
                        "upload_incomplete",
                        "the relay agent's session ended during the upload; send the call \
again with resume to continue from the agent's committed offset",
                    );
                    if job.settle(outcome, false) && !job.transient {
                        inner.forget(&job);
                    }
                    return;
                }
                (inner.log)(&format!(
                    "relay: {}: the agent session ended while {} was out; sending it again to the next one",
                    job.target, job.execution
                ));
            }
        }
    }
}

/// A record that ended without running is forgotten, so its ID may be sent
/// again (RP-6); one a caller or a client's loss ended is among them.
fn finished(inner: &Inner, job: &Arc<Job>) {
    if job.unrun() && !job.transient {
        inner.forget(job);
    }
}

/// One agent call of a record, being followed.
struct Attempt<'a> {
    job: &'a Arc<Job>,
    inner: &'a Inner,
    events: &'a Receiver<AgentEvent>,
    handed_before: bool,
    /// The stream position of the agent's next byte.
    seen: u64,
}

impl Attempt<'_> {
    /// Handle one agent event; `Some` once the call ended.
    fn handle(&mut self, event: AgentEvent) -> Option<Ended> {
        match event {
            AgentEvent::Chunk(data) => self.job.on_chunk(&mut self.seen, &data),
            AgentEvent::End(end) => self.job.on_end(end),
            AgentEvent::Progress(committed) => self.job.on_progress(committed),
            AgentEvent::Query(_) => {}
            AgentEvent::Lost => return Some(Ended::Lost),
            AgentEvent::Result(result) => return Some(self.settle(result)),
        }
        None
    }

    fn settle(&self, result: CallResult) -> Ended {
        if matches!(&result.outcome, Outcome::Failed { code, committed: false, .. } if code == "stopping")
        {
            return Ended::Stopping;
        }
        if self.job.on_result(result, self.handed_before) && !self.job.transient {
            self.inner.forget(self.job);
        }
        Ended::Settled
    }

    /// Follow the agent's call to its end; a record a client gave up on
    /// stops being followed.
    fn follow(mut self) -> Ended {
        loop {
            match self.events.recv_timeout(Duration::from_secs(1)) {
                Ok(event) => {
                    if let Some(ended) = self.handle(event) {
                        return ended;
                    }
                }
                Err(RecvTimeoutError::Timeout) if self.job.is_done() => return Ended::Settled,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Ended::Lost,
            }
        }
    }

    /// Wait for the agent's next event and handle it; `Err` once the call
    /// ended, `Ok(Some)` with a progress.
    fn next(&mut self, wait: bool) -> Result<Option<u64>, Ended> {
        let event = if wait {
            match self.events.recv() {
                Ok(event) => event,
                Err(_) => return Err(Ended::Lost),
            }
        } else {
            match self.events.try_recv() {
                Ok(event) => event,
                Err(mpsc::TryRecvError::Empty) => return Ok(None),
                Err(mpsc::TryRecvError::Disconnected) => return Err(Ended::Lost),
            }
        };
        if let AgentEvent::Progress(at) = event {
            self.job.on_progress(at);
            return Ok(Some(at));
        }
        match self.handle(event) {
            Some(ended) => Err(ended),
            None => Ok(None),
        }
    }

    /// Send an upload the broker reads itself: from the agent's committed
    /// offset when the call resumes, in the window beyond its progress.
    fn push(mut self, conn: &AgentConn, call: u64, upload: &Upload, resume: bool) -> Ended {
        let end = upload.digest();
        let v2 = conn.v2();
        // Where the agent's upload stands: its first progress when the call
        // resumes, else the first byte.
        let mut committed = 0u64;
        if resume && v2 {
            loop {
                match self.next(true) {
                    Ok(Some(at)) => {
                        committed = at;
                        break;
                    }
                    Ok(None) => {}
                    Err(ended) => return ended,
                }
            }
        }
        let mut sent = committed.min(end.size);
        let unreadable = |job: &Arc<Job>, why: String| {
            job.refuse("bad_request", &why);
            Ended::Settled
        };
        let mut reader = match upload.open(sent) {
            Ok(reader) => reader,
            Err(error) => {
                return unreadable(self.job, format!("the upload cannot be read: {error}"))
            }
        };
        let mut buffer = vec![0u8; MAX_CHUNK];
        while sent < end.size {
            // Keep within the window beyond the agent's progress.
            while v2 && sent >= committed + UPLOAD_WINDOW {
                match self.next(true) {
                    Ok(Some(at)) => committed = committed.max(at),
                    Ok(None) => {}
                    Err(ended) => return ended,
                }
            }
            loop {
                match self.next(false) {
                    Ok(Some(at)) => committed = committed.max(at),
                    Ok(None) => break,
                    Err(ended) => return ended,
                }
            }
            let want = (end.size - sent).min(MAX_CHUNK as u64) as usize;
            let count = match read_full(&mut reader, &mut buffer[..want]) {
                Ok(count) if count > 0 => count,
                _ => {
                    return unreadable(
                        self.job,
                        "the upload ended before its size; it changed while it was sent".into(),
                    )
                }
            };
            if !conn.chunk(call, buffer[..count].to_vec()) {
                return Ended::Lost;
            }
            sent += count as u64;
        }
        if !conn.end(call, &self.job.execution, &end) {
            return Ended::Lost;
        }
        self.follow()
    }
}

fn read_full(reader: &mut dyn Read, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}
