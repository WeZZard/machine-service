//! The broker (`docs/arch/broker.md`, BK; wire contract
//! `docs/arch/relay-protocol.md`, RP): relay agents and clients both open
//! resumable sessions to it, and it routes each client's calls to the relay
//! agent of the client's lease.
//!
//! - [`Broker`] holds the credentials, the agent sessions, the client
//!   sessions and the execution records, and serves a
//!   [`remote_session::SessionServer`].
//! - [`Broker::run`] and [`Broker::query`] are the same calls for a caller in
//!   the same process, such as machine-service's HTTP exec, push and pull.
//! - [`tls`] makes the broker's authority and certificates; [`net`] reads the
//!   host's names and addresses.

mod agent;
mod client;
mod credentials;
mod job;
pub mod net;
pub mod tls;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use relay_agent::protocol::{AgentInfo, Digest, ExecutionState, Op, Outcome};
use remote_session::{AuthRequest, Authenticator, Rejection, Session, SessionServer};

use crate::agent::{AgentConn, AgentEvent};
pub use crate::credentials::{credential_sha256, new_credential, Role, Target, Validity};
use crate::credentials::{Grant, Grants};
use crate::job::Job;
pub use crate::job::JobEvent;

/// How the broker behaves.
#[derive(Debug, Clone)]
pub struct BrokerConfig {
    /// How long a call waits for a lease's relay agent to register (RP-8);
    /// by default the agent session's grace period.
    pub agent_wait: Duration,
    /// How long a finished execution record is kept (BK-3: at least 1 h).
    pub retention: Duration,
    /// The largest upload a call may carry (RP-5).
    pub max_upload: usize,
    /// The largest exec or script output kept in a record for late callers;
    /// beyond it, a late caller gets a replay from the agent.
    pub max_kept_output: usize,
    /// How long an agent session gets to send its hello.
    pub hello_timeout: Duration,
    /// How long a query waits for the agent's answer on a live session.
    pub query_timeout: Duration,
}

impl Default for BrokerConfig {
    fn default() -> Self {
        BrokerConfig {
            agent_wait: remote_session::DEFAULT_GRACE,
            retention: Duration::from_secs(3600),
            max_upload: 256 << 20,
            max_kept_output: 16 << 20,
            hello_timeout: Duration::from_secs(30),
            query_timeout: Duration::from_secs(30),
        }
    }
}

/// Why no agent session could take a call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Unavailable {
    Revoked(String),
    Timeout,
}

#[derive(Default)]
struct Links {
    agents: HashMap<Target, Arc<AgentConn>>,
    revoked: HashMap<Target, String>,
    clients: HashMap<Target, Vec<(u64, Session)>>,
    /// Targets whose relay agent is restarting: no call is handed to them
    /// until the agent's next session registers.
    held: HashSet<Target>,
}

/// Called with every relay agent that registers.
type AgentWatcher = dyn Fn(&Target, &AgentInfo) + Send + Sync;

/// Why a relay agent was not restarted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartError {
    /// Calls kept running for the whole drain bound, or the agent refused
    /// because one runs; nothing was restarted.
    Busy,
    /// The agent has no session, or its lease was revoked.
    Unavailable,
    /// The agent took the restart but no new session registered in time.
    NotBack,
    /// The agent refused the restart.
    Refused(String),
}

pub(crate) struct Inner {
    pub config: BrokerConfig,
    grants: Mutex<Grants>,
    principals: Mutex<HashMap<String, (Role, Target)>>,
    links: Mutex<Links>,
    links_changed: Condvar,
    jobs: Mutex<HashMap<(Target, String), Arc<Job>>>,
    next_id: AtomicU64,
    pub log: Arc<dyn Fn(&str) + Send + Sync>,
    watchers: Mutex<Vec<Arc<AgentWatcher>>>,
}

/// What an in-process call answered.
#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    /// The stream's bytes: an exec's or script's output, or a read's file.
    pub bytes: Vec<u8>,
    /// The stream's end.
    pub end: Option<Digest>,
    /// Whether the execution ID had run before.
    pub replayed: bool,
    /// The outcome.
    pub outcome: Outcome,
    /// Whether the call waited for the relay agent to register.
    pub waited: bool,
}

impl Answer {
    /// Whether the bytes match the stream's end.
    pub fn end_matches(&self) -> bool {
        self.end.as_ref().is_some_and(|end| {
            end.size == self.bytes.len() as u64
                && end.sha256 == guest_actions::sha256_hex(&self.bytes)
        })
    }
}

/// Why an in-process call has no outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunError {
    /// The caller's bound passed first. The call still runs, and its
    /// outcome stays in the record for a query.
    Timeout,
    /// The broker dropped the call; this is a bug.
    Internal,
}

/// The broker. Clones share it.
#[derive(Clone)]
pub struct Broker {
    inner: Arc<Inner>,
}

impl Broker {
    /// A broker that logs through `log`.
    pub fn new(config: BrokerConfig, log: impl Fn(&str) + Send + Sync + 'static) -> Broker {
        Broker {
            inner: Arc::new(Inner {
                config,
                grants: Mutex::new(Grants::default()),
                principals: Mutex::new(HashMap::new()),
                links: Mutex::new(Links::default()),
                links_changed: Condvar::new(),
                jobs: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(1),
                log: Arc::new(log),
                watchers: Mutex::new(Vec::new()),
            }),
        }
    }

    /// The broker's configuration.
    pub fn config(&self) -> &BrokerConfig {
        &self.inner.config
    }

    /// The authenticator a [`remote_session::ServerConfig`] needs: it checks
    /// the hello's credential and role on every hello, resumes included.
    pub fn authenticator(&self) -> impl Authenticator {
        let inner = Arc::clone(&self.inner);
        move |request: &AuthRequest<'_>| inner.authenticate(request)
    }

    /// Serve every session `server` accepts, each on a thread of its own.
    pub fn serve(&self, server: SessionServer) {
        let inner = Arc::clone(&self.inner);
        std::thread::Builder::new()
            .name("relay-broker-accept".into())
            .spawn(move || {
                while let Ok(session) = server.accept() {
                    let inner = Arc::clone(&inner);
                    let _ = std::thread::Builder::new()
                        .name("relay-broker-session".into())
                        .spawn(move || inner.dispatch(session));
                }
            })
            .expect("spawn the broker's accept thread");
    }

    /// Issue a new credential for `role` on `target`, and return it. Only its
    /// SHA-256 is kept.
    pub fn issue(&self, target: &Target, role: Role, valid: Option<Validity>) -> String {
        let credential = new_credential();
        self.admit(target, role, &credential_sha256(&credential), valid);
        credential
    }

    /// Accept a credential by its SHA-256, as kept in a lease record, such
    /// as after a restart.
    pub fn admit(&self, target: &Target, role: Role, sha256: &str, valid: Option<Validity>) {
        self.inner.links().revoked.remove(target);
        self.inner.grants().admit(
            sha256.to_string(),
            Grant {
                role,
                target: target.clone(),
                valid,
            },
        );
    }

    /// Refuse every credential of `target` from now on, end its client and
    /// agent sessions, and end every call that waits for its agent (RP-10).
    /// `reason` is `released` or `expired`.
    pub fn revoke(&self, target: &Target, reason: &str) {
        let removed = self
            .inner
            .grants()
            .revoke(target, &[Role::Agent, Role::Client]);
        self.inner
            .forget_principals(target, &[Role::Agent, Role::Client]);
        let (agent, clients) = {
            let mut links = self.inner.links();
            links.revoked.insert(target.clone(), reason.to_string());
            (
                links.agents.remove(target),
                links.clients.remove(target).unwrap_or_default(),
            )
        };
        self.inner.links_changed.notify_all();
        Inner::end_clients(clients, reason);
        if let Some(agent) = agent {
            agent.close();
        }
        if removed > 0 {
            (self.inner.log)(&format!("relay: {target}: credentials revoked ({reason})"));
        }
    }

    /// Refuse the client credentials of `target` and end its client
    /// sessions, keeping its agent, as a durable machine's lease ends.
    pub fn revoke_clients(&self, target: &Target, reason: &str) {
        self.inner.grants().revoke(target, &[Role::Client]);
        self.inner.forget_principals(target, &[Role::Client]);
        let clients = self
            .inner
            .links()
            .clients
            .remove(target)
            .unwrap_or_default();
        Inner::end_clients(clients, reason);
    }

    /// End the relay agent's current session, as a session that expired
    /// would end; the agent dials a new one, and calls that were out are
    /// sent to it again. Returns whether there was a session.
    pub fn end_agent_session(&self, target: &Target) -> bool {
        let conn = self.inner.links().agents.get(target).cloned();
        match conn {
            Some(conn) => {
                conn.close();
                true
            }
            None => false,
        }
    }

    /// Whether any credential of `role` on `target` is accepted.
    pub fn has_credential(&self, target: &Target, role: Role) -> bool {
        self.inner.grants().has(target, role)
    }

    /// The relay agent of `target`, when it has a session.
    pub fn agent(&self, target: &Target) -> Option<AgentInfo> {
        self.inner.agent_info(target)
    }

    /// Wait up to `timeout` for the relay agent of `target` to register.
    pub fn wait_for_agent(&self, target: &Target, timeout: Duration) -> Option<AgentInfo> {
        self.inner
            .wait_agent(target, timeout, &|_| {})
            .ok()
            .map(|conn| conn.info())
    }

    /// Start or attach to one execution, as a client's call does, and
    /// receive its events.
    pub fn submit(
        &self,
        target: &Target,
        execution: &str,
        op: Op,
        upload: Vec<u8>,
    ) -> std::sync::mpsc::Receiver<JobEvent> {
        self.inner.submit(target, execution, op, upload)
    }

    /// Run one call through the lease's relay agent and wait up to `timeout`
    /// for its outcome.
    pub fn run(
        &self,
        target: &Target,
        execution: &str,
        op: Op,
        upload: Vec<u8>,
        timeout: Duration,
    ) -> Result<Answer, RunError> {
        let events = self.submit(target, execution, op, upload);
        let deadline = Instant::now() + timeout;
        let mut bytes = Vec::new();
        let mut end = None;
        let mut waited = false;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match events.recv_timeout(left) {
                Ok(JobEvent::Pending { .. }) => waited = true,
                Ok(JobEvent::Chunk(data)) => bytes.extend_from_slice(&data),
                Ok(JobEvent::End(digest)) => end = Some(digest),
                Ok(JobEvent::Done { replayed, outcome }) => {
                    return Ok(Answer {
                        bytes,
                        end,
                        replayed,
                        outcome,
                        waited,
                    })
                }
                Err(RecvTimeoutError::Timeout) => return Err(RunError::Timeout),
                Err(RecvTimeoutError::Disconnected) => return Err(RunError::Internal),
            }
        }
    }

    /// Call `watch` with every relay agent that registers, after it is the
    /// target's session. It runs on the session's thread and must not block.
    pub fn on_agent(&self, watch: impl Fn(&Target, &AgentInfo) + Send + Sync + 'static) {
        self.inner
            .watchers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Arc::new(watch));
    }

    /// Restart the relay agent of `target` onto the executable at its path
    /// (RA-6) without losing an execution: wait up to `drain` until no call
    /// of the target is out, then hold new calls back, send `restart`, and
    /// wait up to `back` for the agent's next session, to which the held
    /// calls then go. Returns the agent that registered.
    pub fn restart_agent(
        &self,
        target: &Target,
        drain: Duration,
        back: Duration,
    ) -> Result<AgentInfo, RestartError> {
        self.inner.restart_agent(target, drain, back)
    }

    /// What the broker, else the relay agent, knows of `execution` (RP-7).
    pub fn query(&self, target: &Target, execution: &str) -> ExecutionState {
        self.inner.query(target, execution, &|_| {})
    }
}

impl Inner {
    fn grants(&self) -> MutexGuard<'_, Grants> {
        self.grants.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn links(&self) -> MutexGuard<'_, Links> {
        self.links.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn jobs(&self) -> MutexGuard<'_, HashMap<(Target, String), Arc<Job>>> {
        self.jobs.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn principals(&self) -> MutexGuard<'_, HashMap<String, (Role, Target)>> {
        self.principals.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn forget_principals(&self, target: &Target, roles: &[Role]) {
        self.principals()
            .retain(|_, (role, t)| !(t == target && roles.contains(role)));
    }

    fn authenticate(&self, request: &AuthRequest<'_>) -> Result<String, Rejection> {
        let role = Role::of(request.attributes)
            .ok_or_else(|| Rejection::unauthorized("attributes.role must be agent or client"))?;
        let (hash, grant) = self
            .grants()
            .check(request.credential)
            .ok_or_else(|| Rejection::unauthorized("unknown, revoked or expired credential"))?;
        if grant.role != role {
            return Err(Rejection::unauthorized(format!(
                "the credential is not a {} credential",
                role.name()
            )));
        }
        if let Some(lease) = request
            .attributes
            .get("lease")
            .and_then(serde_json::Value::as_str)
        {
            if lease != grant.target.name() {
                return Err(Rejection::unauthorized(format!(
                    "the credential is not for {lease}"
                )));
            }
        }
        let principal = format!("{}:{}:{}", role.name(), grant.target, &hash[..16]);
        self.principals()
            .insert(principal.clone(), (role, grant.target));
        Ok(principal)
    }

    fn dispatch(self: Arc<Self>, session: Session) {
        let found = session
            .principal()
            .and_then(|p| self.principals().get(p).cloned());
        match found {
            Some((Role::Agent, target)) => self.serve_agent(session, target),
            Some((Role::Client, target)) => client::serve(self, session, target),
            None => session.close(),
        }
    }

    fn serve_agent(self: &Arc<Self>, session: Session, target: Target) {
        let hello = match agent::read_hello(&session, self.config.hello_timeout) {
            Ok(hello) => hello,
            Err(error) => {
                (self.log)(&format!("relay: {target}: agent session refused: {error}"));
                session.close();
                return;
            }
        };
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let conn = Arc::new(AgentConn::new(id, target.clone(), session.clone(), hello));
        let replaced = {
            let mut links = self.links();
            if links.revoked.contains_key(&target) {
                drop(links);
                session.close();
                return;
            }
            // A new session ends a restart's hold: held calls go to it.
            links.held.remove(&target);
            links.agents.insert(target.clone(), Arc::clone(&conn))
        };
        self.links_changed.notify_all();
        if let Some(old) = replaced {
            old.close();
        }
        let hello = &conn.hello;
        (self.log)(&format!(
            "relay: {target}: agent registered ({} {}, build {}, {} in flight)",
            hello.os,
            hello.arch,
            hello.build.get(..12).unwrap_or(&hello.build),
            hello.in_flight.len()
        ));
        let watchers: Vec<Arc<AgentWatcher>> = self
            .watchers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let info = conn.info();
        for watch in watchers {
            watch(&target, &info);
        }
        let log = Arc::clone(&self.log);
        conn.read_until_end(&*log);
        let current = {
            let mut links = self.links();
            let current = links.agents.get(&target).is_some_and(|c| c.id == id);
            if current {
                links.agents.remove(&target);
            }
            current
        };
        self.links_changed.notify_all();
        if current {
            (self.log)(&format!("relay: {target}: agent session ended"));
        }
    }

    fn agent_info(&self, target: &Target) -> Option<AgentInfo> {
        self.links()
            .agents
            .get(target)
            .filter(|c| c.alive())
            .map(|c| c.info())
    }

    /// The live agent session of `target`, waiting up to `max` for one.
    /// `on_wait` is told once, with the time left, when the call must wait.
    pub(crate) fn wait_agent(
        &self,
        target: &Target,
        max: Duration,
        on_wait: &dyn Fn(Duration),
    ) -> Result<Arc<AgentConn>, Unavailable> {
        let deadline = Instant::now() + max;
        let mut told = false;
        let mut links = self.links();
        loop {
            if let Some(reason) = links.revoked.get(target) {
                return Err(Unavailable::Revoked(reason.clone()));
            }
            if let Some(conn) = links
                .agents
                .get(target)
                .filter(|c| c.alive() && !links.held.contains(target))
            {
                return Ok(Arc::clone(conn));
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(Unavailable::Timeout);
            }
            if !told {
                told = true;
                drop(links);
                on_wait(deadline - now);
                links = self.links();
                continue;
            }
            let pause = (deadline - now).min(Duration::from_secs(1));
            links = self
                .links_changed
                .wait_timeout(links, pause)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    pub(crate) fn submit(
        self: &Arc<Self>,
        target: &Target,
        execution: &str,
        op: Op,
        upload: Vec<u8>,
    ) -> std::sync::mpsc::Receiver<JobEvent> {
        let key = (target.clone(), execution.to_string());
        let max_kept = self.config.max_kept_output;
        let mut jobs = self.jobs();
        let now = Instant::now();
        jobs.retain(|_, job| !job.expired(now, self.config.retention));
        let existing = jobs.get(&key).cloned();
        let job = match existing {
            Some(existing) => {
                drop(jobs);
                if let Some(events) = existing.subscribe(false) {
                    return events;
                }
                // The kept output was dropped for its size: the agent
                // replays it from its own record.
                let replay = Arc::new(Job::new(
                    target.clone(),
                    execution.to_string(),
                    existing.op.clone(),
                    upload,
                    true,
                    max_kept,
                ));
                let events = replay.subscribe(true).expect("a new record takes callers");
                let inner = Arc::clone(self);
                std::thread::spawn(move || job::drive(inner, replay));
                return events;
            }
            None => {
                let job = Arc::new(Job::new(
                    target.clone(),
                    execution.to_string(),
                    op,
                    upload,
                    false,
                    max_kept,
                ));
                jobs.insert(key, Arc::clone(&job));
                job
            }
        };
        drop(jobs);
        let events = job.subscribe(true).expect("a new record takes callers");
        let inner = Arc::clone(self);
        std::thread::spawn(move || job::drive(inner, job));
        events
    }

    /// Forget a record whose call never ran, so its ID may be sent again.
    pub(crate) fn forget(&self, job: &Arc<Job>) {
        let key = (job.target.clone(), job.execution.clone());
        let mut jobs = self.jobs();
        if jobs.get(&key).is_some_and(|j| Arc::ptr_eq(j, job)) {
            jobs.remove(&key);
        }
    }

    pub(crate) fn query(
        &self,
        target: &Target,
        execution: &str,
        on_wait: &dyn Fn(Duration),
    ) -> ExecutionState {
        let known = self
            .jobs()
            .get(&(target.clone(), execution.to_string()))
            .cloned();
        if let Some(job) = known {
            return job.state();
        }
        loop {
            let Ok(conn) = self.wait_agent(target, self.config.agent_wait, on_wait) else {
                return ExecutionState::Unknown;
            };
            match conn
                .query(execution)
                .recv_timeout(self.config.query_timeout)
            {
                Ok(AgentEvent::Query(state)) => return state,
                Ok(AgentEvent::Lost) | Err(RecvTimeoutError::Disconnected) if !conn.alive() => {
                    continue
                }
                _ => return ExecutionState::Unknown,
            }
        }
    }

    fn restart_agent(
        &self,
        target: &Target,
        drain: Duration,
        back: Duration,
    ) -> Result<AgentInfo, RestartError> {
        // Hold new calls once no call of the target is out; the check and
        // the hold happen under the records' lock, which every new call
        // takes first.
        let deadline = Instant::now() + drain;
        let conn = loop {
            {
                let jobs = self.jobs();
                let busy = jobs
                    .iter()
                    .any(|((t, _), job)| t == target && !job.is_done());
                if !busy {
                    let mut links = self.links();
                    if links.revoked.contains_key(target) {
                        return Err(RestartError::Unavailable);
                    }
                    let Some(conn) = links.agents.get(target).filter(|c| c.alive()).cloned() else {
                        return Err(RestartError::Unavailable);
                    };
                    links.held.insert(target.clone());
                    break conn;
                }
            }
            if Instant::now() >= deadline {
                return Err(RestartError::Busy);
            }
            std::thread::sleep(Duration::from_millis(200));
        };
        let release = || {
            self.links().held.remove(target);
            self.links_changed.notify_all();
        };
        let execution = format!(
            "restart:{}",
            guest_actions::hex(&rand::random::<[u8; 16]>())
        );
        let events = conn.start(&execution, &Op::Restart, &[]);
        let answered = loop {
            match events.recv_timeout(Duration::from_secs(30)) {
                Ok(AgentEvent::Result(result)) => break Some(result.outcome),
                Ok(AgentEvent::Lost) | Err(RecvTimeoutError::Disconnected) => break None,
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => {
                    release();
                    return Err(RestartError::NotBack);
                }
            }
        };
        match answered {
            Some(Outcome::Failed { code, .. }) if code == "busy" => {
                release();
                return Err(RestartError::Busy);
            }
            Some(Outcome::Failed { code, message, .. }) => {
                release();
                return Err(RestartError::Refused(format!("{code}: {message}")));
            }
            _ => {}
        }
        (self.log)(&format!(
            "relay: {target}: relay agent restarting; calls wait for its next session"
        ));
        // The next session clears the hold; wait for it.
        let deadline = Instant::now() + back;
        let mut links = self.links();
        loop {
            if let Some(next) = links
                .agents
                .get(target)
                .filter(|c| c.id != conn.id && c.alive())
            {
                return Ok(next.info());
            }
            let now = Instant::now();
            if now >= deadline || links.revoked.contains_key(target) {
                drop(links);
                release();
                return Err(RestartError::NotBack);
            }
            links = self
                .links_changed
                .wait_timeout(links, (deadline - now).min(Duration::from_secs(1)))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    fn end_clients(clients: Vec<(u64, Session)>, reason: &str) {
        for (_, session) in clients {
            let revoked = relay_agent::protocol::Message::Revoked {
                reason: reason.to_string(),
            };
            let _ = session.try_send(revoked.encode());
            let _ = session.flush(Duration::from_secs(1));
            session.close();
        }
    }

    pub(crate) fn add_client(&self, target: &Target, session: &Session) -> Option<u64> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let mut links = self.links();
        if links.revoked.contains_key(target) {
            return None;
        }
        links
            .clients
            .entry(target.clone())
            .or_default()
            .push((id, session.clone()));
        Some(id)
    }

    pub(crate) fn remove_client(&self, target: &Target, id: u64) {
        let mut links = self.links();
        if let Some(list) = links.clients.get_mut(target) {
            list.retain(|(i, _)| *i != id);
            if list.is_empty() {
                links.clients.remove(target);
            }
        }
    }

    pub(crate) fn agent_info_for(&self, target: &Target) -> Option<AgentInfo> {
        self.agent_info(target)
    }
}

#[cfg(test)]
mod tests;
