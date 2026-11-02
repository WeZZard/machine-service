//! Who owns a command's processes (DC-139): the operating system, so the
//! ownership does not depend on the agent that started them staying alive.
//!
//! - On Linux with a systemd user manager, each command runs in a transient
//!   scope of its own, `relay-agent-<execution hash>-<unique>.scope`, a
//!   name the record keeps before the command starts: a cgroup that
//!   holds every process the command starts, a daemon that double-forks
//!   included, and outlives the agent. Stopping the scope by name stops all
//!   of them.
//! - Elsewhere (macOS, or Linux without a user manager), each command leads
//!   its own process group, and the agent records the group with the start
//!   time the operating system reports for its leader. A restarted agent
//!   stops a group only while its leader still has that start time, so a
//!   reused process ID never matches an unrelated process. A process that
//!   leaves its group escapes; the lease boundary stops it ([HY]).
//!
//! [HY]: ../../../../docs/arch/hygiene.md

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// The prefix of every scope the agent makes.
pub const SCOPE_PREFIX: &str = "relay-agent-";

/// How this agent makes the operating system own its commands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ownership {
    /// Each command in a transient systemd user scope.
    Scopes,
    /// Each command's process group, recorded with its start time; `reason`
    /// says why scopes are not used.
    Groups {
        /// Why this agent does not use scopes.
        reason: String,
    },
}

impl Ownership {
    /// The ownership this machine supports. On Linux it starts one
    /// throw-away scope to prove that the user manager takes them.
    pub fn detect() -> Ownership {
        if !cfg!(target_os = "linux") {
            return Ownership::Groups {
                reason: format!("{} has no cgroups", std::env::consts::OS),
            };
        }
        let unit = unique_unit("probe");
        let probe = Command::new("systemd-run")
            .args(["--user", "--scope", "--quiet", "--collect", "--unit"])
            .arg(&unit)
            .arg("--")
            .arg("true")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output();
        match probe {
            Ok(output) if output.status.success() => Ownership::Scopes,
            Ok(output) => Ownership::Groups {
                reason: format!(
                    "no systemd user manager takes scopes: systemd-run {}: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            },
            Err(error) => Ownership::Groups {
                reason: format!("systemd-run cannot run: {error}"),
            },
        }
    }

    /// A short description for logs and the status answer: `scope`, or
    /// `process-group (<reason>)`.
    pub fn describe(&self) -> String {
        match self {
            Ownership::Scopes => "scope".into(),
            Ownership::Groups { reason } => format!("process-group ({reason})"),
        }
    }

    /// The scope a command runs in under this ownership, named after `key`
    /// (the hash of its execution ID) and made unique on this machine, so a
    /// scope that a daemon keeps alive never blocks a later command.
    pub fn scope_for(&self, key: &str) -> Option<String> {
        match self {
            Ownership::Scopes => Some(format!("{}.scope", unique_unit(key))),
            Ownership::Groups { .. } => None,
        }
    }
}

/// `relay-agent-<key>-<unix ms>-<pid>-<n>`: unique among this machine's
/// agents, whatever their execution IDs.
fn unique_unit(key: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or_default();
    format!(
        "{SCOPE_PREFIX}{key}-{millis}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// The record of who owns one command's processes, kept with its
/// execution so that a restarted agent can stop what it left behind.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Owner {
    /// A transient systemd user scope, stopped by name.
    Scope {
        /// The unit, `relay-agent-<hash>.scope`.
        unit: String,
    },
    /// A process group and the start time of its leader.
    Group {
        /// The process group, which is its leader's process ID.
        group: i32,
        /// The leader's start time, as [`process_start`] reports it.
        started: String,
    },
}

/// What stopping a leftover owner found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stopped {
    /// Its processes were killed (a scope is killed whether or not it still
    /// had any).
    Killed,
    /// The group's leader is gone; nothing was signalled.
    Gone,
    /// The process ID now belongs to a process with another start time, an
    /// unrelated process; nothing was signalled.
    Reused,
    /// Stopping failed.
    Failed(String),
}

impl Owner {
    /// Stop the processes this owner holds, as a restarted agent does for
    /// an execution it left running.
    pub fn stop_leftover(&self) -> Stopped {
        match self {
            Owner::Scope { unit } => match kill_scope(unit) {
                Ok(()) => Stopped::Killed,
                Err(error) => Stopped::Failed(error),
            },
            Owner::Group { group, started } => stop_group(*group, started),
        }
    }

    /// A short description for messages.
    pub fn describe(&self) -> String {
        match self {
            Owner::Scope { unit } => format!("scope {unit}"),
            Owner::Group { group, .. } => format!("process group {group}"),
        }
    }
}

/// Stop the process group `group` only while its leader still has the
/// start time `started`.
pub fn stop_group(group: i32, started: &str) -> Stopped {
    if group <= 0 {
        return Stopped::Gone;
    }
    match process_start(group) {
        None => Stopped::Gone,
        Some(now) if now == started => {
            crate::kill_group(group);
            Stopped::Killed
        }
        Some(_) => Stopped::Reused,
    }
}

/// Kill every process of a scope of this user's systemd manager.
pub fn kill_scope(unit: &str) -> Result<(), String> {
    let output = Command::new("systemctl")
        .args(["--user", "kill", "--kill-whom=all", "--signal=SIGKILL"])
        .arg(unit)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| format!("systemctl cannot run: {error}"))?;
    let text = String::from_utf8_lossy(&output.stderr);
    // A scope whose processes all ended is gone already, which is the goal.
    if output.status.success() || text.contains("not loaded") || text.contains("not found") {
        Ok(())
    } else {
        Err(format!("systemctl kill {unit}: {}", text.trim()))
    }
}

/// The start time the operating system reports for process `pid`, or
/// `None` when no such process exists. Two values are equal only for the
/// same process: on macOS it is the start in microseconds since the epoch,
/// on Linux the boot ID and the start in clock ticks since that boot.
pub fn process_start(pid: i32) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    platform_start(pid)
}

#[cfg(target_os = "macos")]
fn platform_start(pid: i32) -> Option<String> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    // SAFETY: `proc_pidinfo` writes at most `size` bytes into `info`.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    (written == size).then(|| format!("{}.{:06}", info.pbi_start_tvsec, info.pbi_start_tvusec))
}

#[cfg(target_os = "linux")]
fn platform_start(pid: i32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name may hold spaces and parentheses; the fields after
    // the last `)` start with the state, field 3, and the start time is
    // field 22.
    let rest = &stat[stat.rfind(')')? + 1..];
    let ticks = rest.split_whitespace().nth(19)?;
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap_or_default();
    Some(format!("{}:{ticks}", boot.trim()))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn platform_start(_pid: i32) -> Option<String> {
    None
}

/// The program and arguments that put a shell into `scope`: `systemd-run
/// --user --scope`, which registers itself as the scope and then executes
/// the shell in place, so the shell keeps its process ID, its process
/// group, its standard streams and its environment.
pub(crate) fn scope_command(scope: &str, shell: &Path) -> Command {
    let mut command = Command::new("systemd-run");
    let unit = scope.strip_suffix(".scope").unwrap_or(scope);
    command
        .args(["--user", "--scope", "--quiet", "--collect", "--unit"])
        .arg(unit)
        .arg("--")
        .arg(shell_argument(shell));
    command
}

/// The shell as `systemd-run` should name it: its bare file name when the
/// search path finds that same file, so the shell's `$0` reads `bash` as
/// it does under `sshd`; its path otherwise.
fn shell_argument(shell: &Path) -> PathBuf {
    let Some(name) = shell.file_name() else {
        return shell.to_path_buf();
    };
    let wanted = std::fs::canonicalize(shell).ok();
    let found = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file())
    });
    match (
        wanted,
        found.and_then(|path| std::fs::canonicalize(path).ok()),
    ) {
        (Some(wanted), Some(found)) if wanted == found => PathBuf::from(name),
        _ => shell.to_path_buf(),
    }
}
