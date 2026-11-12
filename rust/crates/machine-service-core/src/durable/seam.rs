//! The process table and the running applications that a test build's
//! release sweep sees (DC-253; `docs/arch/hygiene.md` HY-3).
//!
//! A release reads the Mac's process table and `lsappinfo`, and signals what
//! its rules match. A test build never acts on the developer's login
//! session: its sweep sees only the processes it started itself
//! (`service_layout::test_build::signalable`), the lease's commands among
//! them, plus the rows of the test's fixture that [`TEST_VARIABLE`] names,
//! and the running applications that fixture lists. A signal to a fixture
//! row, and a request that an application quit, is written to the fixture's
//! signal record instead (`<fixture>.signals`, one `<pid> <signal>` per
//! line) and ends that row; a signal to any process the build did not start
//! is refused and recorded as `<pid> REFUSED`. The launchd job of a fixture
//! row is the fixture's `job_lease`; no `launchctl` runs.
//!
//! The fixture is JSON: `{"processes": [{"pid", "ppid", "pgid", "sid",
//! "uid", "start": [s, us], "comm", "path", "argv", "env", "job_lease"}],
//! "applications": [<lsappinfo rows>]}`, read anew at every look.

use std::collections::HashSet;
use std::path::PathBuf;

use serde_json::Value;

use super::process::{Arguments, ProcessInfo};

/// The variable naming a test build's process fixture.
pub const TEST_VARIABLE: &str = "MACHINE_SERVICE_TEST_PROCESSES";

/// Whether this build's sweep goes through the seam.
pub fn active() -> bool {
    service_layout::test_build::is_isolated()
}

fn fixture_path() -> Option<PathBuf> {
    service_layout::test_build::seam(TEST_VARIABLE)
}

fn fixture() -> Value {
    fixture_path()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(Value::Null)
}

fn signals_path() -> Option<PathBuf> {
    fixture_path().map(|path| path.with_extension("signals"))
}

/// The record of signals so far.
fn signals() -> Vec<(i32, String)> {
    signals_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let (pid, signal) = line.split_once(' ')?;
            Some((pid.parse().ok()?, signal.to_string()))
        })
        .collect()
}

/// Record that `pid` was sent `signal`.
fn record(pid: i32, signal: &str) {
    use std::io::Write;
    let Some(path) = signals_path() else {
        return;
    };
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{pid} {signal}");
    }
}

/// Whether a fixture row has been ended by a signal or a quit.
fn ended(pid: i32) -> bool {
    signals()
        .iter()
        .any(|(signalled, signal)| *signalled == pid && signal != "REFUSED")
}

fn rows() -> Vec<Value> {
    fixture()["processes"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn row(pid: i32) -> Option<Value> {
    rows()
        .into_iter()
        .find(|row| row["pid"].as_i64() == Some(i64::from(pid)))
}

fn info(row: &Value) -> Option<ProcessInfo> {
    let number = |key: &str| row[key].as_i64().and_then(|n| i32::try_from(n).ok());
    let pid = number("pid")?;
    Some(ProcessInfo {
        pid,
        ppid: number("ppid").unwrap_or(1),
        pgid: number("pgid").unwrap_or(pid),
        sid: number("sid"),
        uid: row["uid"].as_u64().and_then(|n| u32::try_from(n).ok())?,
        start: (
            row["start"][0].as_u64().unwrap_or_default(),
            row["start"][1].as_u64().unwrap_or_default(),
        ),
        comm: row["comm"].as_str().unwrap_or_default().to_string(),
        path: row["path"].as_str().map(str::to_string),
        zombie: false,
    })
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The fixture's living rows of user `uid`.
pub fn fixture_rows(uid: Option<u32>) -> Vec<ProcessInfo> {
    rows()
        .iter()
        .filter_map(info)
        .filter(|info| uid.is_none_or(|uid| info.uid == uid) && !ended(info.pid))
        .collect()
}

/// A fixture row's arguments and environment.
pub fn arguments(pid: i32) -> Option<Arguments> {
    let row = row(pid)?;
    Some(Arguments {
        exec_path: row["path"].as_str().unwrap_or_default().to_string(),
        argv: strings(&row["argv"]),
        env: strings(&row["env"]),
    })
}

/// Whether `pid` is a fixture row.
pub fn is_fixture(pid: i32) -> bool {
    row(pid).is_some()
}

/// Whether a fixture row still runs.
pub fn fixture_alive(pid: i32) -> bool {
    !ended(pid)
}

/// The lease ID of a fixture row's launchd job.
pub fn job_lease(pid: i32) -> Option<String> {
    row(pid)?["job_lease"].as_str().map(str::to_string)
}

/// The running applications the fixture lists.
pub fn applications() -> Value {
    Value::Array(
        fixture()["applications"]
            .as_array()
            .cloned()
            .unwrap_or_default(),
    )
}

/// Ask a fixture row's application to quit: recorded, and the row ends.
pub fn quit(pid: i32) -> bool {
    record(pid, "QUIT");
    true
}

/// Send `signal`, named `name`, to `pid` the way a test build may: a
/// fixture row's is recorded and ends it; a process the build started gets
/// it; any other is refused and recorded.
pub fn send(pid: i32, signal: i32, name: &str) -> bool {
    if is_fixture(pid) {
        record(pid, name);
        return true;
    }
    if signalable().contains(&pid) {
        // SAFETY: the caller re-validated the PID immediately before.
        return unsafe { libc::kill(pid, signal) == 0 };
    }
    record(pid, "REFUSED");
    false
}

/// The processes a test build may signal.
pub fn signalable() -> HashSet<i32> {
    service_layout::test_build::signalable().unwrap_or_default()
}
