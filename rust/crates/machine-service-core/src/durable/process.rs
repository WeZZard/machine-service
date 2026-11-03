//! The process table, the lease's process sessions, and the leftover sweep
//! (`docs/arch/durable-machine.md` DM-4, DM-7; `docs/arch/hygiene.md` HY-3).
//!
//! The sweep may end only processes that the lease itself started. A process
//! is the lease's when it runs as the service's user, started after the
//! lease began, and either
//!
//! - belongs to a process session the lease started, or descends from a
//!   member of one (`session`, `descendant`), or
//! - carries `MACHINE_SERVICE_LEASE_ID` with this lease's ID in the
//!   environment it was started with (`environment`), or
//! - is a regular application the lease launched (`application`, see
//!   `apps.rs`).
//!
//! macOS 27 no longer lets a process read another process's start-up
//! environment (`KERN_PROCARGS2` answers the arguments only, and `ps eww`
//! shows no variables), so on such a system the environment rule finds
//! nothing and a process that left the lease's sessions after its parents
//! exited is not swept. The rule is kept for systems that still answer.
//!
//! Whatever those rules say, the sweep never ends the service itself or its
//! ancestors, the CI runner and everything that descends from it, or the
//! login session's own processes (HY-3). Every signal re-checks the target's
//! user and start time immediately before it is sent, so a recycled PID is
//! never signalled.
//!
//! A recorded session is trusted only while its leader's PID is gone or
//! still names the same process (same start time): once every member of a
//! session has exited, macOS may hand its ID to an unrelated session, which
//! must never be mistaken for the lease's.

use std::collections::{HashMap, HashSet};
use std::ffi::CStr;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// The environment variable that tags every lease command (DM-4).
pub const LEASE_VARIABLE: &str = "MACHINE_SERVICE_LEASE_ID";

/// The CI runner's executables (HY-5).
const CI_EXECUTABLES: [&str; 3] = ["Runner.Listener", "Runner.Worker", "Runner.PluginHost"];
/// Executables of the login session that the sweep never ends (HY-3).
const SESSION_PREFIXES: [&str; 1] = ["/System/Library/CoreServices/"];
/// The kernel's zombie status (`SZOMB`).
const ZOMBIE: u32 = 5;
/// How deep an ancestry walk goes before it stops.
const MAX_DEPTH: usize = 256;

/// One row of the process table.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessInfo {
    /// The process ID.
    pub pid: i32,
    /// The parent's process ID.
    pub ppid: i32,
    /// The process group ID.
    pub pgid: i32,
    /// The session ID, when the kernel answers.
    pub sid: Option<i32>,
    /// The effective user ID.
    pub uid: u32,
    /// The start time, `(seconds, microseconds)` since the epoch.
    pub start: (u64, u64),
    /// The short command name.
    pub comm: String,
    /// The executable path, when the kernel answers.
    pub path: Option<String>,
    /// Whether the process has exited and awaits its parent.
    pub zombie: bool,
}

impl ProcessInfo {
    /// The start time as Unix seconds.
    pub fn started_at(&self) -> f64 {
        self.start.0 as f64 + self.start.1 as f64 / 1_000_000.0
    }

    /// The executable path, or the command name.
    pub fn executable(&self) -> String {
        self.path.clone().unwrap_or_else(|| self.comm.clone())
    }

    /// Whether the process runs an executable called `name`, by its short
    /// command name or its path's file name. The kernel's command name of a
    /// copied platform binary keeps the original's name, so the path decides
    /// too.
    pub fn runs(&self, name: &str) -> bool {
        self.comm == name
            || self
                .path
                .as_deref()
                .and_then(|path| path.rsplit('/').next())
                .is_some_and(|file| file == name)
    }
}

/// A process's start-up arguments and environment (`KERN_PROCARGS2`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Arguments {
    /// The executable path the kernel recorded.
    pub exec_path: String,
    /// The argument vector.
    pub argv: Vec<String>,
    /// The environment the process started with, as `NAME=value`.
    pub env: Vec<String>,
}

/// Read one process's row, or `None` when it does not exist.
pub fn process_info(pid: i32) -> Option<ProcessInfo> {
    if pid <= 0 {
        return None;
    }
    // SAFETY: `proc_bsdinfo` is plain old data; zeroed is a valid value.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: the buffer is a properly sized, writable `proc_bsdinfo`.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    // SAFETY: `getsid` only reads kernel state for `pid`.
    let sid = unsafe { libc::getsid(pid) };
    // SAFETY: `pbi_comm` is NUL-terminated by the kernel within its bounds;
    // the zeroed buffer guarantees a terminator.
    let comm = unsafe { CStr::from_ptr(info.pbi_comm.as_ptr()) }
        .to_string_lossy()
        .to_string();
    Some(ProcessInfo {
        pid,
        ppid: info.pbi_ppid as i32,
        pgid: info.pbi_pgid as i32,
        sid: (sid >= 0).then_some(sid),
        uid: info.pbi_uid,
        start: (info.pbi_start_tvsec, info.pbi_start_tvusec),
        comm,
        path: process_path(pid),
        zombie: info.pbi_status == ZOMBIE,
    })
}

fn process_path(pid: i32) -> Option<String> {
    let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: the buffer is writable for the size passed.
    let length =
        unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    if length <= 0 {
        return None;
    }
    buffer.truncate(length as usize);
    Some(String::from_utf8_lossy(&buffer).to_string())
}

/// The process table the sweep sees, optionally limited to one user. In a
/// test build, only the processes it started and its fixture's rows
/// (`seam`, DC-1xx-ISO-3).
pub fn process_table(uid: Option<u32>) -> Vec<ProcessInfo> {
    if !super::seam::active() {
        return system_table(uid);
    }
    let started = super::seam::signalable();
    let mut rows: Vec<ProcessInfo> = system_table(uid)
        .into_iter()
        .filter(|info| started.contains(&info.pid))
        .collect();
    rows.extend(super::seam::fixture_rows(uid));
    rows
}

/// The Mac's whole process table, optionally limited to one user.
pub fn system_table(uid: Option<u32>) -> Vec<ProcessInfo> {
    let mut capacity = 4096usize;
    let pids: Vec<i32> = loop {
        let mut buffer = vec![0i32; capacity];
        let bytes = (buffer.len() * std::mem::size_of::<i32>()) as libc::c_int;
        // SAFETY: the buffer is writable for `bytes` bytes.
        let count = unsafe { libc::proc_listallpids(buffer.as_mut_ptr().cast(), bytes) };
        if count < 0 {
            return Vec::new();
        }
        let count = count as usize;
        if count < capacity {
            buffer.truncate(count);
            break buffer;
        }
        capacity *= 2;
    };
    pids.into_iter()
        .filter(|pid| *pid > 0)
        .filter_map(process_info)
        .filter(|info| uid.is_none_or(|uid| info.uid == uid))
        .collect()
}

/// Read a process's arguments and start-up environment; a test build's
/// fixture row's are the fixture's.
pub fn process_arguments(pid: i32) -> Option<Arguments> {
    if super::seam::active() && super::seam::is_fixture(pid) {
        return super::seam::arguments(pid);
    }
    system_arguments(pid)
}

fn system_arguments(pid: i32) -> Option<Arguments> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size: libc::size_t = 0;
    // SAFETY: a size query with a null buffer; `mib` is valid for its length.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || size == 0 {
        return None;
    }
    let mut buffer = vec![0u8; size];
    // SAFETY: the buffer is writable for `size` bytes.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            buffer.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    buffer.truncate(size);
    parse_procargs(&buffer)
}

/// Parse a `KERN_PROCARGS2` buffer: `argc`, the executable path, padding,
/// `argc` arguments, then the environment, each NUL-terminated.
pub fn parse_procargs(buffer: &[u8]) -> Option<Arguments> {
    if buffer.len() < 4 {
        return None;
    }
    let argc = i32::from_ne_bytes(buffer[..4].try_into().ok()?);
    let mut cursor = 4;
    let end_of = |from: usize| buffer[from..].iter().position(|byte| *byte == 0);
    let path_end = cursor + end_of(cursor)?;
    let exec_path = String::from_utf8_lossy(&buffer[cursor..path_end]).to_string();
    cursor = path_end;
    while cursor < buffer.len() && buffer[cursor] == 0 {
        cursor += 1;
    }
    let mut strings = Vec::new();
    while cursor < buffer.len() {
        let Some(length) = end_of(cursor) else {
            strings.push(String::from_utf8_lossy(&buffer[cursor..]).to_string());
            break;
        };
        let text = &buffer[cursor..cursor + length];
        cursor += length + 1;
        if text.is_empty() {
            // The environment ends at the first empty string after argv.
            if strings.len() >= argc.max(0) as usize {
                break;
            }
            continue;
        }
        strings.push(String::from_utf8_lossy(text).to_string());
    }
    let split = (argc.max(0) as usize).min(strings.len());
    let env = strings.split_off(split);
    Some(Arguments {
        exec_path,
        argv: strings,
        env,
    })
}

/// A process session the lease started (DM-4): its ID, which is its
/// leader's PID, and the leader's start time.
///
/// A command that the relay agent runs is not a session of its own: the
/// agent starts each command as a process group in the agent's session, and
/// a group leader cannot start a session. Such a command is recorded as its
/// process group, with `group` set, and its members are the processes of
/// that group (RA-2, DC-164).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseSession {
    /// The session ID, or the process group ID when `group` is set.
    pub sid: i32,
    /// The leader's start time.
    pub leader_start: (u64, u64),
    /// Whether this records a process group rather than a session.
    pub group: bool,
}

impl LeaseSession {
    /// The JSON form kept in the lease record.
    pub fn to_json(&self) -> Value {
        let mut value =
            json!({"sid": self.sid, "leader_start": [self.leader_start.0, self.leader_start.1]});
        if self.group {
            value["group"] = json!(true);
        }
        value
    }

    /// Read the JSON form back.
    pub fn from_json(value: &Value) -> Option<Self> {
        let sid = value.get("sid")?.as_i64()? as i32;
        let start = value.get("leader_start")?.as_array()?;
        Some(Self {
            sid,
            leader_start: (start.first()?.as_u64()?, start.get(1)?.as_u64()?),
            group: value.get("group").and_then(Value::as_bool).unwrap_or(false),
        })
    }

    /// Whether `info` is a member: of the session, or of the group.
    fn contains(&self, info: &ProcessInfo) -> bool {
        if self.group {
            info.pgid == self.sid
        } else {
            info.sid == Some(self.sid)
        }
    }
}

/// Which HY-3 rule matched a leftover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    /// It belongs to a process session the lease started.
    Session,
    /// It descends from a member of a lease session.
    Descendant,
    /// Its start-up environment carries this lease's ID.
    Environment,
    /// It is a regular application the lease launched (DC-73).
    Application,
}

impl Rule {
    /// The reported word.
    pub fn as_str(self) -> &'static str {
        match self {
            Rule::Session => "session",
            Rule::Descendant => "descendant",
            Rule::Environment => "environment",
            Rule::Application => "application",
        }
    }
}

/// Why a matched process was spared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Spared {
    /// It is the service or one of its ancestors.
    Service,
    /// It is the CI runner or descends from it (HY-5).
    CiRunner,
    /// It is a login-session or system process.
    LoginSession,
    /// It runs another instance of the service's executable.
    ServiceExecutable,
}

impl Spared {
    /// The reported word.
    pub fn as_str(self) -> &'static str {
        match self {
            Spared::Service => "machine-service",
            Spared::CiRunner => "ci-runner",
            Spared::LoginSession => "login-session",
            Spared::ServiceExecutable => "machine-service-executable",
        }
    }
}

/// Everything the sweep needs to know about the lease and the service.
#[derive(Debug, Clone)]
pub struct SweepContext {
    /// The service's user ID; no other user's process is ever matched.
    pub uid: u32,
    /// The lease's ID.
    pub lease_id: String,
    /// When the lease began, Unix seconds.
    pub lease_started: f64,
    /// The sessions the lease started.
    pub sessions: Vec<LeaseSession>,
    /// The service's own PID.
    pub service_pid: i32,
    /// The service's executable, which the sweep never ends.
    pub service_executable: Option<String>,
    /// The CI runner's folder (HY-5).
    pub ci_root: String,
}

/// A process the rules matched.
#[derive(Debug, Clone)]
pub struct Matched {
    /// The process.
    pub info: ProcessInfo,
    /// The matching rule.
    pub rule: Rule,
}

/// The sweep's verdict over one process table.
#[derive(Debug, Clone, Default)]
pub struct Verdict {
    /// The leftovers to end.
    pub matched: Vec<Matched>,
    /// The matched processes that an exclusion protects.
    pub spared: Vec<(Matched, Spared)>,
}

/// The sessions whose recorded leader is gone or still the same process.
fn trusted_sessions(
    by_pid: &HashMap<i32, &ProcessInfo>,
    sessions: &[LeaseSession],
) -> Vec<LeaseSession> {
    sessions
        .iter()
        .filter(|session| {
            by_pid
                .get(&session.sid)
                .is_none_or(|leader| leader.start == session.leader_start)
        })
        .copied()
        .collect()
}

/// Whether `info` belongs to a trusted session or group, and started no
/// earlier than its leader.
fn in_trusted(info: &ProcessInfo, trusted: &[LeaseSession]) -> bool {
    trusted
        .iter()
        .any(|session| session.contains(info) && info.start >= session.leader_start)
}

/// The members of the lease's trusted sessions: the processes a release
/// stops first (DM-7). Exclusions still apply.
pub fn session_members(
    table: &[ProcessInfo],
    context: &SweepContext,
    arguments: &dyn Fn(i32) -> Option<Arguments>,
) -> Verdict {
    let by_pid: HashMap<i32, &ProcessInfo> = table.iter().map(|info| (info.pid, info)).collect();
    let trusted = trusted_sessions(&by_pid, &context.sessions);
    let mut verdict = Verdict::default();
    for info in table {
        if info.uid != context.uid || info.zombie || !in_trusted(info, &trusted) {
            continue;
        }
        let matched = Matched {
            info: info.clone(),
            rule: Rule::Session,
        };
        place(&mut verdict, matched, &by_pid, context, arguments);
    }
    verdict
}

/// Classify the process table by the HY-3 rules and exclusions.
pub fn classify(
    table: &[ProcessInfo],
    context: &SweepContext,
    arguments: &dyn Fn(i32) -> Option<Arguments>,
) -> Verdict {
    let by_pid: HashMap<i32, &ProcessInfo> = table.iter().map(|info| (info.pid, info)).collect();
    let trusted = trusted_sessions(&by_pid, &context.sessions);
    let in_session =
        |info: &ProcessInfo| -> bool { info.uid == context.uid && in_trusted(info, &trusted) };
    let tag = format!("{LEASE_VARIABLE}={}", context.lease_id);
    let mut verdict = Verdict::default();
    for info in table {
        if info.uid != context.uid || info.zombie || info.pid <= 1 {
            continue;
        }
        if info.started_at() + 1.0 < context.lease_started {
            continue;
        }
        let rule = if in_session(info) {
            Some(Rule::Session)
        } else if ancestors(info, &by_pid).any(&in_session) {
            Some(Rule::Descendant)
        } else if !context.lease_id.is_empty()
            && arguments(info.pid).is_some_and(|args| args.env.iter().any(|entry| entry == &tag))
        {
            Some(Rule::Environment)
        } else {
            None
        };
        if let Some(rule) = rule {
            let matched = Matched {
                info: info.clone(),
                rule,
            };
            place(&mut verdict, matched, &by_pid, context, arguments);
        }
    }
    verdict
}

/// Put a matched process into `matched` or `spared`.
fn place(
    verdict: &mut Verdict,
    matched: Matched,
    by_pid: &HashMap<i32, &ProcessInfo>,
    context: &SweepContext,
    arguments: &dyn Fn(i32) -> Option<Arguments>,
) {
    match exclusion(&matched.info, by_pid, context, arguments) {
        Some(reason) => verdict.spared.push((matched, reason)),
        None => verdict.matched.push(matched),
    }
}

/// The ancestors of a process, nearest first, stopping at launchd.
fn ancestors<'a>(
    info: &ProcessInfo,
    by_pid: &'a HashMap<i32, &'a ProcessInfo>,
) -> impl Iterator<Item = &'a ProcessInfo> + 'a {
    let mut next = info.ppid;
    let mut seen = HashSet::new();
    std::iter::from_fn(move || {
        if next <= 1 || seen.len() >= MAX_DEPTH || !seen.insert(next) {
            return None;
        }
        let parent = by_pid.get(&next).copied()?;
        next = parent.ppid;
        Some(parent)
    })
}

/// The exclusion that protects `info` in `table`, if any (HY-3): the
/// application rule applies the same exclusions as the others.
pub fn excluded(
    info: &ProcessInfo,
    table: &[ProcessInfo],
    context: &SweepContext,
    arguments: &dyn Fn(i32) -> Option<Arguments>,
) -> Option<Spared> {
    let by_pid: HashMap<i32, &ProcessInfo> = table.iter().map(|info| (info.pid, info)).collect();
    exclusion(info, &by_pid, context, arguments)
}

/// The exclusion that protects a process, if any (HY-3).
fn exclusion(
    info: &ProcessInfo,
    by_pid: &HashMap<i32, &ProcessInfo>,
    context: &SweepContext,
    arguments: &dyn Fn(i32) -> Option<Arguments>,
) -> Option<Spared> {
    if info.pid <= 1 || info.pid == context.service_pid {
        return Some(Spared::Service);
    }
    // The service's ancestors.
    let mut walk = context.service_pid;
    let mut depth = 0;
    while walk > 1 && depth < MAX_DEPTH {
        if walk == info.pid {
            return Some(Spared::Service);
        }
        walk = by_pid.get(&walk).map(|parent| parent.ppid).unwrap_or(0);
        depth += 1;
    }
    if let (Some(service), Some(path)) = (&context.service_executable, &info.path) {
        if service == path {
            return Some(Spared::ServiceExecutable);
        }
    }
    // The walk stops at the service: whatever the service starts is the
    // lease's, even when the service itself runs under a CI job, as the
    // component tests do on a hosted runner.
    let is_ci = |candidate: &ProcessInfo| ci_process(candidate, &context.ci_root, arguments);
    if is_ci(info)
        || ancestors(info, by_pid)
            .take_while(|parent| parent.pid != context.service_pid)
            .any(is_ci)
    {
        return Some(Spared::CiRunner);
    }
    if info.path.as_deref().is_some_and(|path| {
        SESSION_PREFIXES
            .iter()
            .any(|prefix| path.starts_with(prefix))
    }) {
        return Some(Spared::LoginSession);
    }
    None
}

/// Whether one process is the CI runner's own: a runner executable, an
/// executable under the runner's folder, or a script there run by an
/// interpreter (HY-5).
fn ci_process(
    candidate: &ProcessInfo,
    ci_root: &str,
    arguments: &dyn Fn(i32) -> Option<Arguments>,
) -> bool {
    if CI_EXECUTABLES.iter().any(|name| candidate.runs(name)) {
        return true;
    }
    let ci_root = ci_root.trim_end_matches('/');
    if ci_root.is_empty() {
        return false;
    }
    let ci_prefix = format!("{ci_root}/");
    if candidate
        .path
        .as_deref()
        .is_some_and(|path| path.starts_with(&ci_prefix))
    {
        return true;
    }
    // A script runs under its interpreter, so its own path shows only in
    // the arguments: `/bin/sh ~/actions-runner/run.sh`.
    arguments(candidate.pid).is_some_and(|args| {
        args.argv
            .iter()
            .take(3)
            .any(|arg| arg.starts_with(&ci_prefix))
    })
}

/// The PIDs of the CI runner and every process that descends from it
/// (HY-4, HY-5).
pub fn ci_pids(
    table: &[ProcessInfo],
    ci_root: &Path,
    arguments: &dyn Fn(i32) -> Option<Arguments>,
) -> HashSet<i32> {
    let by_pid: HashMap<i32, &ProcessInfo> = table.iter().map(|info| (info.pid, info)).collect();
    let root = ci_root.to_string_lossy().to_string();
    let own: HashSet<i32> = table
        .iter()
        .filter(|info| ci_process(info, &root, arguments))
        .map(|info| info.pid)
        .collect();
    table
        .iter()
        .filter(|info| {
            own.contains(&info.pid)
                || ancestors(info, &by_pid).any(|parent| own.contains(&parent.pid))
        })
        .map(|info| info.pid)
        .collect()
}

/// The outcome of ending one process.
#[derive(Debug, Clone)]
pub struct Ended {
    /// The process as matched.
    pub info: ProcessInfo,
    /// The rule that matched it.
    pub rule: Rule,
    /// The last signal sent: `TERM`, `KILL`, or none when it had already
    /// gone.
    pub signal: Option<&'static str>,
    /// Whether the process is gone.
    pub ended: bool,
}

impl Ended {
    /// The hygiene record's entry (HY-3).
    pub fn to_json(&self) -> Value {
        json!({
            "pid": self.info.pid,
            "executable": self.info.executable(),
            "started_at": self.info.started_at(),
            "rule": self.rule.as_str(),
            "signal": self.signal,
            "ended": self.ended,
        })
    }
}

/// Whether `info` still names a living process: same PID, same start time,
/// same user, not a zombie.
pub fn still_alive(info: &ProcessInfo) -> bool {
    if super::seam::active() && super::seam::is_fixture(info.pid) {
        return super::seam::fixture_alive(info.pid);
    }
    process_info(info.pid)
        .is_some_and(|now| now.start == info.start && now.uid == info.uid && !now.zombie)
}

/// Send `signal` to `info` only if it is still the same process.
fn send(info: &ProcessInfo, signal: libc::c_int) -> bool {
    if info.pid <= 1 || !still_alive(info) {
        return false;
    }
    // A test build signals only what it started, or its fixture's rows
    // (DC-1xx-ISO-3).
    if super::seam::active() {
        let name = match signal {
            libc::SIGTERM => "TERM",
            libc::SIGKILL => "KILL",
            _ => "SIGNAL",
        };
        return super::seam::send(info.pid, signal, name);
    }
    // SAFETY: `kill` takes a PID that was re-validated immediately above.
    unsafe { libc::kill(info.pid, signal) == 0 }
}

/// End every matched process: `SIGTERM`, up to `grace` for them to exit,
/// then `SIGKILL` for the rest (DM-7, HY-3).
pub fn terminate(matched: &[Matched], grace: Duration) -> Vec<Ended> {
    let mut outcomes: Vec<Ended> = matched
        .iter()
        .map(|item| {
            let sent = send(&item.info, libc::SIGTERM);
            Ended {
                info: item.info.clone(),
                rule: item.rule,
                signal: sent.then_some("TERM"),
                ended: false,
            }
        })
        .collect();
    wait_gone(&mut outcomes, grace);
    for outcome in outcomes.iter_mut().filter(|outcome| !outcome.ended) {
        if send(&outcome.info, libc::SIGKILL) {
            outcome.signal = Some("KILL");
        }
    }
    wait_gone(&mut outcomes, Duration::from_secs(2));
    outcomes
}

/// `SIGKILL` every matched process at once, for a timeout or a cancelled
/// command (DM-4).
pub fn kill_now(matched: &[Matched]) {
    for item in matched {
        send(&item.info, libc::SIGKILL);
    }
}

fn wait_gone(outcomes: &mut [Ended], limit: Duration) {
    let deadline = Instant::now() + limit;
    loop {
        for outcome in outcomes.iter_mut().filter(|outcome| !outcome.ended) {
            outcome.ended = !still_alive(&outcome.info);
        }
        if outcomes.iter().all(|outcome| outcome.ended) || Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The service's own executable path.
pub fn service_executable() -> Option<String> {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.canonicalize().ok())
        .map(|path| path.to_string_lossy().to_string())
}

/// A [`SweepContext`] for this service process.
pub fn context(
    uid: u32,
    lease_id: &str,
    lease_started: f64,
    sessions: Vec<LeaseSession>,
    ci_root: &Path,
) -> SweepContext {
    SweepContext {
        uid,
        lease_id: lease_id.to_string(),
        lease_started,
        sessions,
        service_pid: std::process::id() as i32,
        service_executable: service_executable(),
        ci_root: ci_root.to_string_lossy().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UID: u32 = 501;

    fn row(pid: i32, ppid: i32, sid: i32, start: u64, comm: &str) -> ProcessInfo {
        ProcessInfo {
            pid,
            ppid,
            pgid: sid,
            sid: Some(sid),
            uid: UID,
            start: (start, 0),
            comm: comm.to_string(),
            path: Some(format!("/usr/bin/{comm}")),
            zombie: false,
        }
    }

    fn context(sessions: Vec<LeaseSession>) -> SweepContext {
        SweepContext {
            uid: UID,
            lease_id: "lease-1".to_string(),
            lease_started: 1000.0,
            sessions,
            service_pid: 50,
            service_executable: Some("/opt/ms/machine-service".to_string()),
            ci_root: "/Users/u/actions-runner".to_string(),
        }
    }

    fn no_arguments(_: i32) -> Option<Arguments> {
        None
    }

    fn pids(verdict: &Verdict) -> Vec<(i32, Rule)> {
        let mut out: Vec<_> = verdict
            .matched
            .iter()
            .map(|item| (item.info.pid, item.rule))
            .collect();
        out.sort_by_key(|item| item.0);
        out
    }

    /// A command the relay agent runs is recorded as its process group in
    /// the agent's session: the group and its descendants are the lease's,
    /// while the agent and the agent's other commands, in the same session,
    /// are not (RA-2, HY-3).
    #[test]
    fn a_relay_command_is_matched_by_its_process_group() {
        let group_row = |pid: i32, ppid: i32, pgid: i32, start: u64, comm: &str| ProcessInfo {
            pgid,
            ..row(pid, ppid, 100, start, comm)
        };
        let table = vec![
            group_row(100, 1, 100, 900, "relay-agent"),
            group_row(200, 100, 200, 1100, "zsh"),
            group_row(201, 200, 200, 1101, "sleep"),
            // Left the group with setpgid; still a descendant.
            group_row(202, 201, 202, 1102, "sleep"),
            // Another command of the agent, not the lease's.
            group_row(300, 100, 300, 1200, "sh"),
        ];
        let group = LeaseSession {
            sid: 200,
            leader_start: (1100, 0),
            group: true,
        };
        assert_eq!(
            LeaseSession::from_json(&group.to_json()),
            Some(group),
            "the record keeps its kind"
        );
        let verdict = classify(&table, &context(vec![group]), &no_arguments);
        assert_eq!(
            pids(&verdict),
            vec![
                (200, Rule::Session),
                (201, Rule::Session),
                (202, Rule::Descendant)
            ]
        );
        let members = session_members(&table, &context(vec![group]), &no_arguments);
        assert_eq!(
            pids(&members),
            vec![(200, Rule::Session), (201, Rule::Session)]
        );
        // Read as a session, the same ID would name nothing here, and the
        // agent's own session ID never names the lease.
        let as_session = LeaseSession {
            group: false,
            ..group
        };
        assert!(pids(&classify(&table, &context(vec![as_session]), &no_arguments)).is_empty());
    }

    /// AC-30 (TL-1): the rules match the lease's session, its descendants
    /// that left the session, and lease-tagged processes, and nothing else:
    /// not a process from before the lease, not another user's, not an
    /// unrelated session.
    #[test]
    fn ac30_rules_match_only_the_lease() {
        let session = LeaseSession {
            sid: 200,
            leader_start: (1100, 0),
            group: false,
        };
        let table = vec![
            row(1, 0, 1, 0, "launchd"),
            row(50, 1, 50, 900, "machine-service"),
            row(200, 50, 200, 1100, "zsh"),
            row(201, 200, 200, 1101, "sleep"),
            // Left the session with setsid, still a child of a member.
            row(300, 201, 300, 1102, "daemonish"),
            // The owner's own browser, started before the lease.
            row(400, 1, 400, 500, "Google Chrome"),
            // The owner's process started during the lease, unrelated.
            row(410, 1, 410, 1200, "Google Chrome"),
            // Tagged by environment, reparented to launchd.
            row(500, 1, 500, 1300, "escaped"),
            // Another user's process in a lease-tagged... it is skipped.
            ProcessInfo {
                uid: 0,
                ..row(600, 1, 200, 1300, "rootish")
            },
        ];
        let arguments = |pid: i32| -> Option<Arguments> {
            (pid == 500 || pid == 600 || pid == 410).then(|| Arguments {
                exec_path: String::new(),
                argv: vec![],
                env: vec![if pid == 410 {
                    "MACHINE_SERVICE_LEASE_ID=lease-2".to_string()
                } else {
                    "MACHINE_SERVICE_LEASE_ID=lease-1".to_string()
                }],
            })
        };
        let verdict = classify(&table, &context(vec![session]), &arguments);
        assert_eq!(
            pids(&verdict),
            vec![
                (200, Rule::Session),
                (201, Rule::Session),
                (300, Rule::Descendant),
                (500, Rule::Environment),
            ]
        );
        assert!(verdict.spared.is_empty());
    }

    /// AC-30 (TL-1): the CI runner and its descendants, the service, its
    /// ancestors, its executable and the login session are spared even when
    /// a rule matches them.
    #[test]
    fn ac30_exclusions_win_over_rules() {
        let session = LeaseSession {
            sid: 200,
            leader_start: (1100, 0),
            group: false,
        };
        let mut ci_listener = row(210, 200, 200, 1101, "Runner.Listener");
        ci_listener.path = Some("/Users/u/actions-runner/bin/Runner.Listener".to_string());
        let ci_job = row(211, 210, 200, 1102, "cargo");
        let ci_script = row(212, 200, 200, 1102, "sh");
        let ci_script_child = row(213, 212, 200, 1103, "sleep");
        let mut finder = row(220, 200, 200, 1101, "Finder");
        finder.path = Some("/System/Library/CoreServices/Finder.app/Contents/MacOS/Finder".into());
        let mut other_instance = row(230, 200, 200, 1101, "machine-service");
        other_instance.path = Some("/opt/ms/machine-service".to_string());
        let table = vec![
            row(40, 1, 40, 900, "launchd-child"),
            row(50, 40, 50, 900, "machine-service"),
            row(200, 50, 200, 1100, "zsh"),
            ci_listener,
            ci_job,
            ci_script,
            ci_script_child,
            finder,
            other_instance,
        ];
        let arguments = |pid: i32| -> Option<Arguments> {
            (pid == 212).then(|| Arguments {
                exec_path: "/bin/sh".to_string(),
                argv: vec![
                    "/bin/sh".to_string(),
                    "/Users/u/actions-runner/run.sh".to_string(),
                ],
                env: vec![],
            })
        };
        let mut sessions = context(vec![session]);
        // The service itself is in a recorded session: still spared.
        sessions.sessions.push(LeaseSession {
            sid: 50,
            leader_start: (900, 0),
            group: false,
        });
        sessions.lease_started = 0.0;
        let verdict = classify(&table, &sessions, &arguments);
        assert_eq!(pids(&verdict), vec![(200, Rule::Session)]);
        let mut spared: Vec<(i32, Spared)> = verdict
            .spared
            .iter()
            .map(|(item, reason)| (item.info.pid, *reason))
            .collect();
        spared.sort_by_key(|item| item.0);
        assert_eq!(
            spared,
            vec![
                (50, Spared::Service),
                (210, Spared::CiRunner),
                (211, Spared::CiRunner),
                (212, Spared::CiRunner),
                (213, Spared::CiRunner),
                (220, Spared::LoginSession),
                (230, Spared::ServiceExecutable),
            ]
        );
    }

    /// HY-3 (TL-1): a recorded session whose leader PID now names a
    /// different process was reused by someone else; none of its members
    /// match.
    #[test]
    fn hy3_reused_session_id_is_not_trusted() {
        let session = LeaseSession {
            sid: 200,
            leader_start: (1100, 0),
            group: false,
        };
        let table = vec![
            // PID 200 now belongs to a later, unrelated session leader.
            row(200, 1, 200, 5000, "unrelated"),
            row(201, 200, 200, 5001, "child"),
        ];
        let verdict = classify(&table, &context(vec![session]), &no_arguments);
        assert!(verdict.matched.is_empty(), "{:?}", verdict.matched);
        let members = session_members(&table, &context(vec![session]), &no_arguments);
        assert!(members.matched.is_empty());
    }

    /// DM-7 (TL-1): session members are the trusted sessions' processes,
    /// with the leader gone or unchanged.
    #[test]
    fn dm7_session_members() {
        let sessions = vec![
            LeaseSession {
                sid: 200,
                leader_start: (1100, 0),
                group: false,
            },
            LeaseSession {
                sid: 300,
                leader_start: (1200, 0),
                group: false,
            },
        ];
        let table = vec![
            row(201, 1, 200, 1101, "orphan-member"),
            row(300, 50, 300, 1200, "leader"),
            row(301, 300, 300, 1201, "member"),
            row(401, 1, 400, 1300, "unrelated"),
        ];
        let verdict = session_members(&table, &context(sessions), &no_arguments);
        assert_eq!(
            pids(&verdict),
            vec![
                (201, Rule::Session),
                (300, Rule::Session),
                (301, Rule::Session)
            ]
        );
    }

    #[test]
    fn procargs_parse() {
        let mut buffer = 2i32.to_ne_bytes().to_vec();
        buffer.extend_from_slice(
            b"/bin/sh\0\0\0\0/bin/sh\0-c\0A=1\0MACHINE_SERVICE_LEASE_ID=x\0\0\0",
        );
        let parsed = parse_procargs(&buffer).unwrap();
        assert_eq!(parsed.exec_path, "/bin/sh");
        assert_eq!(parsed.argv, vec!["/bin/sh", "-c"]);
        assert_eq!(parsed.env, vec!["A=1", "MACHINE_SERVICE_LEASE_ID=x"]);
    }

    /// The real table carries this test process with its arguments.
    #[test]
    fn real_table_has_this_process() {
        let pid = std::process::id() as i32;
        let me = process_info(pid).unwrap();
        // SAFETY: `geteuid` only reads process state.
        assert_eq!(me.uid, unsafe { libc::geteuid() });
        assert!(system_table(Some(me.uid))
            .iter()
            .any(|info| info.pid == pid));
        let args = process_arguments(pid).unwrap();
        assert!(!args.argv.is_empty());
        assert!(args
            .env
            .iter()
            .any(|entry| entry.starts_with("PATH=") || entry.starts_with("HOME=")));
    }
}
