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

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use relay_agent::protocol::{Op, Outcome};
use relay_broker::{Broker, JobEvent, Target};
use serde_json::{json, Value};

use super::super::exec::Request;
use super::super::launch;
use super::{agents, tail_chars, DurableMachine, Reply, Runtime, MAX_OUTPUT_CHARS};

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
}

/// How long a durable release waits for the relay agent to confirm that
/// the lease's records are gone: long enough for an agent to dial again
/// after a lost connection (its backoff stops at 5 s) and for the lease's
/// stopped commands to record their end (DC-1xx-PUR-3).
pub const PURGE_WAIT: Duration = Duration::from_secs(60);

/// One relay call's events, gathered as they come.
struct Call {
    events: Receiver<JobEvent>,
    bytes: Vec<u8>,
    end_sha256: Option<String>,
    outcome: Option<Outcome>,
}

impl Call {
    /// Take events for up to `wait`; true once the outcome is in.
    fn pump(&mut self, wait: Duration) -> bool {
        let deadline = Instant::now() + wait;
        while self.outcome.is_none() {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.events.recv_timeout(left) {
                Ok(JobEvent::Chunk(data)) => self.bytes.extend_from_slice(&data),
                Ok(JobEvent::End(digest)) => self.end_sha256 = Some(digest.sha256),
                Ok(JobEvent::Pending { .. }) => {}
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
    pub fn attach_relay(&self, relay: DurableRelay) {
        let _ = self.relay.set(relay);
        if let Some(record) = self.current_record() {
            if let Some(lease_id) = record["lease_id"].as_str() {
                if record["state"] != "released" {
                    self.bind_relay_lease(lease_id, true);
                }
            }
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
    /// the release's `relay-records` step, and the agent's safety bound
    /// removes the records later (RA-5).
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
                    (json!({"purged": false, "reason": reason}), Some(reason))
                }
            }
        };
        relay.broker.unbind_lease(&relay.target, lease_id);
        answer
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
            &request.program,
            &runtime.env.vars,
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
            Vec::new(),
        );
        let mut call = Call {
            events,
            bytes: Vec::new(),
            end_sha256: None,
            outcome: None,
        };
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
        Ok(Started { call, execution })
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
        } = started;
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
            let bytes = match std::fs::read(source) {
                Ok(bytes) => bytes,
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
                bytes,
            )
        } else {
            (
                Op::Read {
                    path: source.to_string_lossy().to_string(),
                },
                Vec::new(),
            )
        };
        let mut call = Call {
            events: relay.broker.submit(&relay.target, &execution, op, upload),
            bytes: Vec::new(),
            end_sha256: None,
            outcome: None,
        };
        if !call.pump(relay.agent_wait + Duration::from_secs(300)) {
            return Reply::error(
                409,
                format!("relay: no outcome for transfer {execution}; it may still run"),
            );
        }
        let (bytes, sha256) = match call.outcome.as_ref().expect("the outcome is in") {
            Outcome::Written(digest) => (digest.size, digest.sha256.clone()),
            Outcome::Read(digest) => {
                let matches = digest.size == call.bytes.len() as u64
                    && call.end_sha256.as_deref() == Some(digest.sha256.as_str())
                    && service_layout::release::sha256_hex(&call.bytes) == digest.sha256;
                if !matches {
                    return Reply::error(
                        409,
                        "transfer failed: the bytes do not match their SHA-256; pull again",
                    );
                }
                if let Err(error) = write_into_place(&target, &call.bytes) {
                    return Reply::error(409, format!("transfer failed: {error}"));
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

/// Write `bytes` to `target` through a temporary sibling and a rename, so a
/// reader never sees a partial file (DM-5).
fn write_into_place(target: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = target
        .parent()
        .ok_or_else(|| format!("{} has no parent", target.display()))?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|error| error.to_string())?;
    std::io::Write::write_all(&mut file, bytes).map_err(|error| error.to_string())?;
    file.persist(target)
        .map_err(|error| format!("cannot move into {}: {}", target.display(), error.error))?;
    Ok(())
}
