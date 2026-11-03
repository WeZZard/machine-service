//! The application rule of the leftover sweep (`docs/arch/hygiene.md`
//! HY-3; DC-73).
//!
//! An application opened through Launch Services, for example with
//! `open -a TextEdit`, is a child of launchd, so no process session of the
//! lease contains it. The sweep still ends it, but only when the lease
//! launched it, and never an application the owner opened:
//!
//! - it is a regular application, one with a Dock presence (`lsappinfo`);
//! - it runs as the service's user, started after the lease began, and was
//!   not running in the inventory before the lease;
//! - its launchd job, `application.<bundle>.<n>.<n>` in the user's GUI
//!   domain, was started with `MACHINE_SERVICE_LEASE_ID` set to this lease's
//!   ID. `open` hands the caller's environment to the job it creates, and
//!   every lease command carries the variable (DM-4), so the lease's own
//!   launches carry it and nothing else does. An application that was
//!   already running is only activated by `open` and keeps its own job;
//! - the sweep's exclusions do not protect it (the CI runner, the service,
//!   the login session).
//!
//! The job's environment is read with `launchctl print`, which shows every
//! variable of the job. Only the one variable is parsed out, in memory; the
//! output is never logged or stored, because the job inherits whatever the
//! caller's environment held. It is always the system `launchctl`
//! ([`crate::launchd::LAUNCHCTL`]), in a test build too: it only reads, and
//! the lease's applications are real launchd jobs (DC-242).
//!
//! A matched application is first asked to quit, as the Dock's Quit does
//! (`NSRunningApplication.terminate`, an Apple event), and is given the
//! stop grace; one still running then gets `SIGTERM` and `SIGKILL` like any
//! leftover. An application launched by another program, such as CuaDriver's
//! own `launch_app`, carries that program's environment and is not
//! attributed to the lease.

use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::process::{self, Arguments, Ended, Matched, ProcessInfo, Rule, Spared, SweepContext};

/// Time one `launchctl` or `osascript` call gets.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// The PID of each running job in `launchctl print gui/<uid>`'s `services`
/// block, with its label.
pub fn parse_services(text: &str) -> HashMap<i32, String> {
    let mut out = HashMap::new();
    let mut inside = false;
    for line in text.lines() {
        if line.trim_end() == "\tservices = {" {
            inside = true;
            continue;
        }
        if inside && line.trim_end() == "\t}" {
            break;
        }
        if !inside {
            continue;
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        if let [pid, _status, label] = words.as_slice() {
            if let Ok(pid) = pid.parse::<i32>() {
                out.insert(pid, label.to_string());
            }
        }
    }
    out
}

/// The lease ID in one job's `launchctl print` text, and nothing else.
pub fn parse_job_lease(text: &str) -> Option<String> {
    let prefix = format!("{} => ", process::LEASE_VARIABLE);
    text.lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix(&prefix))
        .map(|value| value.trim().to_string())
}

fn launchctl_print(target: &str) -> Option<String> {
    let mut command = Command::new(crate::launchd::LAUNCHCTL);
    command.args(["print", target]);
    let output = crate::proc::run_capture(&mut command, None, CALL_TIMEOUT).ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Reads the lease ID a running application's launchd job started with.
pub struct JobReader {
    uid: u32,
    labels: Option<HashMap<i32, String>>,
}

impl JobReader {
    /// A reader for the user `uid`'s GUI domain.
    pub fn new(uid: u32) -> Self {
        Self { uid, labels: None }
    }

    /// The lease ID of `pid`'s launchd job, when its job carries one.
    pub fn lease_of(&mut self, pid: i32) -> Option<String> {
        let uid = self.uid;
        let labels = self.labels.get_or_insert_with(|| {
            launchctl_print(&format!("gui/{uid}"))
                .map(|text| parse_services(&text))
                .unwrap_or_default()
        });
        let label = labels.get(&pid)?.clone();
        launchctl_print(&format!("gui/{uid}/{label}")).and_then(|text| parse_job_lease(&text))
    }
}

/// The applications the lease launched, and those an exclusion spares.
///
/// `running` is `lsappinfo`'s regular applications now, `before` the PIDs
/// of the regular applications in the inventory before the lease, and
/// `lease_of` answers the lease ID of a PID's launchd job.
pub fn leftovers(
    table: &[ProcessInfo],
    running: &[Value],
    before: &HashSet<i32>,
    context: &SweepContext,
    lease_of: &mut dyn FnMut(i32) -> Option<String>,
    arguments: &dyn Fn(i32) -> Option<Arguments>,
) -> (Vec<Matched>, Vec<(Matched, Spared)>) {
    let by_pid: HashMap<i32, &ProcessInfo> = table.iter().map(|info| (info.pid, info)).collect();
    let mut matched = Vec::new();
    let mut spared = Vec::new();
    let mut seen = HashSet::new();
    for app in running {
        let Some(pid) = app.get("pid").and_then(Value::as_i64).map(|pid| pid as i32) else {
            continue;
        };
        let Some(info) = by_pid.get(&pid) else {
            continue;
        };
        if !seen.insert(pid)
            || info.uid != context.uid
            || info.zombie
            || info.pid <= 1
            || before.contains(&pid)
            || info.started_at() + 1.0 < context.lease_started
            || context.lease_id.is_empty()
        {
            continue;
        }
        if lease_of(pid).as_deref() != Some(context.lease_id.as_str()) {
            continue;
        }
        let item = Matched {
            info: (*info).clone(),
            rule: Rule::Application,
        };
        match process::excluded(info, table, context, arguments) {
            Some(why) => spared.push((item, why)),
            None => matched.push(item),
        }
    }
    (matched, spared)
}

/// The PIDs of the regular applications an inventory recorded.
pub fn inventory_pids(inventory: Option<&Value>) -> HashSet<i32> {
    inventory
        .and_then(|inventory| inventory["parts"]["applications"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|app| app.get("pid").and_then(Value::as_i64))
        .map(|pid| pid as i32)
        .collect()
}

/// Ask one application to quit, as the Dock's Quit does. Answers whether
/// the request was delivered.
pub fn ask_to_quit(info: &ProcessInfo) -> bool {
    if info.pid <= 1 || !process::still_alive(info) {
        return false;
    }
    let script = format!(
        "ObjC.import('AppKit'); \
         var app = $.NSRunningApplication.runningApplicationWithProcessIdentifier({}); \
         (app.isNil() ? 'gone' : (app.terminate ? 'asked' : 'refused'))",
        info.pid
    );
    let mut command = Command::new("/usr/bin/osascript");
    command.args(["-l", "JavaScript", "-e", &script]);
    crate::proc::run_capture(&mut command, None, CALL_TIMEOUT).is_ok_and(|output| {
        output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "asked"
    })
}

/// End the lease's applications: ask each to quit, give them `grace`, then
/// `SIGTERM` and `SIGKILL` whatever still runs (HY-3).
pub fn end(apps: &[Matched], grace: Duration) -> Vec<Ended> {
    let asked: Vec<bool> = apps.iter().map(|app| ask_to_quit(&app.info)).collect();
    let deadline = Instant::now() + grace;
    while apps.iter().any(|app| process::still_alive(&app.info)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    let mut ended = Vec::new();
    let mut remaining = Vec::new();
    for (app, asked) in apps.iter().zip(asked) {
        if process::still_alive(&app.info) {
            remaining.push(app.clone());
        } else {
            ended.push(Ended {
                info: app.info.clone(),
                rule: Rule::Application,
                signal: asked.then_some("QUIT"),
                ended: true,
            });
        }
    }
    ended.extend(process::terminate(&remaining, grace));
    ended
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn info(pid: i32, uid: u32, start: u64, path: &str) -> ProcessInfo {
        ProcessInfo {
            pid,
            ppid: 1,
            pgid: pid,
            sid: Some(pid),
            uid,
            start: (start, 0),
            comm: path.rsplit('/').next().unwrap().to_string(),
            path: Some(path.to_string()),
            zombie: false,
        }
    }

    fn context() -> SweepContext {
        SweepContext {
            uid: 501,
            lease_id: "lease-a".to_string(),
            lease_started: 1000.0,
            sessions: Vec::new(),
            service_pid: 99,
            service_executable: None,
            ci_root: "/Users/u/actions-runner".to_string(),
        }
    }

    /// HY-3 (TL-1, DC-73): only an application the lease launched is a
    /// leftover. The owner's application, one running before the lease,
    /// one started before it, another user's, and one whose job carries
    /// another lease are never matched.
    #[test]
    fn hy3_application_rule_matches_only_the_lease_launches() {
        let table = vec![
            info(
                10,
                501,
                1100,
                "/System/Applications/TextEdit.app/Contents/MacOS/TextEdit",
            ),
            info(
                11,
                501,
                1100,
                "/Applications/Owner.app/Contents/MacOS/Owner",
            ),
            info(
                12,
                501,
                1100,
                "/Applications/Before.app/Contents/MacOS/Before",
            ),
            info(13, 501, 500, "/Applications/Old.app/Contents/MacOS/Old"),
            info(
                14,
                502,
                1100,
                "/Applications/Other.app/Contents/MacOS/Other",
            ),
            info(15, 501, 1100, "/Applications/Else.app/Contents/MacOS/Else"),
            info(
                16,
                501,
                1100,
                "/System/Library/CoreServices/Finder.app/Contents/MacOS/Finder",
            ),
        ];
        let running: Vec<Value> = (10..=16).map(|pid| json!({"pid": pid})).collect();
        let before: HashSet<i32> = [12].into_iter().collect();
        let mut lease_of = |pid: i32| match pid {
            10 | 12 | 13 | 14 | 16 => Some("lease-a".to_string()),
            15 => Some("lease-b".to_string()),
            _ => None,
        };
        let (matched, spared) = leftovers(
            &table,
            &running,
            &before,
            &context(),
            &mut lease_of,
            &|_| None,
        );
        let pids: Vec<i32> = matched.iter().map(|item| item.info.pid).collect();
        assert_eq!(pids, [10]);
        assert_eq!(matched[0].rule.as_str(), "application");
        assert_eq!(spared.len(), 1);
        assert_eq!(spared[0].0.info.pid, 16);
        assert_eq!(spared[0].1.as_str(), "login-session");
    }

    /// DC-73: the services block gives each running job's PID and label,
    /// and a job's text gives the lease ID and nothing else.
    #[test]
    fn dc73_launchctl_parsing() {
        let domain = "gui/501 = {\n\ttype = gui\n\tenvironment = {\n\t\tA => b\n\t}\n\tservices = {\n\t\t   19008      - \tapplication.com.apple.TextEdit.1.2\n\t\t       0      0 \tcom.apple.idle\n\t\t       -      0 \tcom.example.stopped\n\t}\n\tendpoints = {\n\t\t  1  2  x.y\n\t}\n}\n";
        let services = parse_services(domain);
        assert_eq!(services.len(), 2);
        assert_eq!(services[&19008], "application.com.apple.TextEdit.1.2");
        let job = "application.com.apple.TextEdit.1.2 = {\n\tenvironment = {\n\t\tPATH => /usr/bin\n\t\tMACHINE_SERVICE_LEASE_ID => 0123abcd\n\t}\n}\n";
        assert_eq!(parse_job_lease(job).as_deref(), Some("0123abcd"));
        assert_eq!(parse_job_lease("x = {\n\tenvironment = {\n\t}\n}\n"), None);
    }

    #[test]
    fn inventory_pids_read_the_applications_part() {
        let inventory = json!({"parts": {"applications": [{"pid": 7}, {"name": "x"}, {"pid": 9}]}});
        let pids = inventory_pids(Some(&inventory));
        assert_eq!(pids, [7, 9].into_iter().collect());
        assert!(inventory_pids(None).is_empty());
    }
}
