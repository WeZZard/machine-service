//! The relay agent's core (`docs/arch/relay-agent.md`, RA-5).
//!
//! An [`Agent`] owns the durable execution records and the running
//! operations. [`Agent::serve`] answers one link until it closes; the agent
//! outlives its links, so a command keeps running while the broker
//! reconnects, and its output waits in the record for whoever asks next.
//!
//! Each call has one commit point. `exec`, `script` and `write` commit at
//! their end message, after the uploaded bytes matched it; `read`, `cancel`
//! and `status` commit when they arrive. The record reaches the disk at the
//! commit point, before the operation takes effect, so an execution ID runs
//! at most once, even across a restart of the agent. A call with a recorded
//! execution ID never runs again: it receives the recorded outcome, or it
//! attaches to the operation still running.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use guest_actions::ownership::{Owner, Ownership};
use guest_actions::{kill_command, AtomicFile, AtomicTree, Sealed, SealedTree, Tally};

use crate::link::{Link, LinkError};
use crate::protocol::{
    Call, CallResult, CancelFound, Digest, ExecutionState, Hello, Message, Op, OpKind, Outcome,
    Report, Status, MAX_CHUNK, PROTOCOL,
};
use crate::store::Store;

/// How often the agent sends a heartbeat; well under TC-3's 30 seconds.
pub const HEARTBEAT: Duration = Duration::from_secs(10);
/// The longest the agent keeps a completed execution's record. A record
/// goes when the broker purges its lease at release (DC-173); this bound
/// only removes the records of a lease whose purge never came, such as
/// after a crash of machine-service (DC-175).
pub const MAX_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// The largest standard input or script a call may upload.
pub const MAX_UPLOAD: usize = 256 << 20;
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
}

impl AgentOptions {
    /// Options from the process environment: `$SHELL`, `$HOME`, the state
    /// directory `~/.local/state/relay-agent`, the hash of the running
    /// executable, [`HEARTBEAT`], [`MAX_RETENTION`], and the ownership this
    /// machine supports ([`Ownership::detect`]).
    pub fn from_environment() -> AgentOptions {
        let home = guest_actions::user_home();
        AgentOptions {
            shell: guest_actions::user_shell(),
            state: state_directory(&home),
            home,
            build: guest_actions::executable_sha256(),
            heartbeat: HEARTBEAT,
            retention: MAX_RETENTION,
            ownership: Ownership::detect(),
            executable: std::env::current_exe().unwrap_or_default(),
        }
    }
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

/// A call waiting for its end message.
struct Pending {
    execution: String,
    op: Op,
    lease: Option<String>,
    upload: Upload,
}

enum Upload {
    Bytes(Vec<u8>, Tally),
    /// Dropping the file removes its temporary sibling.
    File(AtomicFile),
    /// Dropping the tree removes its spool and its unpacked folder.
    Tree(AtomicTree),
}

/// What a call brings to its commit point.
enum Committed {
    Bytes(Vec<u8>),
    File(Sealed),
    Tree(SealedTree),
    Nothing,
}

/// One link being served.
struct Session {
    inner: Arc<Inner>,
    link: Arc<dyn Link>,
    pending: HashMap<u64, Pending>,
    /// Calls whose answer is still being sent.
    active: Arc<Mutex<HashSet<u64>>>,
}

impl Agent {
    /// Open the agent's records under `options.state`.
    pub fn open(options: AgentOptions) -> std::io::Result<Agent> {
        let store = Store::open(&options.state, options.retention)?;
        Ok(Agent {
            inner: Arc::new(Inner {
                options,
                state: Mutex::new(State {
                    store,
                    live: HashMap::new(),
                }),
                settled: Condvar::new(),
                links: Mutex::new(HashMap::new()),
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
    /// message. Operations still running when the link closes keep running.
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
            pending: HashMap::new(),
            active: Arc::default(),
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
        drop(session);
        self.inner.links.lock().unwrap().remove(&number);
        served
    }

    /// Stop the agent: close every link, kill the process group of every
    /// running command, and wait up to `grace` for running operations to
    /// record their end. An operation stopped this way records
    /// [`Outcome::Interrupted`]; one that does not end in time is recorded so
    /// when the records are next opened.
    pub fn shutdown(&self, grace: Duration) {
        self.inner.stopping.store(true, Ordering::SeqCst);
        for link in self.inner.links.lock().unwrap().values() {
            link.close();
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
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
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
}

impl Session {
    fn send(&self, message: &Message) -> Result<(), LinkError> {
        self.link.send(message.encode())
    }

    fn hello(&self) -> Result<(), LinkError> {
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
            // Heartbeats need no answer; the rest are the agent's own
            // messages, which a broker does not send.
            Message::Heartbeat
            | Message::Hello(_)
            | Message::Result(_)
            | Message::QueryResult { .. }
            | Message::Ready(_)
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
        if self.pending.contains_key(&call) || self.active.lock().unwrap().contains(&call) {
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
            return self.answer(call, execution, known, true);
        }
        // A call resent before its commit point replaces the first, which
        // has not run and now never will.
        let superseded: Vec<u64> = self
            .pending
            .iter()
            .filter(|(_, pending)| pending.execution == execution)
            .map(|(number, _)| *number)
            .collect();
        for number in superseded {
            self.pending.remove(&number);
            self.refuse(
                number,
                &execution,
                "superseded",
                "a later call carries the same execution ID",
            );
        }
        let upload = match &op {
            Op::Exec { .. } | Op::Script { .. } => Upload::Bytes(Vec::new(), Tally::default()),
            Op::Write { path, name, mode } => {
                let unique = self.inner.next.fetch_add(1, Ordering::SeqCst);
                match AtomicFile::create(
                    path,
                    name,
                    *mode,
                    &self.inner.options.home,
                    "relay-agent",
                    unique,
                ) {
                    Ok(file) => Upload::File(file),
                    Err(error) => return self.refuse(call, &execution, error.code, error.message),
                }
            }
            Op::WriteTree { path, name } => {
                let unique = self.inner.next.fetch_add(1, Ordering::SeqCst);
                match AtomicTree::create(
                    path,
                    name,
                    &self.inner.options.home,
                    "relay-agent",
                    unique,
                ) {
                    Ok(tree) => Upload::Tree(tree),
                    Err(error) => return self.refuse(call, &execution, error.code, error.message),
                }
            }
            Op::Read { .. }
            | Op::ReadTree { .. }
            | Op::Cancel { .. }
            | Op::Status
            | Op::Restart
            | Op::Purge { .. } => {
                return self.commit(call, execution, op, lease, Committed::Nothing);
            }
        };
        self.pending.insert(
            call,
            Pending {
                execution,
                op,
                lease,
                upload,
            },
        );
    }

    fn chunk(&mut self, call: u64, data: &[u8]) {
        // Chunks of a call that already failed may still be on the way.
        let Some(pending) = self.pending.get_mut(&call) else {
            return;
        };
        let failed = match &mut pending.upload {
            Upload::Bytes(bytes, tally) => {
                if bytes.len() + data.len() > MAX_UPLOAD {
                    Some(("too_large", "the upload exceeds the limit".to_string()))
                } else {
                    bytes.extend_from_slice(data);
                    tally.update(data);
                    None
                }
            }
            Upload::File(file) => file
                .append(data)
                .err()
                .map(|error| (error.code, error.message)),
            Upload::Tree(tree) => tree
                .append(data)
                .err()
                .map(|error| (error.code, error.message)),
        };
        if let Some((code, message)) = failed {
            let pending = self.pending.remove(&call).expect("a pending call");
            self.refuse(call, &pending.execution, code, message);
        }
    }

    fn end(&mut self, call: u64, end_execution: &str, size: u64, sha256: &str) {
        let Some(Pending {
            execution,
            op,
            lease,
            upload,
        }) = self.pending.remove(&call)
        else {
            return;
        };
        if execution != end_execution {
            return self.refuse(
                call,
                &execution,
                "end_mismatch",
                "the end names another execution ID",
            );
        }
        let committed = match upload {
            Upload::Bytes(bytes, tally) => {
                let digest = tally.finish();
                if digest.size != size || digest.sha256 != sha256 {
                    return self.refuse(
                        call,
                        &execution,
                        "checksum_mismatch",
                        "the upload does not match its end",
                    );
                }
                Committed::Bytes(bytes)
            }
            Upload::File(file) => match file.seal(size, sha256) {
                Ok(sealed) => Committed::File(sealed),
                Err(error) => return self.refuse(call, &execution, error.code, error.message),
            },
            Upload::Tree(tree) => match tree.seal(size, sha256) {
                Ok(sealed) => Committed::Tree(sealed),
                Err(error) => return self.refuse(call, &execution, error.code, error.message),
            },
        };
        self.commit(call, execution, op, lease, committed);
    }

    /// Pass the commit point: record the execution, then start it.
    fn commit(
        &mut self,
        call: u64,
        execution: String,
        op: Op,
        lease: Option<String>,
        committed: Committed,
    ) {
        let inner = Arc::clone(&self.inner);
        let mut state = inner.lock();
        if let Some(known) = state.known(&execution) {
            drop(state);
            return self.answer(call, execution, known, true);
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
                Committed::Bytes(bytes),
            ) => {
                let input = stdin.then_some(bytes);
                self.start_command(&mut state, &execution, kind, &command, input, timeout_s)
            }
            (Op::Script { timeout_s }, Committed::Bytes(bytes)) => {
                let command = guest_actions::script_command();
                self.start_command(
                    &mut state,
                    &execution,
                    kind,
                    command,
                    Some(bytes),
                    timeout_s,
                )
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
                return self.start_read(call, execution, live, path, false);
            }
            (Op::ReadTree { path }, _) => {
                let live = new_live(kind);
                state.live.insert(execution.clone(), Arc::clone(&live));
                drop(state);
                return self.start_read(call, execution, live, path, true);
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
        self.answer(call, execution, known, false);
    }

    /// A restart commits only while nothing else runs or waits for its end
    /// on this link, so no execution is in flight when the process is
    /// replaced; otherwise it is refused with `busy` and not recorded. Its
    /// record holds the status before the restart, so the agent that
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
        let others = self.active.lock().unwrap().len() + self.pending.len();
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
        let pending: Vec<u64> = self
            .pending
            .iter()
            .filter(|(_, pending)| pending.execution == target)
            .map(|(number, _)| *number)
            .collect();
        if pending.is_empty() {
            return CancelFound::Unknown;
        }
        for number in pending {
            self.pending.remove(&number);
            self.refuse(number, target, "cancelled", "cancelled before its end");
        }
        CancelFound::Discarded
    }

    fn start_command(
        &self,
        state: &mut State,
        execution: &str,
        kind: OpKind,
        command: &str,
        stdin: Option<Vec<u8>>,
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
            let exit = spawned.supervise(stdin, Duration::from_secs_f64(timeout_s), |bytes| {
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

    /// Stream a file's bytes, or a folder's tar stream when `tree` is set.
    fn start_read(&self, call: u64, execution: String, live: Arc<Live>, path: String, tree: bool) {
        let inner = Arc::clone(&self.inner);
        let link = Arc::clone(&self.link);
        let active = Arc::clone(&self.active);
        active.lock().unwrap().insert(call);
        thread::spawn(move || {
            let path = guest_actions::resolve(&path, &inner.options.home);
            let send = |bytes: &[u8]| {
                !live.cancelled.load(Ordering::SeqCst)
                    && link
                        .send(
                            Message::Chunk {
                                call,
                                data: bytes.to_vec(),
                            }
                            .encode(),
                        )
                        .is_ok()
            };
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
            active.lock().unwrap().remove(&call);
        });
    }

    /// Answer a call from what its execution ID has: replay a command's
    /// recorded output and follow it while it runs, then send the outcome.
    fn answer(&self, call: u64, execution: String, known: Known, replayed: bool) {
        let inner = Arc::clone(&self.inner);
        let link = Arc::clone(&self.link);
        let active = Arc::clone(&self.active);
        let op = match &known {
            Known::Running(live) => live.op,
            Known::Completed(op, _) => *op,
        };
        active.lock().unwrap().insert(call);
        thread::spawn(move || {
            let streamed = if op.streams_output() {
                stream_output(&inner, &*link, call, &execution, &known)
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
            active.lock().unwrap().remove(&call);
        });
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
/// command runs, then the end with the size and SHA-256 of what was sent.
fn stream_output(
    inner: &Inner,
    link: &dyn Link,
    call: u64,
    execution: &str,
    known: &Known,
) -> Result<(), LinkError> {
    let path = inner.lock().store.output(execution);
    let mut file = File::open(path).ok();
    let mut tally = Tally::default();
    let mut offset = 0u64;
    let mut buffer = vec![0u8; MAX_CHUNK];
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
            link.send(
                Message::Chunk {
                    call,
                    data: buffer[..count].to_vec(),
                }
                .encode(),
            )?;
        }
        if file.is_none() {
            // The output record cannot be read; the end reports what was
            // sent, and the outcome what the command wrote.
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
