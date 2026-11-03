//! The relay agent's core (`docs/arch/relay-agent.md`, RA-5).
//!
//! An [`Agent`] owns the durable execution records, the uploads between
//! their call and their end, and the running operations. [`Agent::serve`]
//! answers one link until it closes; the agent outlives its links, so a
//! command keeps running while the broker reconnects, its output waits in
//! the record for whoever asks next, and an upload waits in its spool for a
//! sender to continue it (`docs/arch/relay-protocol.md`, RP-15).
//!
//! Each call has one commit point. `exec`, `script`, `write` and
//! `write_tree` commit at their end message, after the uploaded stream, all
//! of it, matched it; `read`, `read_tree`, `cancel` and `status` commit when
//! they arrive. The record reaches the disk at the commit point, before the
//! operation takes effect, so an execution ID runs at most once, even across
//! a restart of the agent. A call with a recorded execution ID never runs
//! again: it receives the recorded outcome, or it attaches to the operation
//! still running.
//!
//! A link speaks protocol 1 until the broker's `ready` names protocol 2.
//! With protocol 2 the agent reports each upload's committed offset with
//! `progress`, keeps an upload whose link ended for a sender to continue,
//! and sends at most [`UPLOAD_WINDOW`] bytes of a stream beyond the broker's
//! last `progress` for it.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use guest_actions::ownership::{Owner, Ownership};
use guest_actions::{
    kill_command, seal_spooled_file, seal_spooled_tree, Sealed, SealedTree, Tally,
};

use crate::link::{Link, LinkError};
use crate::protocol::{
    Call, CallResult, CancelFound, CodeSigning, Digest, ExecutionState, Hello, Message, Op, OpKind,
    Outcome, Report, Status, MAX_CHUNK, PROGRESS_STEP, PROTOCOL, UPLOAD_WINDOW,
};
use crate::store::{now_ms, Store};
use crate::uploads::{self, Landing, Upload, Uploads, UPLOAD_RETENTION};

/// How often the agent sends a heartbeat; well under TC-3's 30 seconds.
pub const HEARTBEAT: Duration = Duration::from_secs(10);
/// The longest the agent keeps a completed execution's record. A record
/// goes when the broker purges its lease at release (DC-173); this bound
/// only removes the records of a lease whose purge never came, such as
/// after a crash of machine-service (DC-175).
pub const MAX_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// The longest execution ID.
pub const MAX_EXECUTION_ID: usize = 256;
/// The most executions a hello reports.
const MAX_IN_FLIGHT: usize = 256;

/// How the agent runs operations and where it keeps its records.
#[derive(Clone, Debug)]
pub struct AgentOptions {
    /// The shell that runs each command line with `-c`, as `sshd` does.
    pub shell: PathBuf,
    /// The directory relative paths and commands start from.
    pub home: PathBuf,
    /// The directory of the durable execution records.
    pub state: PathBuf,
    /// The SHA-256 of the agent's executable, announced in the hello.
    pub build: String,
    /// The interval of heartbeats.
    pub heartbeat: Duration,
    /// The longest a completed record is kept when no purge removes it.
    pub retention: Duration,
    /// How the operating system owns the agent's commands (DC-139).
    pub ownership: Ownership,
    /// The executable the agent started from, which a restart executes
    /// again: the path, not the file, so a build renamed into place runs.
    pub executable: PathBuf,
    /// The executable's code signature, announced in the hello (DC-232).
    pub signing: Option<CodeSigning>,
}

impl AgentOptions {
    /// Options from the process environment: `$SHELL`, `$HOME`, the state
    /// directory `.local/state/relay-agent` under the agent's root
    /// (`RELAY_AGENT_ROOT`, else `$HOME`), the hash of the running
    /// executable, [`HEARTBEAT`], [`MAX_RETENTION`], and the ownership this
    /// machine supports ([`Ownership::detect`]).
    pub fn from_environment() -> AgentOptions {
        let home = guest_actions::user_home();
        AgentOptions {
            shell: guest_actions::user_shell(),
            state: state_directory(&crate::config::agent_root()),
            home,
            build: guest_actions::executable_sha256(),
            heartbeat: HEARTBEAT,
            retention: MAX_RETENTION,
            ownership: Ownership::detect(),
            executable: std::env::current_exe().unwrap_or_default(),
            signing: own_signing(),
        }
    }
}

/// The code signature of the running executable, as `codesign` shows it:
/// on macOS only, where a Developer ID signature carries privacy grants
/// (DC-232). `None` elsewhere, or when it cannot be read.
pub fn own_signing() -> Option<CodeSigning> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let executable = std::env::current_exe().ok()?;
    let signature = service_layout::signing::inspect(&executable).ok()?;
    Some(CodeSigning {
        kind: signature.kind().to_string(),
        team: signature.team().map(str::to_string),
    })
}

/// The state directory under a home directory.
pub fn state_directory(home: &std::path::Path) -> PathBuf {
    home.join(".local/state/relay-agent")
}

/// The relay agent.
#[derive(Clone)]
pub struct Agent {
    inner: Arc<Inner>,
}

struct Inner {
    options: AgentOptions,
    state: Mutex<State>,
    /// Signalled whenever a running operation ends.
    settled: Condvar,
    links: Mutex<HashMap<u64, Arc<dyn Link>>>,
    /// The uploads between their call and their end, by execution ID.
    uploads: Mutex<Uploads>,
    next: AtomicU64,
    stopping: AtomicBool,
    /// What a committed restart runs; without one, a restart is refused.
    restart: Mutex<Option<Arc<RestartFn>>>,
}

/// Replaces the process with the agent's executable; it returns only when
/// that fails, with the reason.
pub type RestartFn = dyn Fn() -> std::io::Error + Send + Sync;

struct State {
    store: Store,
    live: HashMap<String, Arc<Live>>,
}

/// An operation that passed its commit point and has not completed.
struct Live {
    op: OpKind,
    /// The process group of a command; 0 for other operations.
    group: AtomicI32,
    /// The systemd scope of a command that runs in one.
    scope: Option<String>,
    cancelled: AtomicBool,
    progress: Mutex<Progress>,
    changed: Condvar,
}

#[derive(Default)]
struct Progress {
    /// Bytes of output in the record so far.
    written: u64,
    outcome: Option<Outcome>,
}

/// What a call found under its execution ID.
enum Known {
    Running(Arc<Live>),
    Completed(OpKind, Outcome),
}

impl State {
    fn known(&self, execution: &str) -> Option<Known> {
        if let Some(live) = self.live.get(execution) {
            return Some(Known::Running(Arc::clone(live)));
        }
        let record = self.store.get(execution)?;
        Some(match &record.outcome {
            Some(outcome) => Known::Completed(record.op, outcome.clone()),
            // A record without an outcome and no live operation cannot
            // happen within one agent; report it as interrupted.
            None => Known::Completed(
                record.op,
                Outcome::Interrupted {
                    message: "the operation has no outcome".into(),
                },
            ),
        })
    }

    fn execution_state(&self, execution: &str) -> ExecutionState {
        match self.known(execution) {
            Some(Known::Running(live)) => ExecutionState::Running { op: live.op },
            Some(Known::Completed(op, outcome)) => ExecutionState::Completed { op, outcome },
            None => ExecutionState::Unknown,
        }
    }
}

/// What a call brings to its commit point.
enum Committed {
    /// A command's standard input, from its spool.
    Input(Option<File>),
    File(Sealed),
    Tree(SealedTree),
    Nothing,
}

/// The broker's acknowledgements of the streams the agent sends on one link
/// (protocol 2), and the protocol the link speaks.
#[derive(Default)]
struct Flow {
    protocol: AtomicU32,
    acked: Mutex<FlowState>,
    changed: Condvar,
}

#[derive(Default)]
struct FlowState {
    acked: HashMap<u64, u64>,
    closed: bool,
}

impl Flow {
    fn new() -> Flow {
        Flow {
            protocol: AtomicU32::new(1),
            ..Flow::default()
        }
    }

    /// Whether the broker speaks protocol 2 on this link.
    fn v2(&self) -> bool {
        self.protocol.load(Ordering::SeqCst) >= 2
    }

    fn lock(&self) -> MutexGuard<'_, FlowState> {
        self.acked
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn ack(&self, call: u64, committed: u64) {
        let mut state = self.lock();
        let acked = state.acked.entry(call).or_default();
        *acked = (*acked).max(committed);
        drop(state);
        self.changed.notify_all();
    }

    fn forget(&self, call: u64) {
        self.lock().acked.remove(&call);
    }

    fn close(&self) {
        self.lock().closed = true;
        self.changed.notify_all();
    }

    /// Wait until `call`'s stream may hold `sent` bytes: no more than
    /// [`UPLOAD_WINDOW`] beyond what the broker acknowledged. Returns false
    /// when the link ended or `stop` says to give up.
    fn wait(&self, call: u64, sent: u64, stop: &dyn Fn() -> bool) -> bool {
        if !self.v2() {
            return true;
        }
        let mut state = self.lock();
        loop {
            if state.closed || stop() {
                return false;
            }
            let acked = state.acked.get(&call).copied().unwrap_or(0);
            if sent <= acked + UPLOAD_WINDOW {
                return true;
            }
            state = self
                .changed
                .wait_timeout(state, Duration::from_millis(500))
                .unwrap_or_else(|poison| poison.into_inner())
                .0;
        }
    }
}

/// An upload call this link feeds, and the progress it last reported.
struct Feeding {
    execution: String,
    reported: u64,
}

/// One link being served.
struct Session {
    inner: Arc<Inner>,
    link: Arc<dyn Link>,
    /// The link's number among the agent's links.
    number: u64,
    /// The upload calls this link feeds, by call number.
    calls: HashMap<u64, Feeding>,
    /// Calls whose answer is still being sent.
    active: Arc<Mutex<HashSet<u64>>>,
    flow: Arc<Flow>,
}

impl Agent {
    /// Open the agent's records and its uploads under `options.state`.
    pub fn open(options: AgentOptions) -> std::io::Result<Agent> {
        let store = Store::open(&options.state, options.retention)?;
        let uploads = uploads::load(&uploads::root(&options.state), &store)?;
        Ok(Agent {
            inner: Arc::new(Inner {
                options,
                state: Mutex::new(State {
                    store,
                    live: HashMap::new(),
                }),
                settled: Condvar::new(),
                links: Mutex::new(HashMap::new()),
                uploads: Mutex::new(uploads),
                next: AtomicU64::new(1),
                stopping: AtomicBool::new(false),
                restart: Mutex::new(None),
            }),
        })
    }

    /// The agent's options.
    pub fn options(&self) -> &AgentOptions {
        &self.inner.options
    }

    /// Whether [`Agent::shutdown`] was called.
    pub fn is_stopped(&self) -> bool {
        self.inner.stopping.load(Ordering::SeqCst)
    }

    /// Serve one link until it closes: send the hello, then answer every
    /// message. Operations still running when the link closes keep running,
    /// and so do the uploads it fed when it spoke protocol 2.
    /// A malformed message closes the link and is returned as an error.
    pub fn serve(&self, link: Arc<dyn Link>) -> Result<(), LinkError> {
        if self.is_stopped() {
            link.close();
            return Err(LinkError::Closed);
        }
        let number = self.inner.next.fetch_add(1, Ordering::SeqCst);
        self.inner
            .links
            .lock()
            .unwrap()
            .insert(number, Arc::clone(&link));
        let mut session = Session {
            inner: Arc::clone(&self.inner),
            link: Arc::clone(&link),
            number,
            calls: HashMap::new(),
            active: Arc::default(),
            flow: Arc::new(Flow::new()),
        };
        let (stop, stopped) = mpsc::channel::<()>();
        let served = session.hello().and_then(|()| {
            let heartbeat = self.inner.options.heartbeat;
            let beating = Arc::clone(&link);
            thread::spawn(move || {
                while let Err(mpsc::RecvTimeoutError::Timeout) = stopped.recv_timeout(heartbeat) {
                    if beating.send(Message::Heartbeat.encode()).is_err() {
                        return;
                    }
                }
            });
            loop {
                let payload = match link.receive() {
                    Ok(payload) => payload,
                    Err(LinkError::Closed) => return Ok(()),
                    Err(error) => return Err(error),
                };
                match Message::decode(&payload) {
                    Ok(message) => session.handle(message),
                    Err(error) => {
                        link.close();
                        return Err(LinkError::Failed(error.to_string()));
                    }
                }
            }
        });
        drop(stop);
        self.inner.links.lock().unwrap().remove(&number);
        session.release_uploads();
        session.flow.close();
        drop(session);
        served
    }

    /// Stop the agent: close every link, kill the process group of every
    /// running command, and wait up to `grace` for running operations to
    /// record their end. An operation stopped this way records
    /// [`Outcome::Interrupted`]; one that does not end in time is recorded so
    /// when the records are next opened. Every upload's bytes are flushed,
    /// so the next agent continues it.
    pub fn shutdown(&self, grace: Duration) {
        self.inner.stopping.store(true, Ordering::SeqCst);
        for link in self.inner.links.lock().unwrap().values() {
            link.close();
        }
        let uploads: Vec<_> = self.inner.uploads().values().cloned().collect();
        for upload in uploads {
            let _ = lock(&upload).sync();
        }
        let mut state = self.inner.lock();
        for live in state.live.values() {
            live.cancelled.store(true, Ordering::SeqCst);
            live.kill();
        }
        let deadline = Instant::now() + grace;
        while !state.live.is_empty() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            state = self.inner.settled.wait_timeout(state, left).unwrap().0;
        }
    }

    /// Let a committed [`Op::Restart`] run `restart`, which replaces the
    /// process; the binary passes [`reexec`].
    pub fn set_restart(&self, restart: impl Fn() -> std::io::Error + Send + Sync + 'static) {
        *self.inner.restart.lock().unwrap() = Some(Arc::new(restart));
    }

    /// Close every link and keep running, as a lost connection would: the
    /// dialer opens a new session, which says hello again.
    pub fn drop_links(&self) {
        for link in self.inner.links.lock().unwrap().values() {
            link.close();
        }
    }

    /// What the agent knows of `execution`.
    pub fn query(&self, execution: &str) -> ExecutionState {
        self.inner.lock().execution_state(execution)
    }

    /// The bytes the agent holds of the upload of `execution`, when it holds
    /// one that has not reached its commit point.
    pub fn upload_held(&self, execution: &str) -> Option<u64> {
        let upload = self.inner.uploads().get(execution).cloned()?;
        let held = lock(&upload).held();
        Some(held)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    fn uploads(&self) -> MutexGuard<'_, Uploads> {
        lock(&self.uploads)
    }

    fn status(&self, state: &State) -> Status {
        let mut running: Vec<String> = state.live.keys().cloned().collect();
        running.sort();
        Status {
            protocol: PROTOCOL,
            build: self.options.build.clone(),
            os: std::env::consts::OS.to_string(),
            running,
            ownership: self.options.ownership.describe(),
        }
    }

    /// Record the outcome of a live operation and wake whoever waits for it.
    fn settle(&self, execution: &str, live: &Live, outcome: Outcome) {
        let mut state = self.lock();
        let _ = state.store.complete(execution, outcome.clone());
        state.live.remove(execution);
        {
            let mut progress = live.progress.lock().unwrap();
            progress.outcome = Some(outcome);
        }
        live.changed.notify_all();
        self.settled.notify_all();
    }

    /// Answer `call` on link `number` with a refusal, when that link is
    /// still served.
    fn refuse_on(&self, number: u64, call: u64, execution: &str, code: &str, message: &str) {
        let link = self.links.lock().unwrap().get(&number).cloned();
        if let Some(link) = link {
            let _ = link.send(
                Message::Result(CallResult {
                    call,
                    execution: execution.to_string(),
                    replayed: false,
                    outcome: Outcome::refused(code, message),
                })
                .encode(),
            );
        }
    }

    /// Drop the upload of `execution`, refusing the call that feeds it with
    /// `code`. Returns whether there was one.
    fn discard_upload(&self, execution: &str, code: &str, message: &str) -> bool {
        let Some(upload) = self.uploads().remove(execution) else {
            return false;
        };
        let upload = Arc::try_unwrap(upload)
            .map(|mutex| mutex.into_inner().unwrap_or_else(|p| p.into_inner()));
        match upload {
            Ok(upload) => {
                if let Some((number, call)) = upload.owner {
                    self.refuse_on(number, call, execution, code, message);
                }
                upload.discard();
            }
            // A link is appending to it this moment; its owner check fails
            // from now on, and the spool goes with the last reference.
            Err(shared) => {
                let owner = lock(&shared).owner.take();
                if let Some((number, call)) = owner {
                    self.refuse_on(number, call, execution, code, message);
                }
                let mut upload = lock(&shared);
                if let Some(spool) = upload.spool.take() {
                    spool.discard();
                }
                upload.forget();
            }
        }
        true
    }

    /// Drop the uploads nobody continued within [`UPLOAD_RETENTION`].
    fn prune_uploads(&self) {
        let horizon = now_ms().saturating_sub(UPLOAD_RETENTION.as_millis() as u64);
        let stale: Vec<String> = self
            .uploads()
            .iter()
            .filter(|(_, upload)| {
                let upload = lock(upload);
                upload.owner.is_none() && upload.updated_ms < horizon
            })
            .map(|(execution, _)| execution.clone())
            .collect();
        for execution in stale {
            self.discard_upload(&execution, "cancelled", "the upload expired");
        }
    }
}

impl Session {
    fn send(&self, message: &Message) -> Result<(), LinkError> {
        self.link.send(message.encode())
    }

    fn hello(&self) -> Result<(), LinkError> {
        self.inner.prune_uploads();
        let (in_flight, carried) = {
            let mut state = self.inner.lock();
            state.store.prune(self.inner.options.retention);
            let mut in_flight: Vec<Report> = state
                .live
                .iter()
                .map(|(execution, live)| Report {
                    execution: execution.clone(),
                    state: ExecutionState::Running { op: live.op },
                })
                .collect();
            let mut unreported: Vec<_> = state
                .store
                .records()
                .filter(|record| !record.reported)
                .filter_map(|record| {
                    let outcome = record.outcome.clone()?;
                    Some((
                        record.finished_ms,
                        record.execution.clone(),
                        record.op,
                        outcome,
                    ))
                })
                .collect();
            unreported.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
            let carried: Vec<String> = unreported
                .iter()
                .take(MAX_IN_FLIGHT.saturating_sub(in_flight.len()))
                .map(|entry| entry.1.clone())
                .collect();
            in_flight.extend(unreported.into_iter().take(carried.len()).map(
                |(_, execution, op, outcome)| Report {
                    execution,
                    state: ExecutionState::Completed { op, outcome },
                },
            ));
            in_flight.truncate(MAX_IN_FLIGHT);
            (in_flight, carried)
        };
        let options = &self.inner.options;
        self.send(&Message::Hello(Hello {
            protocol: PROTOCOL,
            build: options.build.clone(),
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            heartbeat_ms: options.heartbeat.as_millis() as u64,
            signing: options.signing.clone(),
            in_flight,
        }))?;
        let mut state = self.inner.lock();
        for execution in carried {
            state.store.mark_reported(&execution);
        }
        Ok(())
    }

    fn refuse(&self, call: u64, execution: &str, code: &str, message: impl Into<String>) {
        let _ = self.send(&Message::Result(CallResult {
            call,
            execution: execution.to_string(),
            replayed: false,
            outcome: Outcome::refused(code, message),
        }));
    }

    /// Tell the broker how many bytes of an upload the agent holds.
    fn progress(&self, call: u64, execution: &str, committed: u64) {
        if self.flow.v2() {
            let _ = self.send(&Message::Progress {
                call,
                execution: execution.to_string(),
                committed,
            });
        }
    }

    fn handle(&mut self, message: Message) {
        match message {
            Message::Call(call) => self.call(call),
            Message::Chunk { call, data } => self.chunk(call, &data),
            Message::End {
                call,
                execution,
                size,
                sha256,
            } => self.end(call, &execution, size, &sha256),
            Message::Query { call, execution } => {
                let state = self.inner.lock().execution_state(&execution);
                let _ = self.send(&Message::QueryResult {
                    call,
                    execution,
                    state,
                });
            }
            Message::Progress {
                call, committed, ..
            } => self.flow.ack(call, committed),
            // The broker's protocol: the lower of the two is spoken.
            Message::Ready(ready) => self
                .flow
                .protocol
                .store(ready.protocol.min(PROTOCOL), Ordering::SeqCst),
            // Heartbeats need no answer; the rest are the agent's own
            // messages, which a broker does not send.
            Message::Heartbeat
            | Message::Hello(_)
            | Message::Result(_)
            | Message::QueryResult { .. }
            | Message::Pending(_)
            | Message::Revoked { .. } => {}
        }
    }

    fn call(
        &mut self,
        Call {
            call,
            execution,
            op,
            lease,
            resume,
            offset,
        }: Call,
    ) {
        if execution.is_empty() || execution.len() > MAX_EXECUTION_ID {
            return self.refuse(
                call,
                &execution,
                "invalid_request",
                format!("an execution ID has 1 to {MAX_EXECUTION_ID} bytes"),
            );
        }
        let feeding = self.calls.contains_key(&call) && self.fed(call).is_some();
        if feeding || self.active.lock().unwrap().contains(&call) {
            return self.refuse(call, &execution, "call_in_use", "the call number is in use");
        }
        let timeout = match &op {
            Op::Exec { timeout_s, .. } | Op::Script { timeout_s } => *timeout_s,
            _ => 1.0,
        };
        if !(timeout > 0.0 && timeout.is_finite()) {
            return self.refuse(
                call,
                &execution,
                "invalid_request",
                "timeout_s must be positive",
            );
        }
        if let Some(known) = self.inner.lock().known(&execution) {
            return self.answer(call, execution, known, true, offset);
        }
        if op.kind().uploads() {
            return self.open_upload(call, execution, op, lease, resume);
        }
        self.commit(call, execution, op, lease, Committed::Nothing, offset);
    }

    /// Take an upload call: continue the upload this execution ID has when
    /// the call resumes it, else start one, and tell the sender where its
    /// chunks start. A call for an execution another call feeds supersedes
    /// that call, which has not reached its commit point and now never will.
    fn open_upload(
        &mut self,
        call: u64,
        execution: String,
        op: Op,
        lease: Option<String>,
        resume: bool,
    ) {
        let existing = self.inner.uploads().get(&execution).cloned();
        let kept = existing.and_then(|upload| {
            let mut held = lock(&upload);
            if let Some((number, old)) = held.owner.take() {
                if (number, old) != (self.number, call) {
                    if number == self.number {
                        self.calls.remove(&old);
                    }
                    self.inner.refuse_on(
                        number,
                        old,
                        &execution,
                        "superseded",
                        "a later call carries the same execution ID",
                    );
                }
            }
            let same = resume && held.op == op;
            drop(held);
            if same {
                Some(upload)
            } else {
                self.inner.discard_upload(&execution, "superseded", "");
                None
            }
        });
        let upload = match kept {
            Some(upload) => {
                let opened = lock(&upload).open_spool();
                match opened {
                    Ok(()) => upload,
                    Err(error) => {
                        self.inner.discard_upload(&execution, "", "");
                        return self.refuse(call, &execution, error.code, error.message);
                    }
                }
            }
            None => {
                let root = uploads::root(&self.inner.options.state);
                match Upload::create(&root, &self.inner.options.home, &execution, op, lease) {
                    Ok(upload) => {
                        let upload = Arc::new(Mutex::new(upload));
                        self.inner
                            .uploads()
                            .insert(execution.clone(), Arc::clone(&upload));
                        upload
                    }
                    Err(error) => return self.refuse(call, &execution, error.code, error.message),
                }
            }
        };
        let held = {
            let mut upload = lock(&upload);
            upload.owner = Some((self.number, call));
            upload.held()
        };
        self.calls.insert(
            call,
            Feeding {
                execution: execution.clone(),
                reported: held,
            },
        );
        self.progress(call, &execution, held);
    }

    /// The upload `call` feeds on this link, when it still does.
    fn fed(&mut self, call: u64) -> Option<(String, Arc<Mutex<Upload>>)> {
        let execution = self.calls.get(&call)?.execution.clone();
        let upload = self.inner.uploads().get(&execution).cloned();
        let owned = upload.filter(|upload| lock(upload).owner == Some((self.number, call)));
        if owned.is_none() {
            // Superseded, cancelled or expired: its chunks are dropped.
            self.calls.remove(&call);
        }
        owned.map(|upload| (execution, upload))
    }

    fn chunk(&mut self, call: u64, data: &[u8]) {
        // Chunks of a call that already failed may still be on the way.
        let Some((execution, upload)) = self.fed(call) else {
            return;
        };
        let appended = {
            let mut upload = lock(&upload);
            upload.append(data).map(|()| upload.held())
        };
        match appended {
            Ok(held) => {
                let feeding = self.calls.get_mut(&call).expect("a fed call");
                if held >= feeding.reported + PROGRESS_STEP {
                    feeding.reported = held;
                    self.progress(call, &execution, held);
                }
            }
            Err(error) => {
                self.calls.remove(&call);
                lock(&upload).owner = None;
                self.inner.discard_upload(&execution, "", "");
                self.refuse(call, &execution, error.code, error.message);
            }
        }
    }

    fn end(&mut self, call: u64, end_execution: &str, size: u64, sha256: &str) {
        let Some(feeding) = self.calls.get(&call) else {
            return;
        };
        if feeding.execution != end_execution {
            let execution = feeding.execution.clone();
            self.calls.remove(&call);
            self.inner.discard_upload(&execution, "", "");
            return self.refuse(
                call,
                &execution,
                "end_mismatch",
                "the end names another execution ID",
            );
        }
        let Some((execution, fed)) = self.fed(call) else {
            return;
        };
        self.calls.remove(&call);
        let checked = {
            let mut held = lock(&fed);
            held.owner = None;
            let checked = held
                .spool
                .as_mut()
                .expect("an open spool")
                .check(size, sha256);
            if checked.is_err() {
                let _ = held.sync();
            }
            checked
        };
        drop(fed);
        let digest = match checked {
            Ok(digest) => digest,
            // The sender continues from the agent's committed offset; a
            // broker of protocol 1 cannot, so it hears of a mismatch.
            Err(error) if error.code == "upload_incomplete" && self.flow.v2() => {
                return self.refuse(call, &execution, error.code, error.message)
            }
            Err(error) if error.code == "upload_incomplete" => {
                self.inner.discard_upload(&execution, "", "");
                return self.refuse(
                    call,
                    &execution,
                    "checksum_mismatch",
                    format!("the upload does not match its end: {}", error.message),
                );
            }
            Err(error) => {
                self.inner.discard_upload(&execution, "", "");
                return self.refuse(call, &execution, error.code, error.message);
            }
        };
        let Some(upload) = self.inner.uploads().remove(&execution) else {
            return self.refuse(call, &execution, "cancelled", "cancelled before its end");
        };
        let mut upload = match Arc::try_unwrap(upload) {
            Ok(mutex) => mutex.into_inner().unwrap_or_else(|p| p.into_inner()),
            Err(_) => return self.refuse(call, &execution, "superseded", "the upload moved on"),
        };
        let spool = upload.spool.take().expect("an open spool");
        let sealed = match &upload.landing {
            Landing::File {
                destination, mode, ..
            } => seal_spooled_file(spool, digest, destination, *mode).map(Committed::File),
            Landing::Tree {
                destination,
                staged,
                ..
            } => seal_spooled_tree(spool, digest, staged, destination).map(Committed::Tree),
            Landing::Input { .. } => {
                // An open file keeps the bytes once the record is gone.
                let input = spool.reader().ok();
                Ok(Committed::Input(input))
            }
        };
        upload.forget();
        let (op, lease) = (upload.op, upload.lease);
        match sealed {
            Ok(committed) => self.commit(call, execution, op, lease, committed, 0),
            Err(error) => self.refuse(call, &execution, error.code, error.message),
        }
    }

    /// The link ended: an upload it fed waits for a sender to continue it
    /// when the link spoke protocol 2, and is dropped otherwise, since a
    /// broker of protocol 1 sends a call whole again.
    fn release_uploads(&mut self) {
        let v2 = self.flow.v2();
        for (call, feeding) in self.calls.drain() {
            let upload = self.inner.uploads().get(&feeding.execution).cloned();
            let Some(upload) = upload else {
                continue;
            };
            let owned = {
                let mut held = lock(&upload);
                let owned = held.owner == Some((self.number, call));
                if owned {
                    held.owner = None;
                    if v2 {
                        let _ = held.sync();
                    }
                }
                owned
            };
            if owned && !v2 {
                self.inner.discard_upload(&feeding.execution, "", "");
            }
        }
    }

    /// Pass the commit point: record the execution, then start it.
    fn commit(
        &mut self,
        call: u64,
        execution: String,
        op: Op,
        lease: Option<String>,
        committed: Committed,
        offset: u64,
    ) {
        let inner = Arc::clone(&self.inner);
        let mut state = inner.lock();
        if let Some(known) = state.known(&execution) {
            drop(state);
            return self.answer(call, execution, known, true, offset);
        }
        if inner.stopping.load(Ordering::SeqCst) {
            drop(state);
            return self.refuse(call, &execution, "stopping", "the relay agent is stopping");
        }
        if let Op::Restart = op {
            return self.restart(state, call, execution, lease);
        }
        if let Op::Purge { lease: purged } = op {
            let outcome = purge(&mut state, &purged, lease.as_deref());
            drop(state);
            if matches!(outcome, Outcome::Purged { .. }) {
                let doomed: Vec<String> = inner
                    .uploads()
                    .iter()
                    .filter(|(_, upload)| lock(upload).lease.as_deref() == Some(purged.as_str()))
                    .map(|(execution, _)| execution.clone())
                    .collect();
                for upload in doomed {
                    inner.discard_upload(&upload, "cancelled", "the lease's records were purged");
                }
            }
            return send_result(&inner, &*self.link, call, &execution, false, outcome);
        }
        if let Err(error) = state.store.begin(&execution, op.kind(), lease.as_deref()) {
            drop(state);
            return self.refuse(
                call,
                &execution,
                "record_failed",
                format!("the execution could not be recorded: {error}"),
            );
        }
        let kind = op.kind();
        let known = match (op, committed) {
            (
                Op::Exec {
                    command,
                    stdin,
                    timeout_s,
                },
                Committed::Input(input),
            ) => {
                let input = input
                    .filter(|_| stdin)
                    .map(|file| Box::new(file) as Box<dyn Read + Send>);
                self.start_command(&mut state, &execution, kind, &command, input, timeout_s)
            }
            (Op::Script { timeout_s }, Committed::Input(input)) => {
                let command = guest_actions::script_command();
                let input = Some(match input {
                    Some(file) => Box::new(file) as Box<dyn Read + Send>,
                    None => Box::new(std::io::empty()),
                });
                self.start_command(&mut state, &execution, kind, command, input, timeout_s)
            }
            (Op::Write { .. }, Committed::File(sealed)) => {
                let outcome = match sealed.rename() {
                    Ok(digest) => Outcome::Written(Digest {
                        size: digest.size,
                        sha256: digest.sha256,
                    }),
                    Err(error) => Outcome::failed(error.code, error.message),
                };
                let _ = state.store.complete(&execution, outcome.clone());
                Known::Completed(kind, outcome)
            }
            (Op::WriteTree { .. }, Committed::Tree(sealed)) => {
                let outcome = match sealed.place() {
                    Ok(digest) => Outcome::Written(Digest {
                        size: digest.size,
                        sha256: digest.sha256,
                    }),
                    Err(error) => Outcome::failed(error.code, error.message),
                };
                let _ = state.store.complete(&execution, outcome.clone());
                Known::Completed(kind, outcome)
            }
            (Op::Read { path }, _) => {
                let live = new_live(kind);
                state.live.insert(execution.clone(), Arc::clone(&live));
                drop(state);
                return self.start_read(call, execution, live, path, false, offset);
            }
            (Op::ReadTree { path }, _) => {
                let live = new_live(kind);
                state.live.insert(execution.clone(), Arc::clone(&live));
                drop(state);
                return self.start_read(call, execution, live, path, true, offset);
            }
            (Op::Cancel { target }, _) => {
                let found = self.cancel(&state, &target);
                let outcome = Outcome::Cancel { target, found };
                let _ = state.store.complete(&execution, outcome.clone());
                Known::Completed(kind, outcome)
            }
            (Op::Status, _) => {
                let outcome = Outcome::Status(inner.status(&state));
                let _ = state.store.complete(&execution, outcome.clone());
                Known::Completed(kind, outcome)
            }
            (op, _) => {
                let outcome = Outcome::failed("internal", format!("{:?} has no upload", op.kind()));
                let _ = state.store.complete(&execution, outcome.clone());
                Known::Completed(kind, outcome)
            }
        };
        drop(state);
        self.answer(call, execution, known, false, offset);
    }

    /// A restart commits only while nothing else runs or is being answered,
    /// so no execution is in flight when the process is replaced; otherwise
    /// it is refused with `busy` and not recorded.
    /// Its record holds the status before the restart, so the agent that
    /// starts answers a repeated ID from it and never restarts twice.
    fn restart(
        &mut self,
        mut state: MutexGuard<'_, State>,
        call: u64,
        execution: String,
        lease: Option<String>,
    ) {
        let inner = Arc::clone(&self.inner);
        let Some(restart) = inner.restart.lock().unwrap().clone() else {
            drop(state);
            return self.refuse(
                call,
                &execution,
                "unsupported",
                "this agent cannot restart itself",
            );
        };
        // Held uploads do not count: their spools survive the restart, and
        // their senders continue them on the next session (RP-15).
        let others = self.active.lock().unwrap().len();
        if !state.live.is_empty() || others > 0 {
            let running = state.live.len() + others;
            drop(state);
            return self.refuse(
                call,
                &execution,
                "busy",
                format!("{running} operations run or wait; restart when none does"),
            );
        }
        let outcome = Outcome::Status(inner.status(&state));
        let recorded = state
            .store
            .begin(&execution, OpKind::Restart, lease.as_deref())
            .and_then(|()| state.store.complete(&execution, outcome.clone()));
        if let Err(error) = recorded {
            drop(state);
            return self.refuse(
                call,
                &execution,
                "record_failed",
                format!("the restart could not be recorded: {error}"),
            );
        }
        // Nothing commits from here on; a call that arrives now is refused
        // with `stopping`, which the broker sends again to the next agent.
        inner.stopping.store(true, Ordering::SeqCst);
        drop(state);
        send_result(&inner, &*self.link, call, &execution, false, outcome);
        for link in inner.links.lock().unwrap().values() {
            link.flush(Duration::from_secs(2));
        }
        let uploads: Vec<_> = inner.uploads().values().cloned().collect();
        for upload in uploads {
            let _ = lock(&upload).sync();
        }
        let error = restart();
        // The process was not replaced: carry on as before.
        eprintln!("relay-agent: the restart failed: {error}");
        inner.stopping.store(false, Ordering::SeqCst);
    }

    fn cancel(&mut self, state: &State, target: &str) -> CancelFound {
        if let Some(live) = state.live.get(target) {
            live.cancelled.store(true, Ordering::SeqCst);
            live.kill();
            return CancelFound::Signalled;
        }
        if state.store.get(target).is_some() {
            return CancelFound::Completed;
        }
        self.calls.retain(|_, feeding| feeding.execution != target);
        if self
            .inner
            .discard_upload(target, "cancelled", "cancelled before its end")
        {
            CancelFound::Discarded
        } else {
            CancelFound::Unknown
        }
    }

    fn start_command(
        &self,
        state: &mut State,
        execution: &str,
        kind: OpKind,
        command: &str,
        stdin: Option<Box<dyn Read + Send>>,
        timeout_s: f64,
    ) -> Known {
        let options = &self.inner.options;
        // The scope is named before the command starts, so the record names
        // it before anything can run in it; a group is known only once the
        // command has started, and is recorded at once (DC-139).
        let scope = options.ownership.scope_for(&Store::key(execution));
        let output = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(state.store.output(execution));
        let started = output
            .map_err(|error| ("record_failed", error.to_string()))
            .and_then(|output| match &scope {
                Some(unit) => state
                    .store
                    .set_owner(execution, Owner::Scope { unit: unit.clone() })
                    .map(|()| output)
                    .map_err(|error| ("record_failed", error.to_string())),
                None => Ok(output),
            })
            .and_then(|output| {
                guest_actions::spawn_shell_in(
                    &options.shell,
                    &options.home,
                    command,
                    stdin.is_some(),
                    scope.as_deref(),
                )
                .map(|spawned| (output, spawned))
                .map_err(|error| ("spawn_failed", error.to_string()))
            });
        let (mut output, spawned) = match started {
            Ok(started) => started,
            Err((code, message)) => {
                let outcome = Outcome::failed(code, message);
                let _ = state.store.complete(execution, outcome.clone());
                return Known::Completed(kind, outcome);
            }
        };
        if scope.is_none() {
            let started = guest_actions::ownership::process_start(spawned.group);
            let _ = state.store.set_owner(
                execution,
                Owner::Group {
                    group: spawned.group,
                    started: started.unwrap_or_default(),
                },
            );
        }
        let live = new_live_command(kind, spawned.group, scope);
        state.live.insert(execution.to_string(), Arc::clone(&live));
        let inner = Arc::clone(&self.inner);
        let execution = execution.to_string();
        let supervised = Arc::clone(&live);
        thread::spawn(move || {
            let live = supervised;
            let mut tally = Tally::default();
            let exit = spawned.supervise_from(stdin, Duration::from_secs_f64(timeout_s), |bytes| {
                if output.write_all(bytes).is_err() {
                    return false;
                }
                tally.update(bytes);
                live.progress.lock().unwrap().written = tally.size();
                live.changed.notify_all();
                true
            });
            let _ = output.sync_all();
            let digest = tally.finish();
            let outcome = if inner.stopping.load(Ordering::SeqCst) {
                Outcome::Interrupted {
                    message: "the relay agent stopped while the command ran".into(),
                }
            } else {
                Outcome::Exit {
                    rc: exit.rc,
                    signal: exit.signal,
                    timed_out: exit.timed_out,
                    cancelled: live.cancelled.load(Ordering::SeqCst),
                    output: Digest {
                        size: digest.size,
                        sha256: digest.sha256,
                    },
                }
            };
            inner.settle(&execution, &live, outcome);
        });
        Known::Running(live)
    }

    /// Stream a file's bytes, or a folder's tar stream when `tree` is set,
    /// from byte `offset` on; the end names the whole stream.
    fn start_read(
        &self,
        call: u64,
        execution: String,
        live: Arc<Live>,
        path: String,
        tree: bool,
        offset: u64,
    ) {
        let inner = Arc::clone(&self.inner);
        let link = Arc::clone(&self.link);
        let active = Arc::clone(&self.active);
        let flow = Arc::clone(&self.flow);
        active.lock().unwrap().insert(call);
        thread::spawn(move || {
            let path = guest_actions::resolve(&path, &inner.options.home);
            let mut stream = Outgoing::new(call, offset, &*link, &flow);
            let cancelled = || live.cancelled.load(Ordering::SeqCst);
            let send = |bytes: &[u8]| !cancelled() && stream.send(bytes, &cancelled);
            let read = if tree {
                guest_actions::read_tree(&path, MAX_CHUNK, send)
            } else {
                guest_actions::open_read(&path)
                    .and_then(|file| guest_actions::read_chunks(file, MAX_CHUNK, send))
            };
            let outcome = match read {
                Ok(digest) => {
                    let _ = link.send(
                        Message::End {
                            call,
                            execution: execution.clone(),
                            size: digest.size,
                            sha256: digest.sha256.clone(),
                        }
                        .encode(),
                    );
                    Outcome::Read(Digest {
                        size: digest.size,
                        sha256: digest.sha256,
                    })
                }
                Err(_) if live.cancelled.load(Ordering::SeqCst) => {
                    Outcome::failed("cancelled", "the read was cancelled")
                }
                Err(error) => Outcome::failed(error.code, error.message),
            };
            inner.settle(&execution, &live, outcome.clone());
            send_result(&inner, &*link, call, &execution, false, outcome);
            flow.forget(call);
            active.lock().unwrap().remove(&call);
        });
    }

    /// Answer a call from what its execution ID has: replay a command's
    /// recorded output from byte `offset` and follow it while it runs, then
    /// send the outcome.
    fn answer(&self, call: u64, execution: String, known: Known, replayed: bool, offset: u64) {
        let inner = Arc::clone(&self.inner);
        let link = Arc::clone(&self.link);
        let active = Arc::clone(&self.active);
        let flow = Arc::clone(&self.flow);
        let op = match &known {
            Known::Running(live) => live.op,
            Known::Completed(op, _) => *op,
        };
        active.lock().unwrap().insert(call);
        thread::spawn(move || {
            let streamed = if op.streams_output() {
                let stream = Outgoing::new(call, offset, &*link, &flow);
                stream_output(&inner, stream, &execution, &known)
            } else {
                Ok(())
            };
            if streamed.is_ok() {
                let outcome = match known {
                    Known::Running(live) => wait_outcome(&live),
                    Known::Completed(_, outcome) => outcome,
                };
                send_result(&inner, &*link, call, &execution, replayed, outcome);
            }
            flow.forget(call);
            active.lock().unwrap().remove(&call);
        });
    }
}

/// The stream of one call's answer as the agent sends it: bytes before
/// `offset` are counted and not sent, and with protocol 2 at most
/// [`UPLOAD_WINDOW`] bytes are sent beyond the broker's acknowledgement.
struct Outgoing<'a> {
    call: u64,
    offset: u64,
    /// Bytes of the stream so far, sent or skipped.
    position: u64,
    link: &'a dyn Link,
    flow: &'a Flow,
}

impl<'a> Outgoing<'a> {
    fn new(call: u64, offset: u64, link: &'a dyn Link, flow: &'a Flow) -> Outgoing<'a> {
        Outgoing {
            call,
            offset,
            position: 0,
            link,
            flow,
        }
    }

    /// Send the stream's next bytes; false when nobody can receive them.
    fn send(&mut self, bytes: &[u8], stop: &dyn Fn() -> bool) -> bool {
        let start = self.position;
        self.position += bytes.len() as u64;
        if self.position <= self.offset {
            return true;
        }
        let skip = self.offset.saturating_sub(start) as usize;
        let fresh = &bytes[skip..];
        let sent = self.position - self.offset;
        if !self.flow.wait(self.call, sent, stop) {
            return false;
        }
        self.link
            .send(
                Message::Chunk {
                    call: self.call,
                    data: fresh.to_vec(),
                }
                .encode(),
            )
            .is_ok()
    }
}

/// Remove the records of `lease` (DC-173), when the call that asks belongs
/// to that lease and none of the lease's operations still runs. The purge
/// keeps no record: purging again finds nothing more.
fn purge(state: &mut State, lease: &str, caller: Option<&str>) -> Outcome {
    if caller != Some(lease) {
        return Outcome::refused(
            "forbidden",
            "a purge must belong to the lease whose records it removes",
        );
    }
    let running = state
        .live
        .keys()
        .filter(|execution| {
            state
                .store
                .get(execution)
                .is_some_and(|record| record.lease.as_deref() == Some(lease))
        })
        .count();
    if running > 0 {
        return Outcome::refused(
            "busy",
            format!("{running} operations of the lease still run; purge when none does"),
        );
    }
    match state.store.purge(lease) {
        Ok((records, bytes)) => Outcome::Purged {
            lease: lease.to_string(),
            records,
            bytes,
        },
        Err(error) => Outcome::failed(
            "purge_failed",
            format!("the lease's records could not all be removed: {error}"),
        ),
    }
}

fn new_live(op: OpKind) -> Arc<Live> {
    new_live_command(op, 0, None)
}

fn new_live_command(op: OpKind, group: i32, scope: Option<String>) -> Arc<Live> {
    Arc::new(Live {
        op,
        group: AtomicI32::new(group),
        scope,
        cancelled: AtomicBool::new(false),
        progress: Mutex::new(Progress::default()),
        changed: Condvar::new(),
    })
}

impl Live {
    /// Kill a command's processes: its group, and its scope when it has one.
    fn kill(&self) {
        kill_command(self.group.load(Ordering::SeqCst), self.scope.as_deref());
    }
}

fn wait_outcome(live: &Live) -> Outcome {
    let mut progress = live.progress.lock().unwrap();
    loop {
        if let Some(outcome) = &progress.outcome {
            return outcome.clone();
        }
        progress = live.changed.wait(progress).unwrap();
    }
}

fn send_result(
    inner: &Inner,
    link: &dyn Link,
    call: u64,
    execution: &str,
    replayed: bool,
    outcome: Outcome,
) {
    let sent = link.send(
        Message::Result(CallResult {
            call,
            execution: execution.to_string(),
            replayed,
            outcome,
        })
        .encode(),
    );
    if sent.is_ok() {
        inner.lock().store.mark_reported(execution);
    }
}

/// Send a command's output from its record, following the record while the
/// command runs, then the end with the size and SHA-256 of the whole
/// output.
fn stream_output(
    inner: &Inner,
    mut stream: Outgoing<'_>,
    execution: &str,
    known: &Known,
) -> Result<(), LinkError> {
    let (call, link) = (stream.call, stream.link);
    let path = inner.lock().store.output(execution);
    let mut file = File::open(path).ok();
    let mut tally = Tally::default();
    let mut offset = 0u64;
    let mut buffer = vec![0u8; MAX_CHUNK];
    let stop = || inner.stopping.load(Ordering::SeqCst);
    loop {
        let (available, done) = match known {
            Known::Running(live) => {
                let mut progress = live.progress.lock().unwrap();
                while progress.written == offset && progress.outcome.is_none() {
                    progress = live.changed.wait(progress).unwrap();
                }
                (progress.written, progress.outcome.is_some())
            }
            Known::Completed(..) => {
                let length = file
                    .as_ref()
                    .and_then(|file| file.metadata().ok())
                    .map_or(0, |metadata| metadata.len());
                (length, true)
            }
        };
        while offset < available {
            let want = (available - offset).min(MAX_CHUNK as u64) as usize;
            let read = file.as_mut().map(|file| {
                file.seek(SeekFrom::Start(offset))
                    .and_then(|_| file.read(&mut buffer[..want]))
            });
            let count = match read {
                Some(Ok(count)) if count > 0 => count,
                _ => {
                    file = None;
                    break;
                }
            };
            tally.update(&buffer[..count]);
            offset += count as u64;
            if !stream.send(&buffer[..count], &stop) {
                return Err(LinkError::Closed);
            }
        }
        if file.is_none() {
            // The output record cannot be read; the end reports what was
            // read, and the outcome what the command wrote.
            if let Known::Running(live) = known {
                wait_outcome(live);
            }
            break;
        }
        if done {
            break;
        }
    }
    let digest = tally.finish();
    link.send(
        Message::End {
            call,
            execution: execution.to_string(),
            size: digest.size,
            sha256: digest.sha256,
        }
        .encode(),
    )
}

/// Replace this process with `executable`, given the arguments the process
/// started with. The descriptors the agent opened close on `exec`, except
/// standard input and output, which `relay-agent stdio` keeps serving.
/// Returns only when the replacement fails.
pub fn reexec(executable: &std::path::Path) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    std::process::Command::new(executable)
        .args(std::env::args_os().skip(1))
        .exec()
}
