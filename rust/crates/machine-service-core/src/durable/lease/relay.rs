//! A durable lease's exec, push and pull through the Mac's own relay agent
//! (RA-2, RA-8, M4), with machine-service's in-session exec of DM-4 as the
//! fallback.
//!
//! Each call chooses its link: the relay agent when this instance runs the
//! broker, the agent has a session, and its build is the installed
//! release's (DC-140); otherwise the in-session path, and the answer says
//! why. A command through the relay agent is started by the launcher of
//! [`super::super::launch`], which records its process group before the
//! release gate is let go, so release stops it and the sweep finds what it
//! left exactly as for an in-session command (DM-7, HY-3).
//!
//! A relay client's exec and script on the lease start through the same
//! launcher: the broker hands each to [`LeaseLauncher`], which writes the
//! lease's request as machine-service's own exec does, so a command sees the
//! lease's working directory, variables and agent keys whatever its entry,
//! and none of them travels in the relay protocol (DC-1xx-LAUNCH-1).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use relay_agent::protocol::{Op, Outcome};
use relay_broker::{Broker, Events, JobEvent, Purged, Target, Upload};
use serde_json::{json, Value};

use super::super::exec::{self, Program, Request};
use super::super::launch::{self, Launch};
use super::purges::{self, PendingPurge};
use super::{agents, tail_chars, DurableMachine, Reply, Runtime, MAX_OUTPUT_CHARS};
use crate::config::unix_now;

/// The answer's `link` when the relay agent carried the call.
pub const LINK_RELAY: &str = crate::relay::LINK_RELAY;
/// The answer's `link` when machine-service ran it in its own session.
pub const LINK_IN_SESSION: &str = "in-session";
/// How long a call holds the release gate for its launcher to record its
/// process group.
const RECORD_WAIT: Duration = Duration::from_secs(30);

/// The durable machine's relay link.
#[derive(Clone)]
pub struct DurableRelay {
    /// This instance's broker.
    pub broker: Broker,
    /// The broker's target for this Mac (DC-152).
    pub target: Target,
    /// The SHA-256 of the installed release's macOS relay agent, or why
    /// there is none (DC-140).
    pub expected_build: Result<String, String>,
    /// The launcher: the daemon's own executable.
    pub launcher: PathBuf,
    /// How long a call waits for an agent without a session (RP-8).
    pub agent_wait: Duration,
    /// How long release waits for the agent to confirm the purge of the
    /// lease's records (DC-173): [`PURGE_WAIT`], or the agent wait when
    /// that is shorter.
    pub purge_wait: Duration,
    /// Which relay agent is this instance's own, as its state folder names
    /// it (DC-213).
    pub agent: crate::durable::relay_agent::Identity,
}

/// How long a durable release waits for the relay agent to confirm that
/// the lease's records are gone: long enough for an agent to dial again
/// after a lost connection (its backoff stops at 5 s) and for the lease's
/// stopped commands to record their end (DC-176).
pub const PURGE_WAIT: Duration = Duration::from_secs(60);

/// The state of the pass over pending purges (DC-177).
#[derive(Default)]
pub(super) struct Retry {
    running: bool,
    again: bool,
}

/// One relay call's events, gathered as they come.
struct Call {
    events: Events,
    bytes: Vec<u8>,
    /// Where the stream's bytes go instead of `bytes`, with their count and
    /// SHA-256.
    sink: Option<(std::io::BufWriter<std::fs::File>, Received)>,
    sink_failed: Option<String>,
    end_sha256: Option<String>,
    outcome: Option<Outcome>,
}

impl Call {
    fn new(events: Events) -> Call {
        Call {
            events,
            bytes: Vec::new(),
            sink: None,
            sink_failed: None,
            end_sha256: None,
            outcome: None,
        }
    }

    /// Take events for up to `wait`; true once the outcome is in.
    fn pump(&mut self, wait: Duration) -> bool {
        let deadline = Instant::now() + wait;
        while self.outcome.is_none() {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.events.recv_timeout(left) {
                Ok(JobEvent::Chunk(data)) => match self.sink.as_mut() {
                    Some((writer, received)) => {
                        received.update(&data);
                        if let Err(error) = writer.write_all(&data) {
                            self.sink_failed.get_or_insert(error.to_string());
                        }
                    }
                    None => self.bytes.extend_from_slice(&data),
                },
                Ok(JobEvent::End(digest)) => self.end_sha256 = Some(digest.sha256),
                Ok(JobEvent::Pending { .. }) | Ok(JobEvent::Progress(_)) => {}
                Ok(JobEvent::Done { outcome, .. }) => self.outcome = Some(outcome),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    self.outcome = Some(Outcome::Interrupted {
                        message: "the broker lost the call".into(),
                    });
                }
            }
        }
        self.outcome.is_some()
    }
}

/// A relay command started under the release gate.
pub(super) struct Started {
    call: Call,
    execution: String,
    files: launch::Files,
}

/// The broker's launcher of this Mac's relay clients' commands
/// (DC-1xx-LAUNCH-1).
struct LeaseLauncher(Weak<DurableMachine>);

impl relay_broker::Launcher for LeaseLauncher {
    fn launch(&self, lease: &str, execution: &str, op: &Op) -> Result<Op, String> {
        let machine = self
            .0
            .upgrade()
            .ok_or("the durable machine is shutting down")?;
        machine.launch_client_call(lease, execution, op)
    }

    fn settled(&self, lease: &str, execution: &str) {
        if let Some(machine) = self.0.upgrade() {
            if super::valid_lease_id(lease) {
                launch::withdraw(&launch::files(&machine.config.lease_dir(lease), execution));
            }
        }
    }
}

fn failure(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Failed { code, message, .. } => format!("relay: {code}: {message}"),
        Outcome::Interrupted { message } => {
            format!("relay: the relay agent stopped while the call ran: {message}")
        }
        other => format!("relay: unexpected outcome {other:?}"),
    }
}

impl DurableMachine {
    /// Attach this instance's broker, once (M4). A lease that is current
    /// across a restart is bound again; its calls may have reached the
    /// agent before the restart, so its release purges (DM-8, DC-173).
    /// Every registration of this Mac's relay agent sends the purges that
    /// are still pending again, and so does attaching while it has a
    /// session (DC-177).
    pub fn attach_relay(self: &Arc<Self>, relay: DurableRelay) {
        let target = relay.target.clone();
        let broker = relay.broker.clone();
        if self.relay.set(relay).is_err() {
            return;
        }
        if let Some(record) = self.current_record() {
            let state = record["state"].as_str().unwrap_or_default();
            if let Some(lease_id) = record["lease_id"].as_str() {
                if state == "leased" || state == "releasing" {
                    self.bind_relay_lease(lease_id, true);
                }
            }
        }
        broker.launch_through(&target, Arc::new(LeaseLauncher(Arc::downgrade(self))));
        let machine = Arc::downgrade(self);
        let own = target.clone();
        broker.on_agent(move |target, _| {
            if *target == own {
                if let Some(machine) = machine.upgrade() {
                    machine.retry_pending_purges();
                }
            }
        });
        if broker.agent(&target).is_some() {
            self.retry_pending_purges();
        }
    }

    /// Name `lease_id` as the lease of every later relay call to this Mac.
    pub(super) fn bind_relay_lease(&self, lease_id: &str, handed: bool) {
        if let Some(relay) = self.relay.get() {
            relay.broker.bind_lease(&relay.target, lease_id, handed);
        }
    }

    /// Ask the relay agent to remove the lease's execution records, with
    /// the output they keep (DC-173). Returns the hygiene record's
    /// `relay_records` and, when the agent did not confirm, why: that fails
    /// the release's `relay-records` step, and the release keeps the purge
    /// pending until the agent confirms it (DC-177).
    pub(super) fn purge_relay_records(&self, lease_id: &str) -> (Value, Option<String>) {
        let Some(relay) = self.relay.get() else {
            return (
                json!({"purged": false, "skipped": "this instance runs no broker, so no call of the lease went through the relay agent"}),
                None,
            );
        };
        let skipped = !relay.broker.lease_reached_agent(&relay.target, lease_id);
        let answer = if skipped {
            (
                json!({"purged": false, "skipped": "no call of the lease reached the relay agent"}),
                None,
            )
        } else {
            match relay
                .broker
                .purge(&relay.target, lease_id, relay.purge_wait)
            {
                Ok(purged) => (
                    json!({"purged": true, "records": purged.records, "bytes": purged.bytes}),
                    None,
                ),
                Err(reason) => {
                    self.log(&format!(
                        "WARN: durable lease {lease_id}: the relay agent's records were not purged: {reason}"
                    ));
                    (
                        json!({"purged": false, "pending": true, "reason": reason}),
                        Some(reason),
                    )
                }
            }
        };
        relay.broker.unbind_lease(&relay.target, lease_id);
        answer
    }

    /// The purges the relay agent has not confirmed (DC-177).
    pub fn pending_purges(&self) -> Vec<PendingPurge> {
        purges::load(&purges::file(&self.config.state_dir)).unwrap_or_else(|error| {
            self.log(&format!("WARN: relay purges: {error}"));
            Vec::new()
        })
    }

    /// Store `change` applied to the pending purges; the caller holds
    /// `self.purges`.
    fn change_pending(
        &self,
        change: impl FnOnce(Vec<PendingPurge>) -> Vec<PendingPurge>,
    ) -> Result<(), String> {
        let path = purges::file(&self.config.state_dir);
        let pending = purges::load(&path)?;
        Self::write_json(&path, &purges::to_file(change(pending)))
    }

    /// Keep the unconfirmed purge of `lease_id` until the agent confirms
    /// it (DC-177). The caller holds the guard of `self.purges` and writes
    /// the lease's hygiene record under it.
    pub(super) fn keep_purge_pending(
        &self,
        lease_id: &str,
        at: f64,
        reason: &str,
    ) -> Result<(), String> {
        self.change_pending(|pending| purges::kept(pending, lease_id, at, reason))
    }

    /// Send the pending purges again on a background thread: one pass at a
    /// time, and one more when asked while a pass runs (DC-177). Never
    /// blocks, so the broker may call it from an agent's session thread.
    pub fn retry_pending_purges(self: &Arc<Self>) {
        {
            let Ok(mut retry) = self.retry.lock() else {
                return;
            };
            if retry.running {
                retry.again = true;
                return;
            }
            retry.running = true;
            retry.again = false;
        }
        let machine = Arc::clone(self);
        std::thread::spawn(move || loop {
            machine.send_pending_purges();
            let Ok(mut retry) = machine.retry.lock() else {
                return;
            };
            if !retry.again {
                retry.running = false;
                return;
            }
            retry.again = false;
        });
    }

    /// One pass: send each pending purge once while the agent has a
    /// session, including purges a release adds during the pass; drop each
    /// the agent confirms and mark its hygiene record purged.
    fn send_pending_purges(&self) {
        let Some(relay) = self.relay.get() else {
            return;
        };
        let mut sent: Vec<String> = Vec::new();
        loop {
            if relay.broker.agent(&relay.target).is_none() {
                return;
            }
            let pending = self.pending_purges();
            let Some(purge) = purges::next(&pending, &sent).cloned() else {
                return;
            };
            sent.push(purge.lease_id.clone());
            let result = relay
                .broker
                .purge(&relay.target, &purge.lease_id, relay.purge_wait);
            let Ok(_guard) = self.purges.lock() else {
                return;
            };
            let at = unix_now();
            let stored = match &result {
                Ok(purged) => {
                    self.log(&format!(
                        "durable lease {}: the relay agent confirmed the pending purge ({} records, {} bytes)",
                        purge.lease_id, purged.records, purged.bytes
                    ));
                    self.mark_purged_later(&purge, purged, at);
                    self.change_pending(|pending| purges::confirmed(pending, &purge.lease_id))
                }
                Err(reason) => {
                    self.log(&format!(
                        "WARN: durable lease {}: the pending purge is still unconfirmed: {reason}",
                        purge.lease_id
                    ));
                    self.change_pending(|pending| {
                        purges::attempted(pending, &purge.lease_id, at, reason)
                    })
                }
            };
            if let Err(error) = stored {
                self.log(&format!("WARN: relay purges: {error}"));
            }
        }
    }

    /// Record in the lease's hygiene record that its purge was confirmed
    /// after the release. The release's `relay-records` failure stays: it
    /// says what happened at release.
    fn mark_purged_later(&self, purge: &PendingPurge, purged: &Purged, at: f64) {
        let path = self.config.lease_dir(&purge.lease_id).join("hygiene.json");
        let Some(mut hygiene) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        else {
            return;
        };
        hygiene["relay_records"] = json!({
            "purged": true,
            "records": purged.records,
            "bytes": purged.bytes,
            "purged_at": at,
            "pending_since": purge.since,
            "attempts": purge.attempts + 1,
        });
        if let Err(error) = Self::write_json(&path, &hygiene) {
            self.log(&format!("WARN: durable lease {}: {error}", purge.lease_id));
        }
    }

    /// The relay link for the next call, or why the call runs in the
    /// service's own session (RA-8).
    pub fn relay_route(&self) -> Result<&DurableRelay, String> {
        let relay = self.relay.get().ok_or_else(|| {
            "this instance runs no broker (MACHINE_SERVICE_GUEST_LINK is not relay)".to_string()
        })?;
        let agent = relay.broker.agent(&relay.target).ok_or_else(|| {
            "the relay agent of this Mac has no session with the broker".to_string()
        })?;
        let expected = relay.expected_build.as_ref().map_err(Clone::clone)?;
        if agent.build != *expected {
            return Err(format!(
                "the relay agent runs build {}, not the installed release's {expected}; reinstall it with machine-service-install (DC-140)",
                agent.build
            ));
        }
        Ok(relay)
    }

    /// The `link` fields of an answer or a lease record.
    pub(super) fn link_fields(&self, route: &Result<&DurableRelay, String>) -> Value {
        match route {
            Ok(relay) => json!({
                "link": LINK_RELAY,
                "relay_agent": relay.broker.agent(&relay.target).map(|agent| json!({
                    "os": agent.os, "arch": agent.arch, "build": agent.build,
                    "label": relay.agent.label,
                    "root": relay.agent.root.to_string_lossy(),
                })),
            }),
            Err(reason) => json!({"link": LINK_IN_SESSION, "link_reason": reason}),
        }
    }

    /// Start `program` through the relay agent, and hold the caller's
    /// release gate until its launcher has recorded the process group.
    pub(super) fn relay_start(
        &self,
        relay: &DurableRelay,
        lease_id: &str,
        runtime: &Runtime,
        request: &Request<'_>,
        timeout: i64,
        body: &Value,
    ) -> Result<Started, Reply> {
        let execution =
            crate::relay::execution_of(body).map_err(|e| Reply::error(409, e.message))?;
        let files = launch::files(&self.config.lease_dir(lease_id), &execution);
        let command = launch::prepare(
            &files,
            &relay.launcher,
            Launch::Program(&request.program),
            &runtime.env,
            &request.cwd,
        )
        .map_err(|error| Reply::error(409, error))?;
        let events = relay.broker.submit(
            &relay.target,
            &execution,
            Op::Exec {
                command,
                stdin: false,
                timeout_s: timeout as f64,
            },
            Upload::None,
        );
        let mut call = Call::new(events);
        let deadline = Instant::now() + RECORD_WAIT;
        while !files.group.exists() && Instant::now() < deadline {
            if call.pump(Duration::from_millis(20)) {
                break;
            }
        }
        if !files.group.exists() && call.outcome.is_none() {
            self.log(&format!(
                "WARN: durable lease {lease_id}: relay execution {execution} has not started after {}s",
                RECORD_WAIT.as_secs()
            ));
        }
        Ok(Started {
            call,
            execution,
            files,
        })
    }

    /// The launched form of a relay client's `exec` or `script` on
    /// `lease` (DC-1xx-LAUNCH-1): the request of machine-service's own exec,
    /// with the lease's session environment, its keys and its workspace's
    /// `work/` as the working directory, run by the launcher. A script
    /// reaches the launcher's shell on standard input, as the relay agent
    /// feeds it. Under the release gate, like any start, so a release
    /// withdraws what this writes (DM-7).
    fn launch_client_call(&self, lease: &str, execution: &str, op: &Op) -> Result<Op, String> {
        let relay = self.relay.get().ok_or("this instance runs no broker")?;
        let _gate = self
            .gate
            .read()
            .map_err(|_| "durable gate poisoned".to_string())?;
        let record = self
            .current_record()
            .filter(|record| record["lease_id"] == lease && record["state"] == "leased")
            .ok_or_else(|| format!("lease {lease} is not the current lease of this Mac"))?;
        let runtime = self
            .runtime_for(&record)
            .map_err(|error| format!("session unavailable: {error}"))?;
        if runtime.cancel.load(Ordering::SeqCst) {
            return Err(format!("lease {lease} is being released"));
        }
        let cwd = exec::resolve_cwd(&self.config.workspace(lease).join("work"), None)?;
        let files = launch::files(&self.config.lease_dir(lease), execution);
        let launched = |launch: Launch<'_>, stdin: bool, timeout_s: f64| {
            launch::prepare(&files, &relay.launcher, launch, &runtime.env, &cwd).map(|command| {
                Op::Exec {
                    command,
                    stdin,
                    timeout_s,
                }
            })
        };
        match op {
            Op::Exec {
                command,
                stdin,
                timeout_s,
            } => {
                let program = Program::Line {
                    shell: runtime.env.shell(),
                    line: command.clone(),
                };
                launched(Launch::Program(&program), *stdin, *timeout_s)
            }
            Op::Script { timeout_s } => launched(Launch::ScriptOnInput, true, *timeout_s),
            other => Err(format!("{:?} is not a command", other.kind())),
        }
    }

    /// Wait for a relay command started by [`Self::relay_start`].
    pub(super) fn relay_finish(
        &self,
        relay: &DurableRelay,
        name: &str,
        runtime: &Runtime,
        started: Started,
        timeout: i64,
    ) -> Reply {
        let Started {
            mut call,
            execution,
            files,
        } = started;
        // What the launcher did not take goes when the call ends, however.
        struct Leftovers(launch::Files);
        impl Drop for Leftovers {
            fn drop(&mut self) {
                launch::withdraw(&self.0);
            }
        }
        let _leftovers = Leftovers(files);
        let bound =
            Duration::from_secs(timeout as u64) + relay.agent_wait + Duration::from_secs(60);
        let deadline = Instant::now() + bound;
        loop {
            if runtime.cancel.load(Ordering::SeqCst) {
                self.log(&format!(
                    "{name}: running relay command cancelled by release"
                ));
                return Reply::error(409, format!("command cancelled by release of {name}"));
            }
            if call.pump(Duration::from_millis(200)) {
                break;
            }
            if Instant::now() >= deadline {
                return Reply::error(
                    409,
                    format!(
                        "relay: no outcome for execution {execution} within {}s; it may still run",
                        bound.as_secs()
                    ),
                );
            }
        }
        if runtime.cancel.load(Ordering::SeqCst) {
            return Reply::error(409, format!("command cancelled by release of {name}"));
        }
        match call.outcome.as_ref().expect("the outcome is in") {
            Outcome::Exit {
                timed_out: true, ..
            } => Reply::error(
                409,
                format!("command timed out after {timeout} s on {name}"),
            ),
            Outcome::Exit { rc, .. } => {
                let output = String::from_utf8_lossy(&call.bytes);
                let redacted = agents::redact(&output, &runtime.env.secrets);
                Reply::ok(json!({
                    "name": name,
                    "vm": name,
                    "rc": rc.unwrap_or(-1),
                    "output": tail_chars(&redacted, MAX_OUTPUT_CHARS),
                    "link": LINK_RELAY,
                    "execution_id": execution,
                }))
            }
            other => Reply::error(409, failure(other)),
        }
    }

    /// A file push or pull through the relay agent; the paths are already
    /// resolved and checked. `source` is a file.
    pub(super) fn relay_transfer(
        &self,
        relay: &DurableRelay,
        name: &str,
        body: &Value,
        push: bool,
        source: &Path,
        destination: &Path,
    ) -> Reply {
        let execution = match crate::relay::execution_of(body) {
            Ok(execution) => execution,
            Err(error) => return Reply::error(409, error.message),
        };
        let file_name = source
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        let target = if std::fs::metadata(destination).is_ok_and(|meta| meta.is_dir()) {
            destination.join(&file_name)
        } else {
            destination.to_path_buf()
        };
        let (op, upload) = if push {
            use std::os::unix::fs::PermissionsExt;
            // The file streams from the disk (RP-15).
            let upload = match Upload::file(source) {
                Ok(upload) => upload,
                Err(error) => {
                    return Reply::error(409, format!("cannot read {}: {error}", source.display()))
                }
            };
            let mode = std::fs::metadata(source)
                .map(|meta| meta.permissions().mode() & 0o777)
                .ok();
            (
                Op::Write {
                    path: destination.to_string_lossy().to_string(),
                    name: file_name,
                    mode,
                },
                upload,
            )
        } else {
            (
                Op::Read {
                    path: source.to_string_lossy().to_string(),
                },
                Upload::None,
            )
        };
        let size = upload.digest().size;
        // A pull's bytes stream to a temporary sibling of the target, which
        // is renamed into place once they match (DM-5).
        let staged = if push {
            None
        } else {
            match staging_file(&target) {
                Ok(staged) => Some(staged),
                Err(error) => return Reply::error(409, format!("transfer failed: {error}")),
            }
        };
        let mut call = Call::new(relay.broker.submit(&relay.target, &execution, op, upload));
        if let Some(staged) = &staged {
            match staged.reopen() {
                Ok(file) => {
                    call.sink = Some((std::io::BufWriter::new(file), Default::default()));
                }
                Err(error) => return Reply::error(409, format!("transfer failed: {error}")),
            }
        }
        let bound = relay.agent_wait + Duration::from_secs(300 + (size >> 20));
        if !call.pump(bound) {
            return Reply::error(
                409,
                format!("relay: no outcome for transfer {execution}; it may still run"),
            );
        }
        let (bytes, sha256) = match call.outcome.as_ref().expect("the outcome is in") {
            Outcome::Written(digest) => (digest.size, digest.sha256.clone()),
            Outcome::Read(digest) => {
                let Some((mut writer, received)) = call.sink.take() else {
                    return Reply::error(409, "transfer failed: no bytes were kept");
                };
                let flushed = writer.flush();
                let received = received.finish();
                let matches = digest.size == received.size
                    && call.end_sha256.as_deref() == Some(digest.sha256.as_str())
                    && received.sha256 == digest.sha256;
                if !matches {
                    return Reply::error(
                        409,
                        "transfer failed: the bytes do not match their SHA-256; pull again",
                    );
                }
                if let Some(error) = call
                    .sink_failed
                    .take()
                    .or_else(|| flushed.err().map(|e| e.to_string()))
                {
                    return Reply::error(409, format!("transfer failed: {error}"));
                }
                let staged = staged.expect("a pull stages its file");
                if let Err(error) = staged.persist(&target) {
                    return Reply::error(
                        409,
                        format!(
                            "transfer failed: cannot move into {}: {}",
                            target.display(),
                            error.error
                        ),
                    );
                }
                (digest.size, digest.sha256.clone())
            }
            other => return Reply::error(409, format!("transfer failed: {}", failure(other))),
        };
        let mut out = json!({
            "name": name,
            "vm": name,
            "bytes": bytes,
            "sha256": sha256,
            "to": target.to_string_lossy(),
            "link": LINK_RELAY,
            "execution_id": execution,
        });
        if push {
            out["pushed"] = json!(source.to_string_lossy());
        } else {
            out["pulled"] = json!(source.to_string_lossy());
        }
        Reply::ok(out)
    }
}

/// The count and SHA-256 of the bytes a pull received.
#[derive(Default)]
struct Received {
    size: u64,
    hasher: sha2::Sha256,
}

/// The size and SHA-256 of what was received.
struct ReceivedDigest {
    size: u64,
    sha256: String,
}

impl Received {
    fn update(&mut self, data: &[u8]) {
        self.size += data.len() as u64;
        sha2::Digest::update(&mut self.hasher, data);
    }

    fn finish(self) -> ReceivedDigest {
        let digest = sha2::Digest::finalize(self.hasher);
        ReceivedDigest {
            size: self.size,
            sha256: digest.iter().map(|byte| format!("{byte:02x}")).collect(),
        }
    }
}

/// A temporary sibling of `target` that a pull's bytes stream into, renamed
/// into place once they match, so a reader never sees a partial file
/// (DM-5).
fn staging_file(target: &Path) -> Result<tempfile::NamedTempFile, String> {
    let parent = target
        .parent()
        .ok_or_else(|| format!("{} has no parent", target.display()))?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    tempfile::NamedTempFile::new_in(parent).map_err(|error| error.to_string())
}
