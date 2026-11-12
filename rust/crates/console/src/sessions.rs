//! Lease-bound guest-console controller. No Tart API or runtime modification.
//!
//! This is the Rust port of `bin/console_sessions.py`. Only nonsecret
//! observations leave this module. Each attempt owns a separate worker whose
//! controller EOF and monotonic deadline independently revoke streams.
//!
//! One deliberate adaptation: `open` launches the native `console-worker`
//! binary located through `VM_CONSOLE_WORKER` or next to the running
//! executable, invoked as `console-worker --control-fd <fd>` with the control
//! socket descriptor inherited by the child.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};
use thiserror::Error;

use crate::config::ConsoleConfig;
use crate::guest;

/// A guest-console controller failure. The `Display` text matches the Python
/// exception message, including punctuation.
#[derive(Debug, Error)]
pub enum ConsoleError {
    /// The viewer is not configured or the guest prerequisites are missing.
    #[error("VNC unavailable: configure the standard viewer and guest sharing prerequisites")]
    ViewerUnavailable,
    /// The controller is shutting down.
    #[error("Console controller is shutting down")]
    ShuttingDown,
    /// The lease identity did not match the request.
    #[error("Console lease identity mismatch")]
    LeaseIdentityMismatch,
    /// The controller has no matching session for this lease.
    #[error("Console unavailable after restart or for this lease; no automatic recovery")]
    UnavailableAfterRestart,
    /// The console identity did not match the request.
    #[error("Console identity mismatch")]
    IdentityMismatch,
    /// The lease is releasing or the session was revoked.
    #[error("Console access revoked or lease is releasing")]
    AccessRevoked,
    /// The guest address is not a usable IP address.
    #[error("Console guest address invalid")]
    GuestAddressInvalid,
    /// The guest user is not a usable SSH login name.
    #[error("Console guest user invalid")]
    GuestUserInvalid,
    /// The guest reported a standing diagnostic.
    #[error("Guest console is unavailable: {0}")]
    GuestUnavailable(String),
    /// The guest console belongs to another user.
    #[error("Guest console is not ready for the lease user")]
    GuestNotLeaseUser,
    /// The guest did not reach readiness before the deadline.
    #[error("Guest console did not become ready: {0}")]
    GuestNotReady(String),
    /// The guest could not be inspected at all.
    #[error("Guest console preparation failed; verify image prerequisites")]
    PreparationFailed,
    /// The attempt identifier is invalid.
    #[error("Invalid console attempt identifier")]
    InvalidAttemptId,
    /// The console is not ready for an attempt.
    #[error("Guest console is not ready")]
    NotReady,
    /// Another attempt is still active.
    #[error("Another console attempt is active; cancel it first")]
    AttemptActive,
    /// The attempt limit for this session was reached.
    #[error("Console attempt limit reached")]
    AttemptLimit,
    /// The worker could not be launched.
    #[error("Console worker launch failed")]
    WorkerLaunchFailed,
    /// The worker did not report readiness.
    #[error("Console worker readiness timed out")]
    WorkerReadinessTimeout,
    /// The attempt closed before the viewer could be dispatched.
    #[error("Console attempt closed before viewer dispatch")]
    AttemptClosed,
    /// A serialized frame exceeded the protocol limit.
    #[error("Console configuration exceeds protocol limit")]
    ProtocolLimit,
    /// The attempt has no live control channel.
    #[error("Console attempt is no longer active")]
    AttemptInactive,
    /// The control channel failed.
    #[error("Console worker connection lost")]
    ConnectionLost,
    /// A transport-level failure whose text comes from another crate.
    #[error("{0}")]
    Other(String),
}

/// Whether a string is a bounded, safe console identity token.
pub fn identity(value: &Value) -> bool {
    value.as_str().map(identity_str).unwrap_or(false)
}

fn identity_str(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.len() > 120 {
        return false;
    }
    if !bytes[0].is_ascii_alphanumeric() {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'.' || *c == b'-')
}

fn valid_ssh_user(user: &str) -> bool {
    let bytes = user.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    if !(bytes[0].is_ascii_alphabetic() || bytes[0] == b'_') {
        return false;
    }
    let mut index = 1;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'$' {
            return index + 1 == bytes.len();
        }
        if !(byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'.' || byte == b'-') {
            return false;
        }
        index += 1;
    }
    true
}

/// Normalize a guest address the way Python's `ipaddress.ip_address` does.
///
/// CPython accepts `str`, `int`, and `bytes`; `bool` is accepted because it
/// subclasses `int`. An integer below 2**32 renders as IPv4 and anything below
/// 2**128 as IPv6. JSON cannot carry `bytes`, so only the `str`, `int` and
/// `bool` cases are reachable here.
/// Render an IPv6 address the way CPython's `IPv6Address.__str__` does.
///
/// `IPv6Address.__str__` delegates to `_ipv4_mapped_ipv6_to_str` whenever the
/// high 96 bits are exactly `::ffff:0:0`, so a v4-mapped address prints as
/// `::ffff:a.b.c.d` rather than in hexadecimal groups.
fn ipv6_string(address: std::net::Ipv6Addr) -> String {
    match address.to_ipv4_mapped() {
        Some(v4) => format!("::ffff:{v4}"),
        None => address.to_string(),
    }
}

/// Normalize a text address the way `str(ipaddress.ip_address(text))` does.
///
/// CPython's `_split_scope_id` partitions at the first `%` and rejects an empty
/// scope, a scope containing `%`, and a scope containing `/`; an accepted scope
/// is reassembled verbatim, including its case. A scope id is only meaningful
/// on IPv6, so a scoped IPv4 literal such as `127.0.0.1%eth0` is rejected.
fn normalize_ip_text(text: &str) -> Option<String> {
    match text.split_once('%') {
        Some((address, scope)) => {
            if scope.is_empty() || scope.contains('%') || scope.contains('/') {
                return None;
            }
            let parsed: std::net::Ipv6Addr = address.parse().ok()?;
            Some(format!("{}%{scope}", ipv6_string(parsed)))
        }
        None => match text.parse::<std::net::IpAddr>().ok()? {
            std::net::IpAddr::V4(v4) => Some(v4.to_string()),
            std::net::IpAddr::V6(v6) => Some(ipv6_string(v6)),
        },
    }
}

/// Convert a JSON number to Python's `int` when the literal denotes one.
///
/// Python's `json` parses an integer of any width exactly, and
/// `ipaddress.ip_address` accepts it while it fits in 128 bits. The workspace
/// enables `serde_json`'s `arbitrary_precision` feature so an integer wider than
/// `u64` keeps its digits. A decimal point, an exponent, or a sign other than
/// `-0` means Python's `json` produced a `float`, which
/// `ipaddress.ip_address` rejects.
fn python_int_from_json(number: &serde_json::Number) -> Option<u128> {
    let text = number.to_string();
    let digits = match text.strip_prefix('-') {
        // Python's `json` parses `-0` as the integer zero.
        Some(rest) if !rest.is_empty() && rest.bytes().all(|byte| byte == b'0') => rest,
        Some(_) => return None,
        None => text.as_str(),
    };
    if digits.is_empty() {
        return None;
    }
    let mut value: u128 = 0;
    for byte in digits.bytes() {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value
            .checked_mul(10)?
            .checked_add(u128::from(byte - b'0'))?;
    }
    Some(value)
}

fn normalize_ip(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => normalize_ip_text(text),
        Value::Bool(flag) => Some(std::net::Ipv4Addr::from(u32::from(*flag)).to_string()),
        Value::Number(number) => python_int_from_json(number).map(|raw| {
            if raw <= u128::from(u32::MAX) {
                std::net::Ipv4Addr::from(raw as u32).to_string()
            } else {
                ipv6_string(std::net::Ipv6Addr::from(raw))
            }
        }),
        _ => None,
    }
}

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0)
}

fn now_instant() -> Instant {
    Instant::now()
}

fn monotonic_origin() -> (Instant, f64) {
    static ORIGIN: OnceLock<(Instant, f64)> = OnceLock::new();
    *ORIGIN.get_or_init(|| {
        // SAFETY: `clock_gettime` writes to a valid out-pointer.
        let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
        let raw = unsafe {
            if libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) == 0 {
                ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9
            } else {
                0.0
            }
        };
        (Instant::now(), raw)
    })
}

fn mono_seconds(instant: Instant) -> f64 {
    let (base, raw) = monotonic_origin();
    raw + instant.saturating_duration_since(base).as_secs_f64()
}

fn f64_value(value: f64) -> Value {
    serde_json::Number::from_f64(value)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

fn random_password() -> Result<String, ConsoleError> {
    use base64::Engine as _;
    use rand::TryRng as _;
    let mut bytes = [0u8; 6];
    rand::rngs::SysRng
        .try_fill_bytes(&mut bytes)
        .map_err(|_| ConsoleError::Other("console credential generation failed".to_string()))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

fn worker_path() -> PathBuf {
    if let Some(path) = std::env::var_os("VM_CONSOLE_WORKER") {
        return PathBuf::from(path);
    }
    // The installer puts the worker beside the daemon (IN-1).
    let worker = service_layout::Executable::ConsoleWorker;
    service_layout::beside_daemon(worker).unwrap_or_else(|| PathBuf::from(worker.name()))
}

/// A simple set-once event with a bounded wait, mirroring `threading.Event`.
struct Event {
    state: Mutex<bool>,
    cv: Condvar,
}

impl Event {
    fn new() -> Self {
        Self {
            state: Mutex::new(false),
            cv: Condvar::new(),
        }
    }

    fn set(&self) {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = true;
        self.cv.notify_all();
    }

    fn wait_timeout(&self, timeout: Duration) -> bool {
        let guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *guard {
            return true;
        }
        let (guard, _) = self
            .cv
            .wait_timeout_while(guard, timeout, |set| !*set)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard
    }
}

struct AttemptState {
    status: String,
    transport: bool,
    cleanup: String,
    authentication: String,
    control: Option<Arc<UnixStream>>,
    process: Option<Arc<Mutex<Child>>>,
    terminal: bool,
    reason: Option<String>,
    configured: bool,
}

/// One attempt-local controller channel.
struct Attempt {
    name: String,
    state: Mutex<AttemptState>,
    send_lock: Mutex<()>,
    ready: Event,
}

impl Attempt {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            state: Mutex::new(AttemptState {
                status: "preparing".to_string(),
                transport: false,
                cleanup: "not-started".to_string(),
                authentication: "unverified".to_string(),
                control: None,
                process: None,
                terminal: false,
                reason: None,
                configured: false,
            }),
            send_lock: Mutex::new(()),
            ready: Event::new(),
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, AttemptState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn terminal(&self) -> bool {
        self.lock_state().terminal
    }

    fn is_configured(&self) -> bool {
        self.lock_state().configured
    }

    fn has_control(&self) -> bool {
        self.lock_state().control.is_some()
    }

    fn set_process(&self, process: Arc<Mutex<Child>>) {
        self.lock_state().process = Some(process);
    }

    fn set_control(&self, control: Option<Arc<UnixStream>>) {
        self.lock_state().control = control;
    }

    fn clear_control(&self) {
        self.lock_state().control = None;
    }

    fn set_status(&self, status: &str) {
        self.lock_state().status = status.to_string();
    }

    fn set_cleanup(&self, cleanup: &str) {
        self.lock_state().cleanup = cleanup.to_string();
    }

    fn set_authentication(&self, authentication: &str) {
        self.lock_state().authentication = authentication.to_string();
    }

    fn set_transport(&self, connected: bool) {
        self.lock_state().transport = connected;
    }

    fn cleanup(&self) -> String {
        self.lock_state().cleanup.clone()
    }

    fn set_configured(&self, configured: bool) {
        self.lock_state().configured = configured;
    }

    fn close(&self, reason: &str) {
        {
            let mut state = self.lock_state();
            if state.terminal {
                return;
            }
            state.terminal = true;
            state.status = if reason == "cancelled" {
                "cancelled".to_string()
            } else {
                "closed".to_string()
            };
            state.reason = Some(reason.to_string());
            state.transport = false;
            if state.cleanup != "local-children-stopped" {
                state.cleanup = if state.process.is_some() {
                    "pending".to_string()
                } else {
                    "not-started".to_string()
                };
            }
            if let Some(control) = state.control.as_ref() {
                // EOF revokes the worker; retain the receive half for its final
                // transport/cleanup observations rather than inventing success.
                let _ = control.shutdown(Shutdown::Write);
            }
        }
        self.ready.set();
    }

    fn send(&self, frame: &Value) -> Result<(), ConsoleError> {
        let mut payload = serde_json::to_vec(frame).map_err(|_| ConsoleError::ProtocolLimit)?;
        payload.push(b'\n');
        if payload.len() > 65536 {
            return Err(ConsoleError::ProtocolLimit);
        }
        let _send_guard = self
            .send_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let control = {
            let state = self.lock_state();
            if state.terminal || state.control.is_none() {
                return Err(ConsoleError::AttemptInactive);
            }
            state.control.clone().expect("control checked above")
        };
        control
            .as_ref()
            .write_all(&payload)
            .map_err(|_| ConsoleError::ConnectionLost)
    }

    fn report(&self) -> Value {
        let state = self.lock_state();
        let mut result = Map::new();
        result.insert("attempt_id".into(), Value::String(self.name.clone()));
        result.insert("status".into(), Value::String(state.status.clone()));
        result.insert(
            "reason".into(),
            state
                .reason
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        result.insert("transport_connected".into(), Value::Bool(state.transport));
        result.insert(
            "viewer_cleanup".into(),
            Value::String(state.cleanup.clone()),
        );
        result.insert(
            "authentication".into(),
            Value::String(state.authentication.clone()),
        );
        result.insert(
            "viewer_connected".into(),
            Value::String("unverified".into()),
        );
        result.insert(
            "human_confirmation".into(),
            Value::String("unverified".into()),
        );
        Value::Object(result)
    }
}

struct Session {
    lease_id: String,
    environment_fingerprint: Option<Value>,
    console_id: String,
    kind: String,
    status: String,
    reason: Option<String>,
    generation: i64,
    deadline: Instant,
    access_expires_at: f64,
    attempts: Vec<(String, Arc<Attempt>)>,
    guest: Option<Map<String, Value>>,
    ssh_argv: Option<Vec<String>>,
}

struct Inner {
    sessions: HashMap<String, Session>,
    closed: bool,
}

/// The lease-bound console controller.
pub struct Manager {
    config: ConsoleConfig,
    inner: Mutex<Inner>,
}

impl Manager {
    /// Build a controller from an optional trusted configuration.
    pub fn new(config: Option<ConsoleConfig>) -> Self {
        Self {
            config: config.unwrap_or_else(ConsoleConfig::disabled),
            inner: Mutex::new(Inner {
                sessions: HashMap::new(),
                closed: false,
            }),
        }
    }

    fn lock_inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The advertised console capabilities, without probing anything.
    pub fn capabilities(&self) -> Value {
        let mut result = Map::new();
        for kind in ["linux", "macos"] {
            let viewer = self.config.viewer(kind);
            let enabled =
                self.config.enabled && viewer.map(|path| !path.is_empty()).unwrap_or(false);
            let mut capability = Map::new();
            capability.insert("available".into(), Value::Bool(enabled));
            capability.insert(
                "status".into(),
                Value::String(if enabled { "configured" } else { "unavailable" }.to_string()),
            );
            capability.insert(
                "reason".into(),
                if enabled {
                    Value::Null
                } else {
                    Value::String("console configuration or viewer missing".to_string())
                },
            );
            capability.insert(
                "backend".into(),
                Value::String(
                    if kind == "linux" {
                        "x11vnc-inetd"
                    } else {
                        "apple-screen-sharing"
                    }
                    .to_string(),
                ),
            );
            capability.insert(
                "guest_readiness".into(),
                Value::String("unverified".to_string()),
            );
            capability.insert(
                "viewer_location".into(),
                Value::String("service-host".to_string()),
            );
            capability.insert(
                "authentication".into(),
                Value::String(
                    if kind == "linux" {
                        "private-stdin"
                    } else {
                        "human-guest-account-prompt"
                    }
                    .to_string(),
                ),
            );
            capability.insert(
                "session_binding".into(),
                Value::String(
                    if kind == "linux" {
                        "existing-x11-session"
                    } else {
                        "viewer-selection-unverified"
                    }
                    .to_string(),
                ),
            );
            capability.insert(
                "server_enforced_view_only".into(),
                Value::Bool(kind == "linux"),
            );
            capability.insert("pixels".into(), Value::String("unverified".to_string()));
            capability.insert(
                "human_confirmation".into(),
                Value::String("unverified".to_string()),
            );
            result.insert(kind.to_string(), Value::Object(capability));
        }
        Value::Object(result)
    }

    /// Require a console kind to be configured before allocation.
    pub fn require_available(&self, kind: &str) -> Result<(), ConsoleError> {
        let available = self
            .config
            .viewer(kind)
            .map(|viewer| self.config.enabled && !viewer.is_empty())
            .unwrap_or(false);
        if available {
            Ok(())
        } else {
            Err(ConsoleError::ViewerUnavailable)
        }
    }

    /// Reserve a session inside the lease reservation transaction; no external
    /// I/O.
    pub fn reserve(&self, record: &Map<String, Value>) -> Result<(), ConsoleError> {
        let mut inner = self.lock_inner();
        if inner.closed {
            return Err(ConsoleError::ShuttingDown);
        }
        let vm = record
            .get("vm")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let lease_id = record
            .get("lease_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let kind = record
            .get("image_kind")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let ttl = record
            .get("ttl_expires_at")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let session = Session {
            lease_id,
            environment_fingerprint: record.get("environment_fingerprint").cloned(),
            console_id: uuid::Uuid::new_v4().simple().to_string(),
            kind,
            status: "preparing".to_string(),
            reason: None,
            generation: 0,
            deadline: now_instant() + Duration::from_secs_f64((ttl - unix_now()).max(0.0)),
            access_expires_at: ttl,
            attempts: Vec::new(),
            guest: None,
            ssh_argv: None,
        };
        inner.sessions.insert(vm, session);
        Ok(())
    }

    /// Bind the guest's existing console, waiting only for states still
    /// arriving.
    pub fn prepare(
        &self,
        record: &Map<String, Value>,
        key_dir: &Path,
        timeout_s: u64,
    ) -> Result<Map<String, Value>, ConsoleError> {
        let kind = {
            let mut inner = self.lock_inner();
            let lease_id = record.get("lease_id").and_then(Value::as_str);
            let session = current(&mut inner, record, lease_id, None, true)?;
            session.kind.clone()
        };
        self.upload_agent(record, key_dir, &kind)?;
        let deadline = now_instant() + Duration::from_secs(timeout_s);
        self.finish_prepare(
            record,
            &kind,
            deadline,
            |command| self.ssh_command(record, key_dir, command),
            run_probe,
        )
    }

    fn finish_prepare<F, P>(
        &self,
        record: &Map<String, Value>,
        kind: &str,
        deadline: Instant,
        mut make_ssh: F,
        mut run_probe: P,
    ) -> Result<Map<String, Value>, ConsoleError>
    where
        F: FnMut(&str) -> Result<Vec<String>, ConsoleError>,
        P: FnMut(&[String]) -> ProbeStep,
    {
        let probe_command =
            guest::probe_command(kind).map_err(|error| ConsoleError::Other(error.to_string()))?;
        let probe_argv = make_ssh(&probe_command)?;
        let (guest, stream_argv) = loop {
            match run_probe(&probe_argv) {
                ProbeStep::Output(bytes) => {
                    let guest =
                        guest::parse_probe(&bytes).map_err(|_| ConsoleError::PreparationFailed)?;
                    let ready = guest.get("ready").and_then(Value::as_bool).unwrap_or(false);
                    if !ready {
                        let code = guest::diagnostic(&guest);
                        if !guest::is_pending(code) {
                            return Err(ConsoleError::GuestUnavailable(code.to_string()));
                        }
                        self.await_pending(code, deadline)?;
                        continue;
                    }
                    let kind_matches = guest.get("kind").and_then(Value::as_str) == Some(kind);
                    let user_matches = guest
                        .get("session")
                        .and_then(|session| session.get("user"))
                        .and_then(Value::as_str)
                        == record.get("ssh_user").and_then(Value::as_str);
                    if !kind_matches || !user_matches {
                        return Err(ConsoleError::GuestNotLeaseUser);
                    }
                    let stream_command = guest::stream_command(kind)
                        .map_err(|error| ConsoleError::Other(error.to_string()))?;
                    let stream_argv = make_ssh(&stream_command)?;
                    break (guest, stream_argv);
                }
                ProbeStep::Unanswered => self.await_pending("probe_unanswered", deadline)?,
                ProbeStep::Failed => return Err(ConsoleError::PreparationFailed),
            }
        };
        let mut inner = self.lock_inner();
        let lease_id = record.get("lease_id").and_then(Value::as_str);
        let session = current(&mut inner, record, lease_id, None, true)?;
        session.guest = Some(guest);
        session.ssh_argv = Some(stream_argv);
        session.status = "ready".to_string();
        Ok(self.report(record, session, None))
    }

    fn await_pending(&self, code: &str, deadline: Instant) -> Result<(), ConsoleError> {
        let remaining = deadline.saturating_duration_since(now_instant());
        if remaining.is_zero() {
            return Err(ConsoleError::GuestNotReady(code.to_string()));
        }
        let sleep = remaining.min(Duration::from_secs_f64(2.0));
        std::thread::sleep(sleep);
        Ok(())
    }

    fn upload_agent(
        &self,
        record: &Map<String, Value>,
        key_dir: &Path,
        kind: &str,
    ) -> Result<(), ConsoleError> {
        // The delivered agent must match the guest's OS, not the host's.
        let bytes = guest::agent_bytes_for(kind).map_err(|_| ConsoleError::PreparationFailed)?;
        let argv = self.ssh_command(record, key_dir, &guest::upload_command())?;
        let mut command = service_layout::tools::command_for(&argv[0])
            .map_err(|_| ConsoleError::PreparationFailed)?;
        command.args(&argv[1..]);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        match run_capture(command, Some(bytes), Duration::from_secs(30)) {
            Ok(output) if output.status.success() => Ok(()),
            _ => Err(ConsoleError::PreparationFailed),
        }
    }

    fn ssh_command(
        &self,
        record: &Map<String, Value>,
        key_dir: &Path,
        command: &str,
    ) -> Result<Vec<String>, ConsoleError> {
        let ip = normalize_ip(record.get("ip")).ok_or(ConsoleError::GuestAddressInvalid)?;
        let user = record
            .get("ssh_user")
            .and_then(Value::as_str)
            .filter(|user| valid_ssh_user(user))
            .ok_or(ConsoleError::GuestUserInvalid)?;
        let keys = lease_keys::key_args(key_dir)
            .map_err(|error| ConsoleError::Other(error.to_string()))?;
        let mut argv = Vec::with_capacity(keys.len() + 7);
        argv.push("/usr/bin/ssh".to_string());
        argv.extend(keys);
        argv.push("-T".to_string());
        argv.push("-l".to_string());
        argv.push(user.to_string());
        argv.push("--".to_string());
        argv.push(ip);
        argv.push(command.to_string());
        Ok(argv)
    }

    fn report(
        &self,
        record: &Map<String, Value>,
        session: &Session,
        attempt: Option<&Arc<Attempt>>,
    ) -> Map<String, Value> {
        let capabilities = self.capabilities();
        let backend = capabilities
            .get(&session.kind)
            .and_then(|kind| kind.get("backend"))
            .cloned()
            .unwrap_or(Value::Null);
        let authentication_mechanism = capabilities
            .get(&session.kind)
            .and_then(|kind| kind.get("authentication"))
            .cloned()
            .unwrap_or(Value::Null);
        let mut result = Map::new();
        result.insert("schemaVersion".into(), Value::Number(1.into()));
        result.insert(
            "vm".into(),
            record.get("vm").cloned().unwrap_or(Value::Null),
        );
        result.insert("lease_id".into(), Value::String(session.lease_id.clone()));
        result.insert(
            "environment_fingerprint".into(),
            session
                .environment_fingerprint
                .clone()
                .unwrap_or(Value::Null),
        );
        result.insert(
            "console_id".into(),
            Value::String(session.console_id.clone()),
        );
        result.insert("status".into(), Value::String(session.status.clone()));
        result.insert(
            "reason".into(),
            session
                .reason
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        result.insert("backend".into(), backend);
        result.insert(
            "viewer_location".into(),
            Value::String("service-host".to_string()),
        );
        result.insert(
            "source".into(),
            Value::String("guest-sharing-controller".to_string()),
        );
        result.insert("observed_at".into(), f64_value(unix_now()));
        result.insert(
            "access_expires_at".into(),
            f64_value(session.access_expires_at),
        );
        result.insert(
            "viewer_connected".into(),
            Value::String("unverified".to_string()),
        );
        result.insert(
            "human_confirmation".into(),
            Value::String("unverified".to_string()),
        );
        result.insert(
            "session_binding".into(),
            Value::String(
                if session.kind == "macos" {
                    "viewer-selection-unverified"
                } else {
                    "existing-x11-session"
                }
                .to_string(),
            ),
        );
        result.insert(
            "readiness_scope".into(),
            Value::String("guest-console-preflight".to_string()),
        );
        result.insert(
            "server_enforced_view_only".into(),
            Value::Bool(session.kind == "linux"),
        );
        result.insert("authentication_mechanism".into(), authentication_mechanism);
        result.insert(
            "authentication".into(),
            Value::String("unverified".to_string()),
        );
        result.insert("pixels".into(), Value::String("unverified".to_string()));
        if let Some(attempt) = attempt {
            result.insert("attempt".into(), attempt.report());
        }
        result
    }

    /// Resolve a session report without requiring it to be currently active.
    pub fn resolve(
        &self,
        record: &Map<String, Value>,
        lease_id: Option<&str>,
    ) -> Result<Map<String, Value>, ConsoleError> {
        let mut inner = self.lock_inner();
        let session = current(&mut inner, record, lease_id, None, false)?;
        let last = session.attempts.last().map(|(_, attempt)| attempt.clone());
        Ok(self.report(record, session, last.as_ref()))
    }

    /// Open a viewer attempt and dispatch its configuration to a worker.
    pub fn open(
        &self,
        record: &Map<String, Value>,
        lease_id: &str,
        console_id: &str,
        attempt_id: &str,
    ) -> Result<Value, ConsoleError> {
        if !identity_str(attempt_id) {
            return Err(ConsoleError::InvalidAttemptId);
        }
        let vm = record
            .get("vm")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let (attempt, mut config, generation_at_configure) = {
            let mut inner = self.lock_inner();
            let session = current(&mut inner, record, Some(lease_id), Some(console_id), true)?;
            if let Some(existing) = find_attempt(session, attempt_id) {
                return Ok(Value::Object(self.report(record, session, Some(&existing))));
            }
            if session.status != "ready" {
                return Err(ConsoleError::NotReady);
            }
            if session
                .attempts
                .iter()
                .any(|(_, attempt)| !attempt.terminal())
            {
                return Err(ConsoleError::AttemptActive);
            }
            if session.attempts.len() >= 64 {
                return Err(ConsoleError::AttemptLimit);
            }
            let attempt = Arc::new(Attempt::new(attempt_id));
            session
                .attempts
                .push((attempt_id.to_string(), attempt.clone()));
            let kind = session.kind.clone();
            let password = if kind == "linux" {
                Some(random_password()?)
            } else {
                None
            };
            let guest_session = session
                .guest
                .as_ref()
                .and_then(|guest| guest.get("session"))
                .cloned()
                .unwrap_or(Value::Null);
            let mut guest_config = Map::new();
            guest_config.insert("version".into(), Value::Number(1.into()));
            guest_config.insert("session".into(), guest_session);
            guest_config.insert("expires_at".into(), f64_value(unix_now() + 720.0 * 3600.0));
            if let Some(password) = &password {
                guest_config.insert("password".into(), Value::String(password.clone()));
            }
            let mut viewer = Map::new();
            viewer.insert(
                "kind".into(),
                Value::String(if kind == "linux" { "turbovnc" } else { "apple" }.to_string()),
            );
            viewer.insert(
                "executable".into(),
                Value::String(self.config.viewer(&kind).unwrap_or_default().to_string()),
            );
            if let Some(password) = &password {
                viewer.insert("password".into(), Value::String(password.clone()));
            }
            let ssh_argv = session.ssh_argv.clone().unwrap_or_default();
            let mut config = Map::new();
            config.insert("op".into(), Value::String("configure".to_string()));
            config.insert("deadline".into(), f64_value(mono_seconds(session.deadline)));
            config.insert(
                "generation".into(),
                Value::Number(session.generation.into()),
            );
            config.insert(
                "ssh_argv".into(),
                Value::Array(ssh_argv.into_iter().map(Value::String).collect()),
            );
            config.insert("guest_config".into(), Value::Object(guest_config));
            config.insert("viewer".into(), Value::Object(viewer));
            config.insert("connection_limit".into(), Value::Number(1.into()));
            let generation = session.generation;
            (attempt, config, generation)
        };
        // Launch never holds the controller/state lock needed for cancellation.
        let (parent, child) = UnixStream::pair().map_err(|_| {
            self.fail_launch(&attempt);
            ConsoleError::WorkerLaunchFailed
        })?;
        let _ = parent.set_read_timeout(Some(Duration::from_secs(1)));
        let _ = parent.set_write_timeout(Some(Duration::from_secs(1)));
        let parent = Arc::new(parent);
        let child_fd = child.as_raw_fd();
        let worker = worker_path();
        let mut command = Command::new(&worker);
        command
            .arg("--control-fd")
            .arg(child_fd.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: the closure runs in the forked child before exec; it only
        // creates a session and clears FD_CLOEXEC on the control descriptor.
        unsafe {
            use std::os::unix::process::CommandExt;
            command.pre_exec(move || {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                let flags = libc::fcntl(child_fd, libc::F_GETFD);
                if flags < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::fcntl(child_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child_process = match command.spawn() {
            Ok(process) => process,
            Err(_) => {
                drop(child);
                self.fail_launch(&attempt);
                return Err(ConsoleError::WorkerLaunchFailed);
            }
        };
        drop(child);
        let process = Arc::new(Mutex::new(child_process));
        {
            let mut inner = self.lock_inner();
            attempt.set_process(process.clone());
            if let Some(session) = inner.sessions.get_mut(&vm) {
                let expired = session.status == "revoked" || session.deadline <= now_instant();
                if attempt.terminal() || expired {
                    let _ = parent.shutdown(Shutdown::Both);
                    attempt.set_cleanup("pending");
                } else {
                    attempt.set_control(Some(parent.clone()));
                    attempt.set_status("dispatching");
                }
                // Capture the latest authoritative grant, not the pre-launch
                // snapshot.
                config.insert("deadline".into(), f64_value(mono_seconds(session.deadline)));
                config.insert(
                    "generation".into(),
                    Value::Number(session.generation.into()),
                );
            } else {
                let _ = parent.shutdown(Shutdown::Both);
                attempt.set_cleanup("pending");
            }
        }
        let watch_attempt = attempt.clone();
        let watch_control = parent.clone();
        let watch_process = process.clone();
        let _ = std::thread::Builder::new()
            .name(format!("console-watch-{attempt_id}"))
            .spawn(move || watch(watch_attempt, watch_control, watch_process));
        let outcome = (|| -> Result<(), ConsoleError> {
            attempt.send(&Value::Object(config))?;
            let latest = {
                let mut inner = self.lock_inner();
                attempt.set_configured(true);
                match inner.sessions.get_mut(&vm) {
                    Some(session) => (session.generation, session.deadline),
                    None => (generation_at_configure, now_instant()),
                }
            };
            if latest.0 > generation_at_configure {
                let mut renew = Map::new();
                renew.insert("op".into(), Value::String("renew".to_string()));
                renew.insert("generation".into(), Value::Number(latest.0.into()));
                renew.insert("deadline".into(), f64_value(mono_seconds(latest.1)));
                attempt.send(&Value::Object(renew))?;
            }
            if !attempt.ready.wait_timeout(Duration::from_secs(5)) {
                return Err(ConsoleError::WorkerReadinessTimeout);
            }
            {
                let mut inner = self.lock_inner();
                current(&mut inner, record, Some(lease_id), Some(console_id), true)?;
            }
            if attempt.terminal() {
                return Err(ConsoleError::AttemptClosed);
            }
            attempt.send(&serde_json::json!({ "op": "open" }))
        })();
        if let Err(error) = outcome {
            let inner = self.lock_inner();
            if !attempt.terminal() {
                attempt.close("worker-unavailable");
                attempt.set_status("failed");
            }
            drop(inner);
            return Err(error);
        }
        let mut inner = self.lock_inner();
        let session = current(&mut inner, record, Some(lease_id), Some(console_id), true)?;
        Ok(Value::Object(self.report(record, session, Some(&attempt))))
    }

    fn fail_launch(&self, attempt: &Arc<Attempt>) {
        let _inner = self.lock_inner();
        if !attempt.terminal() {
            attempt.close("worker-launch-failed");
            attempt.set_status("failed");
        }
    }

    /// Cancel an attempt, creating a closed placeholder if it never existed.
    pub fn cancel(
        &self,
        record: &Map<String, Value>,
        lease_id: &str,
        console_id: &str,
        attempt_id: &str,
    ) -> Result<Value, ConsoleError> {
        if !identity_str(attempt_id) {
            return Err(ConsoleError::InvalidAttemptId);
        }
        let mut inner = self.lock_inner();
        let session = current(&mut inner, record, Some(lease_id), Some(console_id), false)?;
        let attempt = match find_attempt(session, attempt_id) {
            Some(attempt) => attempt,
            None => {
                if session.attempts.len() >= 64 {
                    return Err(ConsoleError::AttemptLimit);
                }
                let attempt = Arc::new(Attempt::new(attempt_id));
                session
                    .attempts
                    .push((attempt_id.to_string(), attempt.clone()));
                attempt
            }
        };
        attempt.close("cancelled");
        Ok(Value::Object(self.report(record, session, Some(&attempt))))
    }

    /// Publish a renewed deadline to every live attempt.
    pub fn renew(&self, record: &Map<String, Value>, deadline: Option<Instant>) {
        let frames: Vec<(Arc<Attempt>, Value)> = {
            let mut inner = self.lock_inner();
            let vm = record
                .get("vm")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let lease = record.get("lease_id").and_then(Value::as_str);
            let session = match inner.sessions.get_mut(&vm) {
                Some(session) => session,
                None => return,
            };
            if Some(session.lease_id.as_str()) != lease {
                return;
            }
            if session.status == "revoked" || session.deadline <= now_instant() {
                revoke_session(session, "expired");
                return;
            }
            session.generation += 1;
            let ttl = record
                .get("ttl_expires_at")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            session.deadline = deadline.unwrap_or_else(|| {
                now_instant() + Duration::from_secs_f64((ttl - unix_now()).max(0.0))
            });
            session.access_expires_at = ttl;
            let mut frame = Map::new();
            frame.insert("op".into(), Value::String("renew".to_string()));
            frame.insert(
                "generation".into(),
                Value::Number(session.generation.into()),
            );
            frame.insert("deadline".into(), f64_value(mono_seconds(session.deadline)));
            let frame = Value::Object(frame);
            session
                .attempts
                .iter()
                .filter(|(_, attempt)| {
                    !attempt.terminal() && attempt.has_control() && attempt.is_configured()
                })
                .map(|(_, attempt)| (attempt.clone(), frame.clone()))
                .collect()
        };
        for (attempt, frame) in frames {
            std::thread::spawn(move || {
                if attempt.send(&frame).is_err() && !attempt.terminal() {
                    attempt.close("renewal-failed");
                }
            });
        }
    }

    /// Revoke the session bound to a record's lease.
    pub fn revoke(&self, record: &Map<String, Value>, reason: &str) {
        let mut inner = self.lock_inner();
        let vm = record
            .get("vm")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let lease = record.get("lease_id").and_then(Value::as_str);
        if let Some(session) = inner.sessions.get_mut(&vm) {
            if Some(session.lease_id.as_str()) == lease {
                revoke_session(session, reason);
            }
        }
    }

    /// Forget a VM's session and revoke it.
    pub fn forget(&self, vm: &str) {
        let mut inner = self.lock_inner();
        if let Some(mut session) = inner.sessions.remove(vm) {
            revoke_session(&mut session, "released");
        }
    }

    /// Revoke every session and mark the controller closed.
    pub fn shutdown(&self) {
        let mut inner = self.lock_inner();
        inner.closed = true;
        for session in inner.sessions.values_mut() {
            revoke_session(session, "controller-shutdown");
        }
    }
}

fn find_attempt(session: &Session, attempt_id: &str) -> Option<Arc<Attempt>> {
    session
        .attempts
        .iter()
        .find(|(id, _)| id == attempt_id)
        .map(|(_, attempt)| attempt.clone())
}

fn revoke_session(session: &mut Session, reason: &str) {
    session.status = "revoked".to_string();
    session.reason = Some(reason.to_string());
    for (_, attempt) in session.attempts.iter() {
        if !attempt.terminal() {
            attempt.close(reason);
        }
    }
}

fn current<'a>(
    inner: &'a mut Inner,
    record: &Map<String, Value>,
    lease_id: Option<&str>,
    console_id: Option<&str>,
    active: bool,
) -> Result<&'a mut Session, ConsoleError> {
    let lease_mismatch = match lease_id {
        Some(lease) => {
            record.get("lease_id").and_then(Value::as_str) != Some(lease) || !identity_str(lease)
        }
        None => true,
    };
    if lease_mismatch {
        return Err(ConsoleError::LeaseIdentityMismatch);
    }
    let vm = record
        .get("vm")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let session = inner
        .sessions
        .get_mut(&vm)
        .ok_or(ConsoleError::UnavailableAfterRestart)?;
    if Some(session.lease_id.as_str()) != lease_id
        || session.environment_fingerprint.as_ref() != record.get("environment_fingerprint")
    {
        return Err(ConsoleError::UnavailableAfterRestart);
    }
    if let Some(console_id) = console_id {
        if session.console_id != console_id {
            return Err(ConsoleError::IdentityMismatch);
        }
    }
    if session.deadline <= now_instant() && session.status != "revoked" {
        revoke_session(session, "expired");
    }
    if active {
        let state = record.get("state").and_then(Value::as_str);
        let known = matches!(
            state,
            Some("running") | Some("provisioning") | Some("pending")
        );
        if !known || session.status == "revoked" {
            return Err(ConsoleError::AccessRevoked);
        }
    }
    Ok(session)
}

/// The observed result of one probe invocation.
enum ProbeStep {
    /// A command completed with its stdout.
    Output(Vec<u8>),
    /// The command failed or wrote more than the bounded record.
    Unanswered,
    /// The command could not be run or timed out.
    Failed,
}

fn run_probe(argv: &[String]) -> ProbeStep {
    let Ok(mut command) = service_layout::tools::command_for(&argv[0]) else {
        return ProbeStep::Failed;
    };
    command.args(&argv[1..]);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    match run_capture(command, None, Duration::from_secs(30)) {
        Ok(output) if output.status.success() && output.stdout.len() <= 16384 => {
            ProbeStep::Output(output.stdout)
        }
        Ok(_) => ProbeStep::Unanswered,
        Err(_) => ProbeStep::Failed,
    }
}

struct Captured {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
}

fn run_capture(
    mut command: Command,
    stdin: Option<Vec<u8>>,
    timeout: Duration,
) -> io::Result<Captured> {
    if stdin.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command.spawn()?;
    let writer = stdin.map(|bytes| {
        let mut sink = child.stdin.take().expect("stdin is piped when requested");
        std::thread::spawn(move || {
            let _ = sink.write_all(&bytes);
        })
    });
    let reader = child.stdout.take().map(|mut source| {
        std::thread::spawn(move || {
            let mut buffer = Vec::new();
            let _ = source.read_to_end(&mut buffer);
            buffer
        })
    });
    let start = Instant::now();
    loop {
        match child.try_wait()? {
            Some(status) => {
                if let Some(writer) = writer {
                    let _ = writer.join();
                }
                let stdout = reader
                    .map(|reader| reader.join().unwrap_or_default())
                    .unwrap_or_default();
                return Ok(Captured { status, stdout });
            }
            None => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "timed out"));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

fn watch(attempt: Arc<Attempt>, control: Arc<UnixStream>, process: Arc<Mutex<Child>>) {
    let mut pending: Vec<u8> = Vec::new();
    'outer: loop {
        let mut buffer = [0u8; 8192];
        match (&*control).read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                pending.extend_from_slice(&buffer[..read]);
                if pending.len() > 65536 {
                    break;
                }
                while let Some(position) = pending.iter().position(|byte| *byte == b'\n') {
                    let line: Vec<u8> = pending.drain(..=position).collect();
                    let message: Value = match serde_json::from_slice(&line[..line.len() - 1]) {
                        Ok(value) => value,
                        Err(_) => break 'outer,
                    };
                    let message = match message {
                        Value::Object(object) => object,
                        _ => break 'outer,
                    };
                    handle_event(&attempt, &message);
                }
            }
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock
                    || error.kind() == io::ErrorKind::TimedOut =>
            {
                let exited = process
                    .lock()
                    .map(|mut child| child.try_wait().ok().flatten().is_some())
                    .unwrap_or(false);
                if exited {
                    break;
                }
                continue;
            }
            Err(_) => break,
        }
    }
    if !attempt.terminal() {
        attempt.close("worker-connection-lost");
    }
    attempt.clear_control();
    attempt.ready.set();
    if let Ok(mut child) = process.lock() {
        let _ = child.wait();
    }
    // Worker exit is not evidence that Apple's GUI app closed.
    if attempt.cleanup() != "local-children-stopped" {
        attempt.set_cleanup("unconfirmed");
    }
}

fn handle_event(attempt: &Arc<Attempt>, message: &Map<String, Value>) {
    match message.get("op").and_then(Value::as_str) {
        Some("ready") => attempt.ready.set(),
        Some("launched") => {
            if !attempt.terminal() {
                attempt.set_status("launched");
                let authentication =
                    if message.get("authentication").and_then(Value::as_str) == Some("required") {
                        "required"
                    } else {
                        "unverified"
                    };
                attempt.set_authentication(authentication);
            }
        }
        Some("transport") => {
            if !attempt.terminal() {
                let connected = matches!(message.get("connected"), Some(Value::Bool(true)));
                attempt.set_transport(connected);
            }
        }
        Some("closed") => {
            if !attempt.terminal() {
                attempt.close("worker-closed");
            }
        }
        Some("error") => {
            if message.get("reason").and_then(Value::as_str) == Some("stale_generation") {
                return;
            }
            if !attempt.terminal() {
                attempt.close("worker-failed");
            }
        }
        Some("cleanup")
            if message.get("state").and_then(Value::as_str) == Some("local_children_stopped") =>
        {
            attempt.set_cleanup("local-children-stopped");
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guest::PROTOCOL_VERSION;
    use serde_json::json;

    /// Parity check for the predicate behind `ConsoleError::GuestAddressInvalid`.
    ///
    /// Not a port of a Python test: `test_console_sessions.py` has no direct test
    /// of `_ssh`'s address normalization. The expectations below were produced by
    /// running CPython's `str(ipaddress.ip_address(value))` on each input.
    #[test]
    fn guest_address_normalisation_matches_python_ipaddress() {
        let literal = |text: &str| serde_json::from_str::<Value>(text).expect("JSON literal");
        let cases: Vec<(Value, Option<&str>)> = vec![
            (json!("1.2.3.4"), Some("1.2.3.4")),
            (json!("::1"), Some("::1")),
            (json!("::ffff:1.2.3.4"), Some("::ffff:1.2.3.4")),
            // v4-mapped rendering: CPython prints the low 32 bits in dotted form.
            (json!("::ffff:7f00:1"), Some("::ffff:127.0.0.1")),
            (json!("0:0:0:0:0:ffff:7f00:1"), Some("::ffff:127.0.0.1")),
            (json!("::ffff:0:0"), Some("::ffff:0.0.0.0")),
            (json!("1::ffff:7f00:1"), Some("1::ffff:7f00:1")),
            (json!("0:0:1:0:0:0:0:1"), Some("0:0:1::1")),
            (
                json!("ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff"),
                Some("ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff"),
            ),
            // RFC 4007 scope ids: accepted verbatim unless empty or containing `%`.
            (json!("fe80::1%eth0"), Some("fe80::1%eth0")),
            (json!("FE80::01%eth0"), Some("fe80::1%eth0")),
            (json!("fe80::1%ETH0"), Some("fe80::1%ETH0")),
            (json!("::1%eth0"), Some("::1%eth0")),
            (json!("fe80::1%a b"), Some("fe80::1%a b")),
            // CPython rejects `%` and `/` inside a scope.
            (json!("fe80::1%/64"), None),
            (json!("fe80::1%a/b"), None),
            (json!("::1%a/b"), None),
            (json!("::ffff:7f00:1%eth0"), Some("::ffff:127.0.0.1%eth0")),
            (json!("fe80::1%"), None),
            (json!("fe80::1%eth0%1"), None),
            (json!("127.0.0.1%eth0"), None),
            (json!("  fe80::1"), None),
            (json!("fe80::1 "), None),
            (json!("g::1"), None),
            (json!(0), Some("0.0.0.0")),
            (json!(1), Some("0.0.0.1")),
            (json!(4294967295u32), Some("255.255.255.255")),
            (json!(4294967296i64), Some("::1:0:0")),
            (
                json!(18446744073709551615u64),
                Some("::ffff:ffff:ffff:ffff"),
            ),
            // Integers wider than `u64` keep their digits because the workspace
            // enables `serde_json`'s `arbitrary_precision` feature.
            (literal("281474976710655"), Some("::ffff:255.255.255.255")),
            (literal("18446744073709551616"), Some("0:0:0:1::")),
            (literal("79228162514264337593543950336"), Some("0:1::")),
            (
                literal("340282366920938463463374607431768211455"),
                Some("ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff"),
            ),
            (literal("340282366920938463463374607431768211456"), None),
            // Python's `json` parses `-0` as the integer zero.
            (literal("-0"), Some("0.0.0.0")),
            (literal("-1"), None),
            // A decimal point or an exponent means Python's `json` produced a
            // `float`, which `ipaddress.ip_address` rejects.
            (literal("1.0"), None),
            (literal("1e39"), None),
            (json!(true), Some("0.0.0.1")),
            (json!(false), Some("0.0.0.0")),
            (json!(-1), None),
            (json!(1.5), None),
            (json!(1.0e39), None),
            (json!(null), None),
            (json!(""), None),
            (json!("01.2.3.4"), None),
            (json!("0x1"), None),
            (json!("1"), None),
        ];
        for (value, expected) in cases {
            assert_eq!(
                normalize_ip(Some(&value)).as_deref(),
                expected,
                "input {value} should normalise as Python does"
            );
        }
        assert_eq!(normalize_ip(None), None);
    }

    fn config(kind: &str) -> ConsoleConfig {
        let mut config = ConsoleConfig::disabled();
        config.enabled = true;
        match kind {
            "linux" => config.linux_viewer = Some("/fake/viewer".to_string()),
            _ => config.macos_viewer = Some("/fake/Screen Sharing.app".to_string()),
        }
        config
    }

    fn linux_record() -> Map<String, Value> {
        json!({
            "vm": "test",
            "lease_id": "lease",
            "image_kind": "linux",
            "ssh_user": "admin",
            "ip": "127.0.0.1",
            "ttl_expires_at": unix_now() + 60.0,
            "state": "running",
        })
        .as_object()
        .unwrap()
        .clone()
    }

    fn answer(ready: bool, error: Option<&str>, user: &str) -> Vec<u8> {
        let mut record = json!({
            "version": PROTOCOL_VERSION,
            "kind": "linux",
            "ready": ready,
            "error": error,
        });
        if ready {
            record["session"] = json!({"id": "7", "uid": 1000, "user": user});
        } else {
            record["session"] = Value::Null;
        }
        serde_json::to_vec(&record).unwrap()
    }

    fn prepare_with(
        manager: &Manager,
        record: &Map<String, Value>,
        answers: Vec<Vec<u8>>,
        timeout_s: u64,
    ) -> (Result<Map<String, Value>, ConsoleError>, usize) {
        let mut answers = answers.into_iter();
        let calls = Arc::new(Mutex::new(0usize));
        let counter = calls.clone();
        let deadline = now_instant() + Duration::from_secs(timeout_s);
        let result = manager.finish_prepare(
            record,
            "linux",
            deadline,
            |_command| Ok(vec!["/bin/true".to_string()]),
            move |_argv| {
                *counter.lock().unwrap() += 1;
                match answers.next() {
                    Some(bytes) => ProbeStep::Output(bytes),
                    None => ProbeStep::Unanswered,
                }
            },
        );
        let calls = *calls.lock().unwrap();
        (result, calls)
    }

    #[test]
    fn macos_preflight_does_not_claim_authentication_or_pixels() {
        let manager = Manager::new(Some(config("macos")));
        let capability = manager.capabilities();
        let macos = capability.get("macos").unwrap();
        assert_eq!(
            macos.get("server_enforced_view_only"),
            Some(&Value::Bool(false))
        );
        assert_eq!(
            macos.get("session_binding"),
            Some(&Value::String("viewer-selection-unverified".into()))
        );
        let record = json!({
            "vm": "fixture",
            "lease_id": "lease",
            "image_kind": "macos",
            "state": "running",
            "ttl_expires_at": unix_now() + 20.0,
        })
        .as_object()
        .unwrap()
        .clone();
        manager.reserve(&record).unwrap();
        {
            let mut inner = manager.lock_inner();
            inner.sessions.get_mut("fixture").unwrap().status = "ready".to_string();
        }
        let report = manager.resolve(&record, Some("lease")).unwrap();
        assert_eq!(
            report.get("server_enforced_view_only"),
            Some(&Value::Bool(false))
        );
        assert_eq!(
            report.get("authentication"),
            Some(&Value::String("unverified".into()))
        );
        assert_eq!(
            report.get("authentication_mechanism"),
            Some(&Value::String("human-guest-account-prompt".into()))
        );
        assert_eq!(
            report.get("pixels"),
            Some(&Value::String("unverified".into()))
        );
        assert_eq!(
            report.get("human_confirmation"),
            Some(&Value::String("unverified".into()))
        );
    }

    #[test]
    fn wrong_lease_environment_and_console_are_refused() {
        let manager = Manager::new(Some(config("linux")));
        let record = linux_record();
        manager.reserve(&record).unwrap();
        let mut wrong_lease = record.clone();
        wrong_lease.insert("lease_id".into(), Value::String("wrong".into()));
        assert!(manager
            .current_for_test(&wrong_lease, Some("wrong"), None)
            .is_err());
        let mut wrong_env = record.clone();
        wrong_env.insert(
            "environment_fingerprint".into(),
            Value::String("wrong".into()),
        );
        assert!(manager
            .current_for_test(&wrong_env, Some("lease"), None)
            .is_err());
        assert!(manager
            .current_for_test(&record, Some("lease"), Some("wrong"))
            .is_err());
    }

    #[test]
    fn cancel_before_open_never_replays_a_worker() {
        let manager = Manager::new(Some(config("linux")));
        let record = linux_record();
        manager.reserve(&record).unwrap();
        let console_id = {
            let mut inner = manager.lock_inner();
            let session = inner.sessions.get_mut("test").unwrap();
            session.status = "ready".to_string();
            session.guest = Some(
                json!({"session": {"id": "1", "uid": 1000, "user": "admin"}})
                    .as_object()
                    .unwrap()
                    .clone(),
            );
            session.ssh_argv = Some(vec!["/bin/true".to_string()]);
            session.console_id.clone()
        };
        let response = manager
            .cancel(&record, "lease", &console_id, "early")
            .unwrap();
        assert_eq!(
            response.get("attempt").unwrap().get("status"),
            Some(&Value::String("cancelled".into()))
        );
        let response = manager
            .open(&record, "lease", &console_id, "early")
            .unwrap();
        assert_eq!(
            response.get("attempt").unwrap().get("status"),
            Some(&Value::String("cancelled".into()))
        );
    }

    #[test]
    fn restart_never_reconstructs_console() {
        let manager = Manager::new(Some(config("linux")));
        let record = linux_record();
        manager.reserve(&record).unwrap();
        let other = Manager::new(Some(config("linux")));
        let error = other.resolve(&record, Some("lease")).unwrap_err();
        assert!(error.to_string().contains("restart"));
    }

    #[test]
    fn probe_false_is_not_ready() {
        let manager = Manager::new(Some(config("linux")));
        let record = linux_record();
        manager.reserve(&record).unwrap();
        let (result, calls) = prepare_with(
            &manager,
            &record,
            vec![answer(false, Some("NO_SESSION"), "admin")],
            180,
        );
        assert!(result.is_err());
        assert_eq!(calls, 1);
        let inner = manager.lock_inner();
        assert_eq!(inner.sessions.get("test").unwrap().status, "preparing");
    }

    #[test]
    fn pending_graphical_session_is_waited_for_not_failed() {
        let manager = Manager::new(Some(config("linux")));
        let record = linux_record();
        manager.reserve(&record).unwrap();
        let (result, calls) = prepare_with(
            &manager,
            &record,
            vec![
                answer(false, Some("active_console_session_required"), "admin"),
                answer(false, Some("active_console_session_required"), "admin"),
                answer(false, Some("x11_environment_unavailable"), "admin"),
                answer(true, None, "admin"),
            ],
            6,
        );
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(calls, 4);
        let inner = manager.lock_inner();
        assert_eq!(inner.sessions.get("test").unwrap().status, "ready");
    }

    #[test]
    fn pending_code_that_never_clears_names_itself_after_the_deadline() {
        let manager = Manager::new(Some(config("linux")));
        let record = linux_record();
        manager.reserve(&record).unwrap();
        let answers = (0..500)
            .map(|_| answer(false, Some("x11_environment_unavailable"), "admin"))
            .collect();
        let started = Instant::now();
        let (result, calls) = prepare_with(&manager, &record, answers, 1);
        let elapsed = started.elapsed();
        let error = result.unwrap_err();
        assert!(error.to_string().contains("x11_environment_unavailable"));
        assert!(calls > 1);
        assert!(elapsed <= Duration::from_secs(3));
        let inner = manager.lock_inner();
        assert_eq!(inner.sessions.get("test").unwrap().status, "preparing");
    }

    #[test]
    fn standing_fault_fails_immediately_and_never_waits() {
        for code in [
            "x11vnc_unavailable",
            "x11vnc_options_unavailable",
            "local_x11_display_required",
            "xauthority_invalid",
            "console_user_required",
            "guest_platform_mismatch",
        ] {
            let manager = Manager::new(Some(config("linux")));
            let record = linux_record();
            manager.reserve(&record).unwrap();
            let answers = (0..5).map(|_| answer(false, Some(code), "admin")).collect();
            let (result, calls) = prepare_with(&manager, &record, answers, 180);
            let error = result.unwrap_err();
            assert!(error.to_string().contains(code));
            assert_eq!(calls, 1);
        }
    }

    #[test]
    fn unrecognized_guest_code_is_never_quoted_into_a_message() {
        let manager = Manager::new(Some(config("linux")));
        let record = linux_record();
        manager.reserve(&record).unwrap();
        let answers = (0..3)
            .map(|_| answer(false, Some("cat /etc/shadow"), "admin"))
            .collect();
        let (result, calls) = prepare_with(&manager, &record, answers, 180);
        let error = result.unwrap_err();
        assert!(!error.to_string().contains("shadow"));
        assert!(error.to_string().contains("unrecognized_guest_diagnostic"));
        assert_eq!(calls, 1);
    }

    #[test]
    fn wrong_console_user_still_fails_without_waiting() {
        let manager = Manager::new(Some(config("linux")));
        let record = linux_record();
        manager.reserve(&record).unwrap();
        let answers = (0..3).map(|_| answer(true, None, "someone")).collect();
        let (result, calls) = prepare_with(&manager, &record, answers, 180);
        let error = result.unwrap_err();
        assert!(error.to_string().contains("lease user"));
        assert_eq!(calls, 1);
    }

    #[test]
    #[ignore = "test_real_worker_events_and_cancel: requires the native console-worker binary \
                and a real Unix socketpair, so it runs in live acceptance"]
    fn real_worker_events_and_cancel() {
        // Ported from test_stock_console.RealWorkerController. It needs the
        // native console-worker and a real Unix socketpair, neither of which is
        // available while console-worker is a placeholder.
        let manager = Manager::new(Some(config("linux")));
        let record = linux_record();
        manager.reserve(&record).unwrap();
        let console_id = {
            let mut inner = manager.lock_inner();
            let session = inner.sessions.get_mut("test").unwrap();
            session.status = "ready".to_string();
            session.guest = Some(
                json!({"session": {"id": "1", "uid": 1000, "user": "admin"}})
                    .as_object()
                    .unwrap()
                    .clone(),
            );
            session.console_id.clone()
        };
        manager
            .open(&record, "lease", &console_id, "attempt1")
            .unwrap();
        manager.shutdown();
    }

    impl Manager {
        fn current_for_test(
            &self,
            record: &Map<String, Value>,
            lease_id: Option<&str>,
            console_id: Option<&str>,
        ) -> Result<(), ConsoleError> {
            let mut inner = self.lock_inner();
            current(&mut inner, record, lease_id, console_id, true).map(|_| ())
        }
    }
}
