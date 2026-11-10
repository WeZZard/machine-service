//! The durable lease: acquire, exec, push, pull, heartbeat, release, GC and
//! restart (`docs/arch/durable-machine.md`, DM-1 to DM-8), with the
//! capability report (MM-3) and hygiene (HY-2 to HY-4).
//!
//! A durable lease outlives the service process, so its state is on disk:
//! `<state>/durable.json` names the current lease, and
//! `<state>/leases/<lease_id>/` keeps the lease record, the process sessions
//! the lease started, the inventories and the hygiene record (DM-1). The
//! host-wide lock in `/var/tmp` makes the lease exclusive across every
//! instance on the Mac (DM-2).
//!
//! The session environment and the loaded agent keys live in memory only
//! (AK-5). After a restart they are rebuilt from the login shell and the key
//! files on first use.

use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::agents::{self, AgentKeys};
use super::capabilities::Capabilities;
use super::config::{
    DurableConfig, DAILY_REFRESH_S, KEPT_WORKSPACE_RETENTION_S, RECORD_RETENTION_S,
};
use super::exec::{self, Outcome, Program};
use super::hostlock::{Holder, HostLock, LockError};
use super::inventory;
use super::process::{self, LeaseSession};
use super::session_env::{self, SessionEnv, WorkspacePaths};
use crate::config::{log_to_file, unix_now};

mod purges;
mod relay;
pub use purges::PendingPurge;
pub use relay::{DurableRelay, LINK_IN_SESSION, LINK_RELAY, PURGE_WAIT};

/// Fields a durable acquisition refuses, because none can be applied to a
/// running Mac (DM-3).
pub const REFUSED_FIELDS: [&str; 7] = [
    "cpu",
    "memory_mb",
    "disk_gb",
    "vnc",
    "network",
    "source",
    "expected_source_fingerprint",
];
/// The most exec output returned, in characters, as for a virtual machine.
const MAX_OUTPUT_CHARS: usize = 64000;
/// The default exec timeout, in seconds, as for a virtual machine.
const DEFAULT_EXEC_TIMEOUT_S: i64 = 600;

/// One HTTP answer: a status code and a JSON body.
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    /// The HTTP status code.
    pub status: u16,
    /// The JSON body.
    pub body: Value,
}

impl Reply {
    /// A successful answer.
    pub fn ok(body: Value) -> Self {
        Self { status: 200, body }
    }

    /// An error answer, `{"error": message}`.
    pub fn error(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            body: json!({"error": message.into()}),
        }
    }
}

/// The in-memory part of the current lease.
#[derive(Clone)]
struct Runtime {
    lease_id: String,
    env: Arc<SessionEnv>,
    cancel: Arc<AtomicBool>,
}

/// The durable machine of one instance.
pub struct DurableMachine {
    /// The resolved configuration.
    pub config: DurableConfig,
    /// Hardware UUID shared by every instance on this Mac (MI-1, DC-87).
    pub machine_id: Option<String>,
    /// The cached capability facts.
    pub capabilities: Capabilities,
    /// Shared live name and persisted history (MI-2, MI-3).
    pub identity: super::identity::Identity,
    lock: HostLock,
    /// Serializes reads and writes of the lease state.
    store: Mutex<()>,
    /// Exec, push and pull take it shared to start; a release takes it
    /// exclusively to commit, so nothing starts unrecorded after a release
    /// began (DM-7).
    gate: RwLock<()>,
    runtime: Mutex<Option<Runtime>>,
    /// The broker, when this instance runs one (M4).
    relay: std::sync::OnceLock<DurableRelay>,
    /// Serializes changes to the pending purges and the hygiene records
    /// they update (DC-177).
    purges: Mutex<()>,
    /// Whether a pass over the pending purges runs, and whether another
    /// was asked for while it ran (DC-177).
    retry: Mutex<relay::Retry>,
}

impl DurableMachine {
    /// A durable machine for `config`.
    pub fn new(config: DurableConfig) -> Result<Self, String> {
        let lock = HostLock::new(&config.lock_dir, config.uid);
        let identity =
            super::identity::Identity::load(&config.state_dir, &config.instance, unix_now())?;
        if let Some(old) = identity.startup_rename() {
            log_to_file(
                &config.log_file,
                &format!(
                    "machine name changed from {old} to {} while stopped (MI-2, DC-88)",
                    identity.name()
                ),
            );
        }
        Ok(Self {
            identity,
            machine_id: super::capabilities::hardware_uuid(),
            config,
            capabilities: Capabilities::new(),
            lock,
            store: Mutex::new(()),
            gate: RwLock::new(()),
            runtime: Mutex::new(None),
            relay: std::sync::OnceLock::new(),
            purges: Mutex::new(()),
            retry: Mutex::new(relay::Retry::default()),
        })
    }

    /// Whether this instance exposes its Mac (DC-39).
    pub fn exposed(&self) -> bool {
        self.config.exposed
    }

    /// The durable machine's name, which is the instance ID (MM-2).
    pub fn name(&self) -> String {
        self.identity.name()
    }

    /// Whether a route belongs to this durable machine, including former
    /// labels. Operations still require `current_for` authorization (MI-3).
    pub fn owns_name(&self, name: &str) -> bool {
        name == self.name()
            || self.identity.renamed_message(name, unix_now()).is_some()
            || self.current_record().is_some_and(|record| {
                record["state"] != "released" && name == granted_name(&record)
            })
    }

    /// A former-name refusal names the current label for seven days (MI-3).
    pub fn unknown_name(&self, name: &str) -> Reply {
        Reply::error(
            404,
            self.identity
                .renamed_message(name, unix_now())
                .unwrap_or_else(|| format!("unknown machine: {name}")),
        )
    }

    /// The refusal of a durable request on an unexposed instance (MM-1).
    pub fn not_exposed(&self) -> Reply {
        Reply::error(
            404,
            format!("durable machine not exposed on {}", self.name()),
        )
    }

    fn log(&self, message: &str) {
        log_to_file(&self.config.log_file, message);
    }

    // ------------------------------------------------------------ storage

    fn current_file(&self) -> PathBuf {
        self.config.state_dir.join("durable.json")
    }

    fn record_file(&self, lease_id: &str) -> PathBuf {
        self.config.lease_dir(lease_id).join("lease.json")
    }

    fn sessions_file(&self, lease_id: &str) -> PathBuf {
        self.config.lease_dir(lease_id).join("sessions.json")
    }

    fn ci_file(&self, lease_id: &str) -> PathBuf {
        self.config.lease_dir(lease_id).join("ci-busy.json")
    }

    /// Sample whether a CI job runs now and fold it into the lease's busy
    /// intervals (HY-5, DC-75). A sample that cannot be stored is logged;
    /// hygiene never refuses (HY-1).
    fn sample_ci(&self, lease_id: &str, now: f64) -> Value {
        let path = self.ci_file(lease_id);
        let state = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .unwrap_or(Value::Null);
        let busy = super::capabilities::ci_runner(&self.config.ci_root, self.config.uid)["busy"]
            .as_bool()
            .unwrap_or(false);
        let next = ci_sample(&state, busy, now);
        if let Err(error) = Self::write_json(&path, &next) {
            self.log(&format!("WARN: durable lease {lease_id}: {error}"));
        }
        next
    }

    fn current_id(&self) -> Option<String> {
        let text = std::fs::read_to_string(self.current_file()).ok()?;
        let value: Value = serde_json::from_str(&text).ok()?;
        value
            .get("current")
            .and_then(Value::as_str)
            .filter(|id| valid_lease_id(id))
            .map(str::to_string)
    }

    fn read_record(&self, lease_id: &str) -> Option<Value> {
        let text = std::fs::read_to_string(self.record_file(lease_id)).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// The current lease's record, if any.
    pub fn current_record(&self) -> Option<Value> {
        let _store = self.store.lock().ok()?;
        self.current_id().and_then(|id| self.read_record(&id))
    }

    /// Keep the SHA-256 of the relay client credential issued for the
    /// current lease `lease_id` in its record, as a virtual machine's record
    /// keeps `relay_client_sha256`, so that the broker accepts it again
    /// after a restart of the instance (DM-8, RP-3).
    pub fn record_relay_client(&self, lease_id: &str, sha256: &str) -> Result<(), String> {
        let _gate = self.gate.read().map_err(|_| "durable gate poisoned")?;
        let _store = self.store.lock().map_err(|_| "durable state poisoned")?;
        let mut record = self
            .current_id()
            .filter(|id| id == lease_id)
            .and_then(|id| self.read_record(&id))
            .filter(|record| record["state"] != "released")
            .ok_or_else(|| format!("{lease_id} is not the current lease"))?;
        record["relay_client_sha256"] = json!(sha256);
        self.write_record(&record)
    }

    fn write_json(path: &Path, value: &Value) -> Result<(), String> {
        let parent = path.parent().unwrap_or(Path::new("/"));
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        let tmp = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4().simple()));
        let mut bytes = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
        bytes.push(b'\n');
        let written = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .and_then(|mut file| file.write_all(&bytes).and_then(|_| file.sync_all()))
            .and_then(|_| std::fs::rename(&tmp, path));
        if let Err(error) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("cannot write {}: {error}", path.display()));
        }
        Ok(())
    }

    fn write_record(&self, record: &Value) -> Result<(), String> {
        let lease_id = record["lease_id"].as_str().unwrap_or_default();
        Self::write_json(&self.record_file(lease_id), record)
    }

    fn set_current(&self, lease_id: Option<&str>) -> Result<(), String> {
        Self::write_json(&self.current_file(), &json!({"current": lease_id}))
    }

    /// The sessions the lease started, and the process groups of its relay
    /// commands (DM-7, HY-3).
    fn sessions(&self, lease_id: &str) -> Vec<LeaseSession> {
        let mut sessions = self.stored_sessions(lease_id);
        sessions.extend(super::launch::recorded_groups(
            &self.config.lease_dir(lease_id),
        ));
        sessions
    }

    fn stored_sessions(&self, lease_id: &str) -> Vec<LeaseSession> {
        std::fs::read_to_string(self.sessions_file(lease_id))
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(LeaseSession::from_json)
            .collect()
    }

    fn append_session(&self, lease_id: &str, session: LeaseSession) -> Result<(), String> {
        let _store = self
            .store
            .lock()
            .map_err(|_| "durable state poisoned".to_string())?;
        let mut sessions: Vec<Value> = self
            .stored_sessions(lease_id)
            .iter()
            .map(LeaseSession::to_json)
            .collect();
        sessions.push(session.to_json());
        Self::write_json(&self.sessions_file(lease_id), &Value::Array(sessions))
    }

    fn holder(&self, record: &Value) -> Holder {
        let ttl = record["ttl_expires_at"].as_f64().unwrap_or_default();
        Holder {
            state_dir: self.config.state_dir.to_string_lossy().to_string(),
            lease_id: record["lease_id"].as_str().unwrap_or_default().to_string(),
            purpose: record["purpose"].as_str().unwrap_or_default().to_string(),
            name: granted_name(record).to_string(),
            ttl_expires_at: ttl,
            reclaim_at: ttl + self.config.grace_hours * 3600.0,
        }
    }

    fn leased_message(&self, name: &str, purpose: &str, until: f64) -> String {
        format!(
            "durable machine leased: {} (purpose {purpose}, until {})",
            name,
            format_gmtime(until)
        )
    }

    // ------------------------------------------------------------ records

    /// The machine record of `GET /machines` (MM-3): every field a virtual
    /// machine's record carries, with `kind`, `instance` and
    /// `capabilities`.
    pub fn machine_record(&self) -> Value {
        let record = self.current_record();
        self.machine_record_from(record.as_ref())
    }

    fn machine_record_from(&self, record: Option<&Value>) -> Value {
        let name = self.name();
        let agents = agents::inspect(&self.config.agents_dir(), self.config.uid);
        let capabilities = self.capabilities.report(&self.config, &agents);
        let (cpu, memory_mb, disk_gb) = self.capabilities.resources();
        let lease = record.filter(|record| record["state"] != "released");
        let field = |name: &str| {
            lease
                .and_then(|record| record.get(name).cloned())
                .unwrap_or(Value::Null)
        };
        let (state, unavailable) = super::capabilities::durable_state(
            lease.and_then(|record| record["state"].as_str()),
            &capabilities["display"],
            &self.config.user,
        );
        let mut out = json!({
            "name": name,
            "kind": "durable",
            "instance": name,
            "machine_id": self.machine_id,
            "state": state,
            "lease_id": field("lease_id"),
            "granted_name": lease.map(granted_name).map(str::to_string),
            "purpose": field("purpose"),
            "image": null,
            "image_kind": null,
            "created_at": field("created_at"),
            "ttl_hours": field("ttl_hours"),
            "ttl_expires_at": field("ttl_expires_at"),
            "grace_until": field("grace_until"),
            "warned": lease.map(|record| record["warned"].clone()).unwrap_or(json!(false)),
            "env": field("env"),
            "ip": null,
            "cpu": cpu,
            "memory_mb": memory_mb,
            "disk_gb": disk_gb,
            "configuration": null,
            "workspace": field("workspace"),
            "agents": field("agents"),
            "warnings": super::capabilities::warnings(&capabilities, self.config.disk_warn_bytes),
            "relay_purges_pending": self
                .pending_purges()
                .iter()
                .map(PendingPurge::to_json)
                .collect::<Vec<_>>(),
            "capabilities": capabilities,
        });
        if let Some(reason) = unavailable {
            out["unavailable_reason"] = json!(reason);
        }
        // The link a lease's calls take now, with or without a lease: the
        // Mac's relay agent keeps its session across leases (RA-4, RA-8).
        if self.relay.get().is_some() {
            if let Value::Object(fields) = self.link_fields(&self.relay_route()) {
                for (key, value) in fields {
                    out[key] = value;
                }
            }
        }
        if let Some(ttl) = lease.and_then(|record| record["ttl_expires_at"].as_f64()) {
            out["ttl_hours_remaining"] =
                json!(((ttl - unix_now()) / 3600.0 * 100.0).round() / 100.0);
        }
        if let Some(grace) = lease.and_then(|record| record["grace_until"].as_f64()) {
            out["grace_hours_remaining"] =
                json!(((grace - unix_now()) / 3600.0 * 100.0).round() / 100.0);
        }
        out
    }

    /// The validated current lease named by `name` and `body.lease_id`
    /// (MM-2): a missing or stale `lease_id` is refused with 409 and nothing
    /// runs.
    fn current_for(&self, name: &str, body: &Value) -> Result<Value, Reply> {
        let record = self.current_record();
        let requested_id = body.get("lease_id").and_then(Value::as_str);
        // MI-3 / DC-89: an old name authorizes nothing without the current
        // lease's ID. Pre-MI records keep their original `name` compatibly.
        if name != self.name()
            && !record.as_ref().is_some_and(|record| {
                record["state"] != "released"
                    && name == granted_name(record)
                    && requested_id.is_some()
                    && record["lease_id"].as_str() == requested_id
            })
        {
            return Err(self.unknown_name(name));
        }
        let Some(lease_id) = requested_id else {
            return Err(Reply::error(
                409,
                format!("lease_id is required for an operation on durable machine {name}"),
            ));
        };
        let current = record
            .as_ref()
            .filter(|record| record["lease_id"].as_str() == Some(lease_id));
        let Some(record) = current else {
            return Err(Reply::error(
                409,
                format!("lease {lease_id} is not the current lease of durable machine {name}"),
            ));
        };
        let state = record["state"].as_str().unwrap_or_default();
        if state != "leased" {
            return Err(Reply::error(
                409,
                format!("durable machine {name} is {state}; refusing operation"),
            ));
        }
        Ok(record.clone())
    }

    // ------------------------------------------------------------ acquire

    /// `POST /acquire` with `machine` (DM-3).
    pub fn acquire(&self, body: &Value) -> Reply {
        let name = body
            .get("machine")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if body.get("image").is_some_and(|value| !value.is_null())
            || body.get("line").is_some_and(|value| !value.is_null())
        {
            return Reply::error(400, "machine and image are mutually exclusive");
        }
        if !self.config.exposed {
            return if self.identity.renamed_message(&name, unix_now()).is_some() {
                self.unknown_name(&name)
            } else {
                self.not_exposed()
            };
        }
        if name != self.name() {
            return self.unknown_name(&name);
        }
        for field in REFUSED_FIELDS {
            if body.get(field).is_some_and(|value| !value.is_null()) {
                return Reply::error(400, format!("{field} does not apply to a durable machine"));
            }
        }
        let Some(purpose) = body.get("purpose").and_then(Value::as_str) else {
            return Reply::error(400, "'purpose' (string) is required");
        };
        if !valid_purpose(purpose) {
            return Reply::error(400, "purpose must match [a-z0-9][a-z0-9-]{0,63}");
        }
        let env = body
            .get("env")
            .and_then(Value::as_str)
            .unwrap_or("default")
            .to_string();
        if env != "default" && env != "none" {
            return Reply::error(400, "env must be default or none for a durable machine");
        }
        let ttl_value = body.get("ttl_hours").cloned().unwrap_or(json!(24));
        let ttl = match acquisition_options::resolve(None, None, None, true, &ttl_value, false) {
            Ok(resolved) => resolved["effective"]["initial_ttl_hours"]
                .as_f64()
                .unwrap_or(24.0),
            Err(error) => return Reply::error(400, error.to_string()),
        };
        if !(0.1..=720.0).contains(&ttl) {
            return Reply::error(400, "ttl_hours must be within [0.1, 720]");
        }

        // Without the service user's GUI session a lease could not reach the
        // window server; the refusal is a capacity refusal a client may wait
        // on (MM-5, DC-68).
        let display = super::capabilities::display(&self.config, &mut serde_json::Map::new());
        if let ("unavailable", Some(reason)) =
            super::capabilities::durable_state(None, &display, &self.config.user)
        {
            return Reply::error(
                409,
                format!("durable machine unavailable: {} ({reason})", self.name()),
            );
        }

        // MI-3: a rename may have happened during the display probe. Hold
        // the current label through reservation, then release it before
        // slow preparation or helpers that read the shared name again.
        let current_name = self.identity.current_name();
        if name != *current_name {
            drop(current_name);
            return self.unknown_name(&name);
        }
        // Reserve: the lease state, then the host-wide lock (DM-2).
        let now = unix_now();
        let lease_id = uuid::Uuid::new_v4().simple().to_string();
        let workspace = self.config.workspace(&lease_id);
        let record = json!({
            "name": name,
            "kind": "durable",
            "instance": name,
            "machine_id": self.machine_id,
            "lease_id": lease_id,
            // MI-3 / DC-89: immutable for the whole lease, including restarts.
            "granted_name": name,
            "purpose": purpose,
            "image": null,
            "image_kind": null,
            "state": "acquiring",
            "created_at": now,
            "ttl_hours": ttl,
            "ttl_expires_at": now + ttl * 3600.0,
            "grace_until": null,
            "warned": false,
            "env": env,
            "ip": null,
            "configuration": null,
            "workspace": workspace.to_string_lossy(),
            "state_dir": self.config.state_dir.to_string_lossy(),
        });
        {
            let Ok(_store) = self.store.lock() else {
                return Reply::error(500, "durable state poisoned");
            };
            if let Some(current) = self.current_id().and_then(|id| self.read_record(&id)) {
                if current["state"] != "released" {
                    let message = self.leased_message(
                        &name,
                        current["purpose"].as_str().unwrap_or_default(),
                        current["ttl_expires_at"].as_f64().unwrap_or_default(),
                    );
                    self.log(&format!(
                        "durable acquire refused (purpose={purpose}): {message}"
                    ));
                    return Reply::error(409, message);
                }
            }
            match self.lock.take(&self.holder(&record), now) {
                Ok(()) => {}
                Err(LockError::Held(holder)) => {
                    let message =
                        self.leased_message(&name, &holder.purpose, holder.ttl_expires_at);
                    self.log(&format!(
                        "durable acquire refused (purpose={purpose}): {message}; held from {}",
                        holder.state_dir
                    ));
                    return Reply::error(409, message);
                }
                Err(LockError::Unusable(reason)) => {
                    return Reply::error(409, format!("durable lease lock unusable: {reason}"));
                }
            }
            let reserved = self
                .write_record(&record)
                .and_then(|_| Self::write_json(&self.sessions_file(&lease_id), &json!([])))
                .and_then(|_| self.set_current(Some(&lease_id)));
            if let Err(error) = reserved {
                drop(_store);
                self.rollback(&lease_id);
                return Reply::error(409, format!("durable acquisition failed: {error}"));
            }
        }
        drop(current_name);
        // The relay agent's records of this lease's calls name it, so
        // release can purge them (DC-173).
        self.bind_relay_lease(&lease_id, false);
        self.log(&format!(
            "durable acquire {} for purpose {purpose} (lease {lease_id}, ttl {ttl}h)",
            self.name()
        ));

        match self.prepare(record) {
            Ok(record) => {
                let mut out = self.machine_record_from(Some(&record));
                // The record carries the link the lease's calls take now;
                // each answer says which one served it (RA-8).
                out["workspace"] = record["workspace"].clone();
                Reply::ok(out)
            }
            Err(error) => {
                self.log(&format!(
                    "WARN: durable acquisition {lease_id} failed, rolled back: {error}"
                ));
                self.rollback(&lease_id);
                Reply::error(409, format!("durable acquisition failed: {error}"))
            }
        }
    }

    /// Create the workspace, build the session, take the inventory before,
    /// and mark the lease leased (DM-3, HY-2, HY-4).
    fn prepare(&self, mut record: Value) -> Result<Value, String> {
        let lease_id = record["lease_id"].as_str().unwrap_or_default().to_string();
        let workspace = self.config.workspace(&lease_id);
        create_workspace(&self.config.workspace_root, &workspace)?;
        let load_agents = record["env"] == "default";
        let (session, keys) = self.build_session(&lease_id, load_agents)?;
        record["environment_names"] = json!(session.names());
        record["agents"] = keys.as_ref().map(AgentKeys::record).unwrap_or(Value::Null);

        let login = self.login_environment().unwrap_or_default();
        self.capabilities.refresh(&self.config, &login);

        let before = inventory::take(
            inventory::standard_parts(
                &self.inventory_context(&session),
                self.capabilities.software(),
            ),
            self.config.inventory_timeout,
        );
        let before_path = self
            .config
            .lease_dir(&lease_id)
            .join("inventory-before.json");
        if let Err(error) = Self::write_json(&before_path, &before) {
            // Hygiene reports and never refuses (HY-1).
            self.log(&format!("WARN: durable lease {lease_id}: {error}"));
        }
        record["hygiene"] = json!({"inventory_before": before_path.to_string_lossy()});

        let _store = self
            .store
            .lock()
            .map_err(|_| "durable state poisoned".to_string())?;
        record["state"] = json!("leased");
        self.write_record(&record)?;
        // The first CI sample (HY-5); it touches only its own file.
        self.sample_ci(&lease_id, unix_now());
        if let Ok(mut runtime) = self.runtime.lock() {
            *runtime = Some(Runtime {
                lease_id,
                env: Arc::new(session),
                cancel: Arc::new(AtomicBool::new(false)),
            });
        }
        Ok(record)
    }

    /// Undo a failed acquisition: the workspace, the record, the current
    /// pointer and the lock (DM-3).
    fn rollback(&self, lease_id: &str) {
        let _ = remove_tree(&self.config.workspace(lease_id));
        let _ = std::fs::remove_dir_all(self.config.lease_dir(lease_id));
        if let Ok(_store) = self.store.lock() {
            if self.current_id().as_deref() == Some(lease_id) {
                let _ = self.set_current(None);
            }
        }
        let state_dir = self.config.state_dir.to_string_lossy().to_string();
        if let Err(error) = self.lock.release(&state_dir, lease_id) {
            self.log(&format!(
                "WARN: durable lock release for {lease_id}: {error}"
            ));
        }
        if let Ok(mut runtime) = self.runtime.lock() {
            if runtime
                .as_ref()
                .is_some_and(|runtime| runtime.lease_id == lease_id)
            {
                *runtime = None;
            }
        }
    }

    fn base_environment() -> Vec<(String, String)> {
        // A test build's lease shell reads only its fixture's `HOME`, never
        // the startup files an inherited `ZDOTDIR` names (DC-253).
        std::env::vars()
            .filter(|(name, _)| !(service_layout::test_build::is_isolated() && name == "ZDOTDIR"))
            .collect()
    }

    fn login_environment(&self) -> Result<Vec<(String, String)>, String> {
        session_env::capture_login_environment(
            &self.config.login_shell,
            &Self::base_environment(),
            &self.config.home,
            self.config.fact_timeout,
        )
    }

    /// Build the session environment and, with `load_agents`, read the
    /// agent keys (DM-4, AK-3, AK-4). Writes the Codex `config.toml` when
    /// Codex is loaded.
    fn build_session(
        &self,
        lease_id: &str,
        load_agents: bool,
    ) -> Result<(SessionEnv, Option<AgentKeys>), String> {
        let workspace = self.config.workspace(lease_id);
        let (work, tmp, codex) = (
            workspace.join("work"),
            workspace.join("tmp"),
            workspace.join("codex"),
        );
        let paths = WorkspacePaths {
            root: &workspace,
            work: &work,
            tmp: &tmp,
            codex: &codex,
        };
        let login = self.login_environment()?;
        let keys = load_agents.then(|| agents::inspect(&self.config.agents_dir(), self.config.uid));
        let session = session_env::build(login, lease_id, &paths, &self.config.home, keys.as_ref());
        if let Some(config) = keys.as_ref().and_then(AgentKeys::codex_config) {
            let path = codex.join("config.toml");
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&path)
                .and_then(|mut file| file.write_all(config.as_bytes()))
                .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
        }
        Ok((session, keys))
    }

    /// The current runtime, rebuilt after a restart (DM-8).
    fn runtime_for(&self, record: &Value) -> Result<Runtime, String> {
        let lease_id = record["lease_id"].as_str().unwrap_or_default();
        if let Ok(runtime) = self.runtime.lock() {
            if let Some(runtime) = runtime
                .as_ref()
                .filter(|runtime| runtime.lease_id == lease_id)
            {
                return Ok(runtime.clone());
            }
        }
        let (session, _) = self.build_session(lease_id, record["env"] == "default")?;
        let mut runtime = self
            .runtime
            .lock()
            .map_err(|_| "durable runtime poisoned".to_string())?;
        if let Some(existing) = runtime
            .as_ref()
            .filter(|runtime| runtime.lease_id == lease_id)
        {
            return Ok(existing.clone());
        }
        let built = Runtime {
            lease_id: lease_id.to_string(),
            env: Arc::new(session),
            cancel: Arc::new(AtomicBool::new(false)),
        };
        *runtime = Some(built.clone());
        Ok(built)
    }

    fn inventory_context(&self, session: &SessionEnv) -> inventory::Context {
        inventory::Context {
            uid: self.config.uid,
            home: self.config.home.clone(),
            ci_root: self.config.ci_root.clone(),
            workspace_root: self.config.workspace_root.clone(),
            env: session.vars.clone(),
            timeout: self.config.inventory_timeout,
        }
    }

    // ------------------------------------------------------------ exec

    /// `POST /machines/<name>/exec` (DM-4).
    pub fn exec(&self, name: &str, body: &Value) -> Reply {
        let timeout = match body.get("timeout") {
            None | Some(Value::Null) => DEFAULT_EXEC_TIMEOUT_S,
            Some(value) => match value.as_i64() {
                Some(seconds) if seconds > 0 => seconds,
                _ => return Reply::error(409, "timeout must be positive"),
            },
        };
        let (running, runtime, reason) = {
            let Ok(_gate) = self.gate.read() else {
                return Reply::error(500, "durable gate poisoned");
            };
            let record = match self.current_for(name, body) {
                Ok(record) => record,
                Err(reply) => return reply,
            };
            let lease_id = record["lease_id"].as_str().unwrap_or_default().to_string();
            let runtime = match self.runtime_for(&record) {
                Ok(runtime) => runtime,
                Err(error) => return Reply::error(409, format!("session unavailable: {error}")),
            };
            let program = if let Some(script) = body.get("script").and_then(Value::as_str) {
                Program::Script(script.as_bytes().to_vec())
            } else if let Some(argv) = body.get("argv").and_then(Value::as_array) {
                Program::Line {
                    shell: runtime.env.shell(),
                    line: argv
                        .iter()
                        .map(|value| match value {
                            Value::String(text) => crate::ssh::shell_quote(text),
                            other => crate::ssh::shell_quote(&other.to_string()),
                        })
                        .collect::<Vec<_>>()
                        .join(" "),
                }
            } else {
                return Reply::error(409, "need 'argv' or 'script'");
            };
            let work = self.config.workspace(&lease_id).join("work");
            let cwd = match exec::resolve_cwd(&work, body.get("cwd").and_then(Value::as_str)) {
                Ok(cwd) => cwd,
                Err(error) => return Reply::error(409, error),
            };
            let request = exec::Request {
                program,
                env: &runtime.env.vars,
                cwd,
                timeout: std::time::Duration::from_secs(timeout as u64),
                ci_root: self.config.ci_root.clone(),
            };
            if runtime.cancel.load(Ordering::SeqCst) {
                return Reply::error(409, format!("command cancelled by release of {name}"));
            }
            // The relay agent runs it in the desktop session when it can;
            // else the service does, as DM-4 says (RA-8).
            let reason = match self.relay_route() {
                Ok(relay) => {
                    let started = match self
                        .relay_start(relay, &lease_id, &runtime, &request, timeout, body)
                    {
                        Ok(started) => started,
                        Err(reply) => return reply,
                    };
                    drop(_gate);
                    return self.relay_finish(relay, name, &runtime, started, timeout);
                }
                Err(reason) => reason,
            };
            let started = exec::start(&request, self.config.uid, |session| {
                self.append_session(&lease_id, session)
            });
            match started {
                Ok(running) => (running, runtime, reason),
                Err(error) => return Reply::error(409, error),
            }
        };
        match running.wait(&runtime.cancel) {
            Ok(Outcome::Exited { rc, output }) => {
                let redacted = agents::redact(&output, &runtime.env.secrets);
                Reply::ok(json!({
                    "name": name,
                    "vm": name,
                    "rc": rc,
                    "output": tail_chars(&redacted, MAX_OUTPUT_CHARS),
                    "link": LINK_IN_SESSION,
                    "link_reason": reason,
                }))
            }
            Ok(Outcome::TimedOut) => Reply::error(
                409,
                format!("command timed out after {timeout} s on {name}"),
            ),
            Ok(Outcome::Cancelled) => {
                self.log(&format!("{name}: running command cancelled by release"));
                Reply::error(409, format!("command cancelled by release of {name}"))
            }
            Err(error) => Reply::error(409, error),
        }
    }

    // ------------------------------------------------------------ transfers

    /// A path a transfer must not touch: the lease SSH material and the
    /// agent key files, so no transfer copies a key into a file (AK-5).
    fn protected(&self, path: &Path) -> Result<(), String> {
        crate::packs::protect_key_material(&self.config.state_dir, path, true)
            .map_err(|error| error.message)?;
        let agents = self.config.agents_dir();
        let agents = agents.canonicalize().unwrap_or(agents);
        let target = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if target.starts_with(&agents) || agents.starts_with(&target) {
            return Err("Transfer path overlaps the agent key files".to_string());
        }
        Ok(())
    }

    /// Check that `body.lease_id` names the current lease of `name`, before
    /// a byte-carrying transfer receives its bytes (FD-4).
    pub fn check_lease(&self, name: &str, body: &Value) -> Result<(), Reply> {
        let Ok(_gate) = self.gate.read() else {
            return Err(Reply::error(500, "durable gate poisoned"));
        };
        self.current_for(name, body).map(|_| ())
    }

    /// `POST /machines/<name>/push` and `/pull`, path-based, for a loopback
    /// client (DM-5). `push` copies `local_path` to `remote_path`; a
    /// relative `remote_path` resolves against the workspace's `work/`.
    pub fn transfer(&self, name: &str, body: &Value, push: bool, loopback: bool) -> Reply {
        let Ok(_gate) = self.gate.read() else {
            return Reply::error(500, "durable gate poisoned");
        };
        let record = match self.current_for(name, body) {
            Ok(record) => record,
            Err(reply) => return reply,
        };
        if !loopback {
            return Reply::error(
                409,
                "path-based push and pull need a loopback client; a host path means nothing on another Mac (use PUT or GET /machines/<name>/files)",
            );
        }
        let local = body.get("local_path").and_then(Value::as_str);
        let remote = body.get("remote_path").and_then(Value::as_str);
        let (Some(local), Some(remote)) = (local, remote) else {
            return Reply::error(409, "need local_path and remote_path");
        };
        let lease_id = record["lease_id"].as_str().unwrap_or_default();
        let work = self.config.workspace(lease_id).join("work");
        let remote_path = if Path::new(remote).is_absolute() {
            PathBuf::from(remote)
        } else {
            work.join(remote)
        };
        let (source, destination) = if push {
            (PathBuf::from(local), remote_path)
        } else {
            (remote_path, PathBuf::from(local))
        };
        if std::fs::symlink_metadata(&source).is_err() {
            return Reply::error(409, format!("no such path: {}", source.display()));
        }
        for path in [&source, &destination] {
            if let Err(error) = self.protected(path) {
                return Reply::error(409, error);
            }
        }
        // A file goes through the relay agent when it can; a folder, which
        // the relay protocol does not move, goes by the service (DC-151).
        let is_file = std::fs::symlink_metadata(&source).is_ok_and(|meta| meta.is_file());
        let route = if is_file {
            self.relay_route()
        } else {
            Err("a folder is copied by the service; the relay link moves single files".into())
        };
        let reason = match route {
            Ok(relay) => {
                return self.relay_transfer(relay, name, body, push, &source, &destination)
            }
            Err(reason) => reason,
        };
        match copy_into_place(&source, &destination) {
            Ok((target, bytes, digest)) => {
                let mut out = json!({
                    "name": name,
                    "vm": name,
                    "bytes": bytes,
                    "sha256": digest,
                    "link": LINK_IN_SESSION,
                    "link_reason": reason,
                });
                if push {
                    out["pushed"] = json!(local);
                    out["to"] = json!(target.to_string_lossy());
                } else {
                    out["pulled"] = json!(source.to_string_lossy());
                    out["to"] = json!(target.to_string_lossy());
                }
                Reply::ok(out)
            }
            Err(error) => Reply::error(409, format!("transfer failed: {error}")),
        }
    }

    // ------------------------------------------------------------ heartbeat

    /// `POST /machines/<name>/heartbeat` (DM-6).
    pub fn heartbeat(&self, name: &str, body: &Value) -> Reply {
        let Ok(_gate) = self.gate.read() else {
            return Reply::error(500, "durable gate poisoned");
        };
        let mut record = match self.current_for(name, body) {
            Ok(record) => record,
            Err(reply) => return reply,
        };
        let ttl = match body.get("ttl_hours") {
            None => record["ttl_hours"].as_f64().unwrap_or(24.0),
            Some(value) => {
                match acquisition_options::resolve(None, None, None, true, value, false) {
                    Ok(resolved) => resolved["effective"]["initial_ttl_hours"]
                        .as_f64()
                        .unwrap_or(24.0),
                    Err(error) => return Reply::error(409, error.to_string()),
                }
            }
        };
        let expires = unix_now() + ttl * 3600.0;
        record["ttl_expires_at"] = json!(expires);
        record["grace_until"] = Value::Null;
        record["warned"] = json!(false);
        {
            let Ok(_store) = self.store.lock() else {
                return Reply::error(500, "durable state poisoned");
            };
            if let Err(error) = self.write_record(&record) {
                return Reply::error(409, error);
            }
        }
        if let Err(error) = self.lock.renew(&self.holder(&record)) {
            self.log(&format!(
                "WARN: durable heartbeat {name}: lock renewal failed: {error}"
            ));
            return Reply::error(409, format!("durable lease lock lost: {error}"));
        }
        self.log(&format!(
            "heartbeat {name}: TTL renewed for {ttl}h (until {})",
            format_gmtime(expires)
        ));
        Reply::ok(self.machine_record_from(Some(&record)))
    }

    // ------------------------------------------------------------ release

    /// `POST /machines/<name>/release` (DM-7).
    pub fn release(&self, name: &str, body: &Value) -> Reply {
        let reason = body
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("requested")
            .to_string();
        let keep = body
            .get("keep_workspace")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let record = {
            let Ok(_gate) = self.gate.write() else {
                return Reply::error(500, "durable gate poisoned");
            };
            let record = match self.current_for(name, body) {
                Ok(record) => record,
                Err(reply) => return reply,
            };
            match self.commit_release(record, &reason, keep) {
                Ok(record) => record,
                Err(error) => return Reply::error(409, error),
            }
        };
        self.log(&format!("release requested for {name} (reason: {reason})"));
        Reply::ok(self.finish_release(record))
    }

    /// Mark the lease releasing and preempt its running commands. Call with
    /// the gate held exclusively.
    fn commit_release(&self, mut record: Value, reason: &str, keep: bool) -> Result<Value, String> {
        record["state"] = json!("releasing");
        record["release_reason"] = json!(reason);
        record["keep_workspace"] = json!(keep);
        {
            let _store = self
                .store
                .lock()
                .map_err(|_| "durable state poisoned".to_string())?;
            self.write_record(&record)?;
        }
        if let Ok(runtime) = self.runtime.lock() {
            if let Some(runtime) = runtime.as_ref() {
                runtime.cancel.store(true, Ordering::SeqCst);
            }
        }
        // A relay command not yet started finds nothing to run (DM-7).
        if let Some(lease_id) = record["lease_id"].as_str() {
            super::launch::withdraw_requests(&self.config.lease_dir(lease_id));
        }
        Ok(record)
    }

    /// Stop the lease's sessions, sweep, take the inventory after, remove
    /// the workspace, and free the machine (DM-7, HY-3, HY-4). A failure in
    /// any step is recorded and never keeps the machine leased.
    fn finish_release(&self, mut record: Value) -> Value {
        let lease_id = record["lease_id"].as_str().unwrap_or_default().to_string();
        let name = self.name();
        let lease_dir = self.config.lease_dir(&lease_id);
        let mut failures: Vec<Value> = Vec::new();

        // 1. The lease's own sessions: SIGTERM, 10 s, SIGKILL (DM-7).
        let started = record["created_at"].as_f64().unwrap_or_default();
        let context = process::context(
            self.config.uid,
            &lease_id,
            started,
            self.sessions(&lease_id),
            &self.config.ci_root,
        );
        // One snapshot is classified before any signal: stopping a session
        // first would reparent its descendants to launchd and hide them.
        let table = process::process_table(Some(self.config.uid));
        let members = process::session_members(&table, &context, &process::process_arguments);
        let mut verdict = process::classify(&table, &context, &process::process_arguments);
        // The applications the lease launched, from the same moment (HY-3,
        // DC-73). Without a reading of the running applications none is
        // matched: the rule never guesses.
        let before_inventory = std::fs::read_to_string(lease_dir.join("inventory-before.json"))
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok());
        let running = inventory::running_applications(self.config.fact_timeout)
            .ok()
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default();
        let mut jobs = super::apps::JobReader::new(self.config.uid);
        let (apps, apps_spared) = super::apps::leftovers(
            &table,
            &running,
            &super::apps::inventory_pids(before_inventory.as_ref()),
            &context,
            &mut |pid| jobs.lease_of(pid),
            &process::process_arguments,
        );
        let stopped = process::terminate(&members.matched, self.config.stop_grace);

        // 2. The leftover sweep (HY-3): what the snapshot matched beyond the
        // sessions, then whatever a second look still finds.
        let member_pids: std::collections::HashSet<i32> =
            members.matched.iter().map(|item| item.info.pid).collect();
        let apps: Vec<process::Matched> = apps
            .into_iter()
            .filter(|app| !member_pids.contains(&app.info.pid))
            .collect();
        let app_pids: std::collections::HashSet<i32> =
            apps.iter().map(|app| app.info.pid).collect();
        verdict.matched.retain(|item| {
            !member_pids.contains(&item.info.pid) && !app_pids.contains(&item.info.pid)
        });
        // Applications are asked to quit first, then the other leftovers
        // get SIGTERM and SIGKILL (HY-3).
        let mut swept = super::apps::end(&apps, self.config.stop_grace);
        swept.extend(process::terminate(&verdict.matched, self.config.stop_grace));
        verdict.spared.extend(apps_spared);
        let handled: std::collections::HashSet<i32> = member_pids
            .iter()
            .copied()
            .chain(swept.iter().map(|ended| ended.info.pid))
            .collect();
        let table = process::process_table(Some(self.config.uid));
        let mut second = process::classify(&table, &context, &process::process_arguments);
        second
            .matched
            .retain(|item| !handled.contains(&item.info.pid));
        swept.extend(process::terminate(&second.matched, self.config.stop_grace));
        verdict.spared.extend(second.spared);
        let mut spared: Vec<Value> = Vec::new();
        let mut spared_pids = std::collections::HashSet::new();
        for (item, why) in members.spared.iter().chain(verdict.spared.iter()) {
            if spared_pids.insert(item.info.pid) {
                spared.push(json!({
                    "pid": item.info.pid,
                    "executable": item.info.executable(),
                    "started_at": item.info.started_at(),
                    "rule": item.rule.as_str(),
                    "spared": why.as_str(),
                }));
            }
        }
        for outcome in stopped.iter().chain(swept.iter()) {
            if !outcome.ended {
                failures.push(json!({
                    "step": "sweep",
                    "reason": format!("process {} did not end", outcome.info.pid),
                }));
            }
        }

        // 2b. The relay agent's records of the lease, with the command
        // output they keep, which may hold agent keys (DC-173, AK-5). Its
        // commands were stopped above, so none of them still runs.
        let (relay_records, unconfirmed) = self.purge_relay_records(&lease_id);
        if let Some(reason) = &unconfirmed {
            failures.push(json!({"step": "relay-records", "reason": reason}));
        }

        // 3. The expensive facts and the inventory after (MM-4, HY-4).
        let login = match self.login_environment() {
            Ok(login) => login,
            Err(error) => {
                failures.push(json!({"step": "login-environment", "reason": error}));
                Self::base_environment()
            }
        };
        self.capabilities.refresh(&self.config, &login);
        let session = SessionEnv {
            vars: login
                .into_iter()
                .filter(|(name, _)| !session_env::CREDENTIAL_VARIABLES.contains(&name.as_str()))
                .collect(),
            ..Default::default()
        };
        let after = inventory::take(
            inventory::standard_parts(
                &self.inventory_context(&session),
                self.capabilities.software(),
            ),
            self.config.inventory_timeout,
        );
        let before_path = lease_dir.join("inventory-before.json");
        let after_path = lease_dir.join("inventory-after.json");
        let changes_path = lease_dir.join("changes.json");
        let before = std::fs::read_to_string(&before_path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok());
        if before.is_none() {
            failures.push(
                json!({"step": "inventory", "reason": "the inventory before the lease is missing"}),
            );
        }
        let changes = inventory::diff(before.as_ref().unwrap_or(&json!({"parts": {}})), &after);
        for (path, value) in [(&after_path, &after), (&changes_path, &changes)] {
            if let Err(error) = Self::write_json(path, value) {
                failures.push(json!({"step": "inventory", "reason": error}));
            }
        }

        // 4. The workspace (DM-7, HY-2).
        let workspace = self.config.workspace(&lease_id);
        let keep = record["keep_workspace"].as_bool().unwrap_or(false);
        let workspace_report = if keep {
            json!({"path": workspace.to_string_lossy(), "kept": true, "removed": false})
        } else {
            match remove_tree(&workspace) {
                Ok(()) => {
                    json!({"path": workspace.to_string_lossy(), "kept": false, "removed": true})
                }
                Err(error) => {
                    failures.push(json!({"step": "workspace", "reason": error}));
                    json!({"path": workspace.to_string_lossy(), "kept": false, "removed": false})
                }
            }
        };

        let released_at = unix_now();
        // The last sample, then any interval still open ends at release
        // (HY-5, DC-75).
        let ci = ci_close(&self.sample_ci(&lease_id, released_at), released_at);
        // An unconfirmed purge is kept before the lease is released, under
        // the lock the retry takes to mark the hygiene record, so neither a
        // crash nor a retry that confirms at once loses it (DC-177).
        let purges = self.purges.lock();
        if let Some(reason) = &unconfirmed {
            if let Err(error) = self.keep_purge_pending(&lease_id, released_at, reason) {
                self.log(&format!("WARN: durable lease {lease_id}: {error}"));
                failures.push(json!({
                    "step": "relay-records",
                    "reason": format!("the purge cannot be kept pending: {error}"),
                }));
            }
        }
        let hygiene = json!({
            "lease_id": lease_id,
            "name": name,
            "started_at": started,
            "released_at": released_at,
            "reason": record["release_reason"],
            "stopped": stopped.iter().map(process::Ended::to_json).collect::<Vec<_>>(),
            "swept": swept.iter().map(process::Ended::to_json).collect::<Vec<_>>(),
            "spared": spared,
            "inventory": {
                "before": before_path.to_string_lossy(),
                "after": after_path.to_string_lossy(),
                "changes": changes_path.to_string_lossy(),
            },
            "changes": changes,
            "workspace": workspace_report,
            "relay_records": relay_records,
            "ci_busy_intervals": ci["intervals"],
            "ci_sampling": {"interval_s": ci["interval_s"], "samples": ci["samples"]},
            "failures": failures,
        });
        let hygiene_path = lease_dir.join("hygiene.json");
        if let Err(error) = Self::write_json(&hygiene_path, &hygiene) {
            self.log(&format!("WARN: durable lease {lease_id}: {error}"));
        }
        drop(purges);

        // 5. Free the machine.
        record["state"] = json!("released");
        record["released_at"] = json!(released_at);
        record["workspace_kept"] = json!(keep);
        record["hygiene"] = json!({
            "record": hygiene_path.to_string_lossy(),
            "inventory_before": before_path.to_string_lossy(),
            "inventory_after": after_path.to_string_lossy(),
            "changes": changes_path.to_string_lossy(),
        });
        if let Ok(_store) = self.store.lock() {
            if let Err(error) = self.write_record(&record) {
                self.log(&format!("WARN: durable lease {lease_id}: {error}"));
            }
            if self.current_id().as_deref() == Some(lease_id.as_str()) {
                if let Err(error) = self.set_current(None) {
                    self.log(&format!("WARN: durable lease {lease_id}: {error}"));
                }
            }
        }
        let state_dir = self.config.state_dir.to_string_lossy().to_string();
        match self.lock.release(&state_dir, &lease_id) {
            Ok(true) => {}
            Ok(false) => self.log(&format!(
                "WARN: durable lease {lease_id}: the host lock no longer named it"
            )),
            Err(error) => self.log(&format!(
                "WARN: durable lease {lease_id}: lock release failed: {error}"
            )),
        }
        if let Ok(mut runtime) = self.runtime.lock() {
            if runtime
                .as_ref()
                .is_some_and(|runtime| runtime.lease_id == lease_id)
            {
                *runtime = None;
            }
        }
        self.log(&format!(
            "durable lease {lease_id} on {name} released ({} stopped, {} swept, {} spared)",
            stopped.len(),
            swept.len(),
            spared_pids.len()
        ));
        json!({
            "name": name,
            "lease_id": lease_id,
            "released": true,
            "reason": record["release_reason"],
            "hygiene": hygiene,
        })
    }

    // ------------------------------------------------------------ hygiene, refresh

    /// `GET /machines/<name>/leases/<lease_id>/hygiene` (HY-4).
    pub fn hygiene(&self, name: &str, lease_id: &str) -> Reply {
        if name != self.name() {
            if let Err(refused) = self.current_for(name, &json!({"lease_id": lease_id})) {
                return refused;
            }
        }
        if !valid_lease_id(lease_id) {
            return Reply::error(404, format!("unknown lease: {lease_id}"));
        }
        let dir = self.config.lease_dir(lease_id);
        if let Some(record) = std::fs::read_to_string(dir.join("hygiene.json"))
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        {
            return Reply::ok(record);
        }
        match self.read_record(lease_id) {
            Some(record) => Reply::ok(json!({
                "lease_id": lease_id,
                "name": name,
                "state": record["state"],
                "inventory": {"before": dir.join("inventory-before.json").to_string_lossy()},
            })),
            None => Reply::error(404, format!("unknown lease: {lease_id}")),
        }
    }

    /// `POST /machines/<name>/refresh` (MM-4): re-read the expensive facts.
    pub fn refresh(&self, name: &str, body: &Value) -> Reply {
        if name != self.name() {
            if let Err(refused) = self.current_for(name, body) {
                return refused;
            }
        }
        let login = self
            .login_environment()
            .unwrap_or_else(|_| Self::base_environment());
        self.capabilities.refresh(&self.config, &login);
        Reply::ok(self.machine_record())
    }

    // ------------------------------------------------------------ GC, restart

    /// One GC pass (DM-6, DM-7): warn past TTL, release past TTL + grace,
    /// finish an interrupted release, prune old records and kept
    /// workspaces, and refresh the expensive facts daily.
    pub fn gc_once(&self) {
        if !self.config.exposed {
            return;
        }
        let now = unix_now();
        if let Some(mut record) = self.current_record() {
            let state = record["state"].as_str().unwrap_or_default().to_string();
            if state == "leased" {
                if let Some(lease_id) = record["lease_id"].as_str() {
                    self.sample_ci(lease_id, now);
                }
            }
            let ttl = record["ttl_expires_at"].as_f64().unwrap_or(f64::INFINITY);
            if state == "releasing" {
                self.log(&format!(
                    "GC: finishing the release of durable lease {}",
                    record["lease_id"]
                ));
                self.finish_release(record);
            } else if state == "leased" && now >= ttl {
                match record["grace_until"].as_f64() {
                    None => {
                        let until = now + self.config.grace_hours * 3600.0;
                        record["grace_until"] = json!(until);
                        record["warned"] = json!(true);
                        if let Ok(_store) = self.store.lock() {
                            let _ = self.write_record(&record);
                        }
                        self.log(&format!(
                            "LEASE WARN: durable machine {} past TTL; grace until {}",
                            self.name(),
                            format_gmtime(until)
                        ));
                    }
                    Some(until) if now >= until => {
                        self.log(&format!(
                            "GC: reclaiming durable machine {} (TTL + grace expired)",
                            self.name()
                        ));
                        let committed = match self.gate.write() {
                            Ok(_gate) => self.commit_release(record, "ttl-expired", false).ok(),
                            Err(_) => None,
                        };
                        if let Some(record) = committed {
                            self.finish_release(record);
                        }
                    }
                    Some(_) => {}
                }
            }
        }
        self.prune(now);
        if self
            .capabilities
            .refreshed_at()
            .is_none_or(|at| now - at >= DAILY_REFRESH_S)
        {
            let login = self
                .login_environment()
                .unwrap_or_else(|_| Self::base_environment());
            self.capabilities.refresh(&self.config, &login);
        }
    }

    /// Remove records past 30 days and kept workspaces past 7 days (DM-7).
    fn prune(&self, now: f64) {
        let Ok(entries) = std::fs::read_dir(self.config.state_dir.join("leases")) else {
            return;
        };
        for entry in entries.flatten() {
            let lease_id = entry.file_name().to_string_lossy().to_string();
            if !valid_lease_id(&lease_id) {
                continue;
            }
            let Some(mut record) = self.read_record(&lease_id) else {
                continue;
            };
            if record["state"] != "released" {
                continue;
            }
            let released = record["released_at"].as_f64().unwrap_or(now);
            let workspace = self.config.workspace(&lease_id);
            if now - released >= KEPT_WORKSPACE_RETENTION_S
                && record["workspace_kept"] == true
                && workspace.exists()
            {
                match remove_tree(&workspace) {
                    Ok(()) => {
                        record["workspace_removed_at"] = json!(now);
                        if let Ok(_store) = self.store.lock() {
                            let _ = self.write_record(&record);
                        }
                        self.log(&format!("GC: removed the kept workspace of {lease_id}"));
                    }
                    Err(error) => self.log(&format!(
                        "WARN: GC could not remove the kept workspace of {lease_id}: {error}"
                    )),
                }
            }
            if now - released >= RECORD_RETENTION_S {
                let _ = remove_tree(&workspace);
                let _ = std::fs::remove_dir_all(self.config.lease_dir(&lease_id));
                self.log(&format!(
                    "GC: removed the record of durable lease {lease_id}"
                ));
            }
        }
    }

    /// Reconcile at startup (DM-8): keep a leased lease and its lock, roll
    /// back an interrupted acquisition, finish an interrupted release, and
    /// release a lease left on an instance whose exposure is now off. Work
    /// that may take long runs on a background thread.
    pub fn startup(self: &Arc<Self>) {
        let record = self.current_record();
        if let Some(mut record) = record {
            if record.get("granted_name").is_none() {
                record["granted_name"] = json!(granted_name(&record));
                if let Err(error) = self.write_record(&record) {
                    self.log(&format!(
                        "WARN: cannot migrate durable granted_name: {error}"
                    ));
                }
            }
            let lease_id = record["lease_id"].as_str().unwrap_or_default().to_string();
            match record["state"].as_str().unwrap_or_default() {
                "acquiring" => {
                    self.log(&format!(
                        "startup: rolling back durable acquisition {lease_id}; it did not survive the restart"
                    ));
                    self.rollback(&lease_id);
                }
                "releasing" => {
                    self.log(&format!("startup: finishing the release of durable lease {lease_id}"));
                    let machine = Arc::clone(self);
                    std::thread::spawn(move || {
                        machine.finish_release(record);
                    });
                }
                "leased" if !self.config.exposed => {
                    self.log(&format!(
                        "startup: durable exposure is off; releasing durable lease {lease_id}"
                    ));
                    let machine = Arc::clone(self);
                    std::thread::spawn(move || {
                        let committed = match machine.gate.write() {
                            Ok(_gate) => machine
                                .commit_release(record, "durable-exposure-off", false)
                                .ok(),
                            Err(_) => None,
                        };
                        if let Some(record) = committed {
                            machine.finish_release(record);
                        }
                    });
                }
                "leased" => {
                    match self.lock.take(&self.holder(&record), unix_now()) {
                        Ok(()) => self.log(&format!(
                            "startup: durable lease {lease_id} on {} continues",
                            self.name()
                        )),
                        Err(error) => self.log(&format!(
                            "WARN: startup: durable lease {lease_id} is current but the host lock is {error}"
                        )),
                    }
                }
                _ => {}
            }
        }
        if self.config.exposed {
            let machine = Arc::clone(self);
            std::thread::spawn(move || {
                let login = machine
                    .login_environment()
                    .unwrap_or_else(|_| Self::base_environment());
                machine.capabilities.refresh(&machine.config, &login);
            });
        }
    }
}

/// Create a fresh, empty workspace with `work/`, `tmp/` and `codex/`, mode
/// 700 (DM-1, HY-2).
fn create_workspace(root: &Path, workspace: &Path) -> Result<(), String> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(root)
        .map_err(|error| format!("cannot create {}: {error}", root.display()))?;
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(workspace)
        .map_err(|error| {
            format!(
                "cannot create the workspace {}: {error}",
                workspace.display()
            )
        })?;
    for folder in ["work", "tmp", "codex"] {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(workspace.join(folder))
            .map_err(|error| format!("cannot create {folder}/: {error}"))?;
    }
    Ok(())
}

/// Remove a tree without following symlinks, making read-only folders
/// writable first when a plain removal fails.
fn remove_tree(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
        Ok(_) => {}
    }
    if std::fs::remove_dir_all(path).is_ok() {
        return Ok(());
    }
    fn unlock(path: &Path) {
        let Ok(metadata) = std::fs::symlink_metadata(path) else {
            return;
        };
        if metadata.is_dir() {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
            if let Ok(entries) = std::fs::read_dir(path) {
                for entry in entries.flatten() {
                    unlock(&entry.path());
                }
            }
        }
    }
    unlock(path);
    std::fs::remove_dir_all(path)
        .map_err(|error| format!("cannot remove {}: {error}", path.display()))
}

/// Copy `source` to `destination` through a temporary sibling and a rename,
/// so a reader never sees a partial file (DM-5). An existing folder at the
/// destination receives the source under its own name. Returns the final
/// path, the bytes moved and their SHA-256 (a file's digest, or a folder's
/// digest over its sorted relative paths and file digests).
/// Fold one CI sample into the busy intervals (HY-5, DC-75): a busy
/// sample opens an interval when none is open; an idle one closes the open
/// interval at its own time. A job that starts and ends between two samples
/// is not seen, so the record says how often it looked.
pub fn ci_sample(state: &Value, busy: bool, now: f64) -> Value {
    let mut intervals: Vec<Value> = state["intervals"].as_array().cloned().unwrap_or_default();
    let open = intervals
        .last()
        .is_some_and(|interval| interval["to"].is_null());
    if busy && !open {
        intervals.push(json!({"from": now, "to": null}));
    } else if !busy && open {
        if let Some(last) = intervals.last_mut() {
            last["to"] = json!(now);
        }
    }
    json!({
        "interval_s": crate::GC_INTERVAL_S,
        "samples": state["samples"].as_u64().unwrap_or(0) + 1,
        "last_sample": now,
        "intervals": intervals,
    })
}

/// Close an interval still open at `at`, the end of the lease.
pub fn ci_close(state: &Value, at: f64) -> Value {
    let mut state = state.clone();
    if let Some(last) = state["intervals"]
        .as_array_mut()
        .and_then(|intervals| intervals.last_mut())
    {
        if last["to"].is_null() {
            last["to"] = json!(at);
        }
    }
    state
}

pub fn copy_into_place(
    source: &Path,
    destination: &Path,
) -> Result<(PathBuf, u64, String), String> {
    let source_meta = std::fs::symlink_metadata(source).map_err(|error| error.to_string())?;
    if source_meta.file_type().is_symlink() {
        return Err(format!("{} is a symlink", source.display()));
    }
    let mut target = destination.to_path_buf();
    if std::fs::metadata(destination).is_ok_and(|meta| meta.is_dir()) {
        let base = source
            .file_name()
            .ok_or_else(|| format!("{} has no name", source.display()))?;
        target = destination.join(base);
    }
    let parent = target
        .parent()
        .ok_or_else(|| format!("{} has no parent", target.display()))?
        .to_path_buf();
    std::fs::create_dir_all(&parent).map_err(|error| error.to_string())?;
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    let tmp = parent.join(format!(
        ".{name}.machine-service-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let result = if source_meta.is_dir() {
        if target.exists() {
            return Err(format!("{} already exists", target.display()));
        }
        copy_tree(source, &tmp).map(|(bytes, entries)| {
            let mut hasher = Sha256::new();
            for (relative, digest) in entries {
                hasher.update(relative.as_bytes());
                hasher.update([0]);
                hasher.update(digest.as_bytes());
                hasher.update(b"\n");
            }
            (bytes, hex(&hasher.finalize()))
        })
    } else {
        copy_file(source, &tmp)
    };
    let (bytes, digest) = match result {
        Ok(done) => done,
        Err(error) => {
            let _ = remove_tree(&tmp);
            return Err(error);
        }
    };
    if let Err(error) = std::fs::rename(&tmp, &target) {
        let _ = remove_tree(&tmp);
        return Err(format!("cannot move into {}: {error}", target.display()));
    }
    Ok((target, bytes, digest))
}

fn copy_file(source: &Path, target: &Path) -> Result<(u64, String), String> {
    use std::io::Read;
    let mut input = std::fs::File::open(source).map_err(|error| error.to_string())?;
    let mode = input
        .metadata()
        .map(|meta| meta.permissions().mode() & 0o777)
        .unwrap_or(0o644);
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(target)
        .map_err(|error| error.to_string())?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut total = 0u64;
    loop {
        let count = input.read(&mut buffer).map_err(|error| error.to_string())?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        output
            .write_all(&buffer[..count])
            .map_err(|error| error.to_string())?;
        total += count as u64;
    }
    output.sync_all().map_err(|error| error.to_string())?;
    Ok((total, hex(&hasher.finalize())))
}

fn copy_tree(source: &Path, target: &Path) -> Result<(u64, Vec<(String, String)>), String> {
    let mut entries = Vec::new();
    let mut total = 0u64;
    fn walk(
        root: &Path,
        source: &Path,
        target: &Path,
        total: &mut u64,
        entries: &mut Vec<(String, String)>,
    ) -> Result<(), String> {
        std::fs::create_dir(target).map_err(|error| error.to_string())?;
        let mut children: Vec<PathBuf> = std::fs::read_dir(source)
            .map_err(|error| error.to_string())?
            .flatten()
            .map(|entry| entry.path())
            .collect();
        children.sort();
        for child in children {
            let meta = std::fs::symlink_metadata(&child).map_err(|error| error.to_string())?;
            let destination = target.join(child.file_name().unwrap_or_default());
            if meta.file_type().is_symlink() {
                return Err(format!("{} is a symlink", child.display()));
            } else if meta.is_dir() {
                walk(root, &child, &destination, total, entries)?;
            } else {
                let (bytes, digest) = copy_file(&child, &destination)?;
                *total += bytes;
                let relative = child
                    .strip_prefix(root)
                    .unwrap_or(&child)
                    .to_string_lossy()
                    .to_string();
                entries.push((relative, digest));
            }
        }
        Ok(())
    }
    walk(source, source, target, &mut total, &mut entries)?;
    entries.sort();
    Ok((total, entries))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

// MI-3: leases written before granted_name was introduced already persist
// their original name; never substitute the daemon's current label.
fn granted_name(record: &Value) -> &str {
    record["granted_name"]
        .as_str()
        .or_else(|| record["name"].as_str())
        .unwrap_or_default()
}

/// A lease ID as acquisition issues it: 32 lowercase hex characters.
pub fn valid_lease_id(value: &str) -> bool {
    value.len() == 32
        && value
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

fn valid_purpose(purpose: &str) -> bool {
    if purpose.is_empty() || purpose.len() > 64 {
        return false;
    }
    let mut chars = purpose.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn tail_chars(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    text.chars().skip(count - max).collect()
}

pub(super) fn format_gmtime(seconds: f64) -> String {
    // SAFETY: `gmtime_r` writes into a caller-provided `tm`.
    unsafe {
        let stamp = seconds as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::gmtime_r(&stamp, &mut tm).is_null() {
            return String::new();
        }
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hy5_ci_busy_intervals() {
        let state = ci_sample(&Value::Null, false, 100.0);
        let state = ci_sample(&state, true, 160.0);
        let state = ci_sample(&state, true, 220.0);
        let state = ci_sample(&state, false, 280.0);
        let state = ci_sample(&state, true, 340.0);
        assert_eq!(state["samples"], 5);
        assert_eq!(
            state["intervals"],
            json!([{"from": 160.0, "to": 280.0}, {"from": 340.0, "to": null}])
        );
        let closed = ci_close(&state, 400.0);
        assert_eq!(closed["intervals"][1], json!({"from": 340.0, "to": 400.0}));
        assert_eq!(
            ci_close(&ci_sample(&Value::Null, false, 1.0), 2.0)["intervals"],
            json!([])
        );
    }

    #[test]
    fn lease_ids_and_purposes() {
        assert!(valid_lease_id(&uuid::Uuid::new_v4().simple().to_string()));
        assert!(!valid_lease_id("../../etc"));
        assert!(!valid_lease_id("ABCDEF0123456789ABCDEF0123456789"));
        assert!(valid_purpose("metal-check"));
        assert!(!valid_purpose("Metal"));
    }

    /// DM-5 (TL-1): a push lands whole, with its digest, inside an existing
    /// folder under its own name; a folder push digests its tree.
    #[test]
    fn dm5_copy_into_place() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"hello").unwrap();
        let into = dir.path().join("dest");
        std::fs::create_dir(&into).unwrap();
        let (target, bytes, digest) = copy_into_place(&file, &into).unwrap();
        assert_eq!(target, into.join("a.txt"));
        assert_eq!(bytes, 5);
        assert_eq!(
            digest,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"hello");
        // Replacing a file goes through the temporary sibling too.
        std::fs::write(&file, b"again").unwrap();
        copy_into_place(&file, &into.join("a.txt")).unwrap();
        assert_eq!(std::fs::read(into.join("a.txt")).unwrap(), b"again");

        let tree = dir.path().join("tree");
        std::fs::create_dir_all(tree.join("sub")).unwrap();
        std::fs::write(tree.join("sub/b"), b"b").unwrap();
        let (target, bytes, _) = copy_into_place(&tree, &dir.path().join("copy")).unwrap();
        assert_eq!(bytes, 1);
        assert_eq!(std::fs::read(target.join("sub/b")).unwrap(), b"b");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with('.'))
            .collect();
        assert!(leftovers.is_empty());
    }
}
