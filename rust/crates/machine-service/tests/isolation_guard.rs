//! The guard of a test build (DC-251 to DC-256), proved on the
//! real daemon that `cargo test` built.
//!
//! Each test starts the daemon from a hostile environment, the one a
//! developer's shell would hand it: the account's own `HOME`, an inherited
//! `VM_ENVIRONMENT_FILE`, `MACHINE_SERVICE_GUEST_LINK=relay`,
//! `MACHINE_SERVICE_EXPOSE_DURABLE=on` and a real-looking `PATH` whose first
//! folder holds a recorder for every tool the service could run. Step by
//! step, until every value is the test's own, the daemon must refuse before
//! it binds or writes anything; with owned values it serves, binds only
//! loopback, runs no recorder, and leaves no process behind, whether its
//! handle ends it or its lifeline does.

mod common;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use common::{canonical_dir, free_port, harness, try_http, write_exec, Lifeline, DAEMON};
use serde_json::{json, Value};
use service_layout::test_build::{account_home, harness::living, INSTALLED_PORTS};

/// Every tool name a recorder stands for: the service's tools, the tools
/// its leases and inventories look for, and launchd's and DNS-SD's.
const RECORDED: [&str; 26] = [
    "tart",
    "softnet",
    "ssh",
    "scp",
    "ssh-keygen",
    "sh",
    "zsh",
    "pgrep",
    "osascript",
    "codesign",
    "tar",
    "plutil",
    "lsappinfo",
    "crontab",
    "ioreg",
    "sw_vers",
    "scutil",
    "launchctl",
    "brew",
    "npm",
    "dns-sd",
    "open",
    "claude",
    "codex",
    "node",
    "git",
];

struct Hostile {
    _dir: tempfile::TempDir,
    root: PathBuf,
    recorders: PathBuf,
}

impl Hostile {
    fn new() -> Hostile {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_dir(dir.path());
        let recorders = root.join("recorders");
        let log = root.join("recorded.log");
        for name in RECORDED {
            write_exec(
                &recorders,
                name,
                &format!(
                    "#!/bin/sh\nprintf '%s %s\\n' \"$0\" \"$*\" >> '{}'\nexit 1\n",
                    log.display()
                ),
            );
        }
        let environment = root.join("environment.json");
        std::fs::write(&environment, "{}").unwrap();
        Hostile {
            _dir: dir,
            root,
            recorders,
        }
    }

    /// What the recorders saw.
    fn recorded(&self) -> String {
        std::fs::read_to_string(self.root.join("recorded.log")).unwrap_or_default()
    }

    /// The environment of a developer's shell, recorders first on `PATH`.
    fn shell(&self) -> Vec<(String, String)> {
        let home = account_home().expect("the account's home");
        [
            ("HOME", home.display().to_string()),
            (
                "PATH",
                format!(
                    "{}:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
                    self.recorders.display()
                ),
            ),
            (
                "VM_ENVIRONMENT_FILE",
                self.root.join("environment.json").display().to_string(),
            ),
            ("MACHINE_SERVICE_GUEST_LINK", "relay".to_string()),
            ("MACHINE_SERVICE_EXPOSE_DURABLE", "on".to_string()),
            ("ZDOTDIR", home.display().to_string()),
            ("SHELL", "/bin/zsh".to_string()),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_string(), value))
        .collect()
    }

    /// Start the daemon with `environment` alone, as an isolated child.
    fn start(&self, environment: &[(String, String)], lifeline: &Lifeline) -> common::Owned {
        let err = std::fs::File::create(self.root.join("daemon.err")).unwrap();
        let mut command = harness::command_on(DAEMON, lifeline);
        command
            .envs(environment.iter().cloned())
            .stdout(Stdio::null())
            .stderr(err);
        command.spawn().unwrap()
    }

    fn stderr(&self) -> String {
        std::fs::read_to_string(self.root.join("daemon.err")).unwrap_or_default()
    }
}

/// One change to the environment that makes one more value the test's own.
type Fix = dyn Fn(&mut Vec<(String, String)>);

fn set(environment: &mut Vec<(String, String)>, name: &str, value: impl Into<String>) {
    environment.retain(|(existing, _)| existing != name);
    environment.push((name.to_string(), value.into()));
}

fn unset(environment: &mut Vec<(String, String)>, name: &str) {
    environment.retain(|(existing, _)| existing != name);
}

/// The TCP ports the processes `pids` listen on, as `lsof` names them.
fn listening(pids: &[i32]) -> Vec<String> {
    if pids.is_empty() {
        return Vec::new();
    }
    let list: Vec<String> = pids.iter().map(i32::to_string).collect();
    let output = std::process::Command::new("/usr/sbin/lsof")
        .args(["-nP", "-a", "-iTCP", "-sTCP:LISTEN", "-p", &list.join(",")])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .skip(1)
        .filter_map(|line| line.split_whitespace().nth(8).map(str::to_string))
        .collect()
}

/// Wait up to `limit` for `child` to exit, answering whether it did.
fn exits(child: &mut common::Owned, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if child.try_wait().is_ok_and(|status| status.is_some()) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}

/// From the developer's own environment, each installed default in turn is
/// refused before the daemon binds or writes anything, until every value is
/// the test's own; no recorder ever runs.
#[test]
fn a_hostile_environment_is_refused_step_by_step() {
    let hostile = Hostile::new();
    let owned = hostile.root.join("owned");
    let home = owned.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let mut environment = hostile.shell();
    let steps: Vec<(&str, Box<Fix>)> = vec![
        (
            "VM_ENVIRONMENT_FILE",
            Box::new(|env| unset(env, "VM_ENVIRONMENT_FILE")),
        ),
        (
            "port 6240",
            Box::new(|env| set(env, "MACHINE_SERVICE_PORT", free_port().to_string())),
        ),
        (
            "binds only loopback",
            Box::new(|env| set(env, "MACHINE_SERVICE_HOST", "127.0.0.1")),
        ),
        (
            "port 6241",
            Box::new(|env| set(env, "MACHINE_SERVICE_SESSION_PORT", free_port().to_string())),
        ),
        (
            ".local/state/machine-service",
            Box::new({
                let state = owned.join("state");
                move |env| set(env, "MACHINE_SERVICE_STATE", state.display().to_string())
            }),
        ),
        (
            ".tart",
            Box::new({
                let tart = owned.join("tart");
                move |env| set(env, "TART_HOME", tart.display().to_string())
            }),
        ),
        (
            ".config/vm-credentials",
            Box::new({
                let home = home.clone();
                move |env| set(env, "HOME", home.display().to_string())
            }),
        ),
        (
            "host-wide lease lock",
            Box::new({
                let lock = owned.join("lock");
                move |env| {
                    set(
                        env,
                        "MACHINE_SERVICE_DURABLE_LOCK_DIR",
                        lock.display().to_string(),
                    )
                }
            }),
        ),
        (
            "MACHINE_SERVICE_NAME_FILE",
            Box::new({
                let name = owned.join("name");
                move |env| set(env, "MACHINE_SERVICE_NAME_FILE", name.display().to_string())
            }),
        ),
    ];
    std::fs::write(owned.join("name"), "guard-test\n").unwrap();
    set(&mut environment, "MACHINE_SERVICE_HOST", "0.0.0.0");
    for (refusal, fix) in &steps {
        let lifeline = Lifeline::new();
        let mut daemon = hostile.start(&environment, &lifeline);
        assert!(
            exits(&mut daemon, Duration::from_secs(20)),
            "the daemon served despite {refusal}: {}",
            hostile.stderr()
        );
        let status = daemon.wait().unwrap();
        let stderr = hostile.stderr();
        assert!(!status.success(), "{refusal}: {stderr}");
        assert!(stderr.contains("test build"), "{refusal}: {stderr}");
        assert!(
            stderr.contains(refusal),
            "expected the refusal of {refusal}: {stderr}"
        );
        assert_eq!(hostile.recorded(), "", "{refusal}: a recorder ran");
        fix(&mut environment);
    }
}

/// With every value its own and the hostile rest kept, the daemon serves a
/// durable lease: it binds only loopback, its broker included, runs no
/// tool outside its table, and its handle leaves no process behind.
#[test]
fn an_owned_daemon_binds_loopback_runs_no_untabled_tool_and_leaves_nothing() {
    let hostile = Hostile::new();
    let owned = hostile.root.join("owned");
    let home = owned.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join(".zprofile"),
        "export PATH=/usr/bin:/bin:/usr/sbin:/sbin\n",
    )
    .unwrap();
    let tools = owned.join("tools");
    common::tool_table(&tools, None);
    std::fs::write(owned.join("name"), "guard-test\n").unwrap();
    let port = free_port();
    let mut environment = hostile.shell();
    unset(&mut environment, "VM_ENVIRONMENT_FILE");
    for (name, value) in [
        ("HOME", home.display().to_string()),
        ("USER", "guard".to_string()),
        ("MACHINE_SERVICE_PORT", port.to_string()),
        ("MACHINE_SERVICE_SESSION_PORT", free_port().to_string()),
        (
            "MACHINE_SERVICE_STATE",
            owned.join("state").display().to_string(),
        ),
        (
            "MACHINE_SERVICE_CONFIG_DIR",
            owned.join("config").display().to_string(),
        ),
        (
            "MACHINE_SERVICE_DURABLE_LOCK_DIR",
            owned.join("lock").display().to_string(),
        ),
        (
            "MACHINE_SERVICE_NAME_FILE",
            owned.join("name").display().to_string(),
        ),
        ("MACHINE_SERVICE_TEST_TOOLS", tools.display().to_string()),
        (
            "MACHINE_SERVICE_TEST_BONJOUR",
            owned.join("bonjour").display().to_string(),
        ),
        (
            "MACHINE_SERVICE_TEST_PROCESSES",
            owned.join("processes.json").display().to_string(),
        ),
        (
            "MACHINE_SERVICE_RELAY_AGENT_RELEASE",
            owned.join("no-release").display().to_string(),
        ),
        ("TART_HOME", owned.join("tart").display().to_string()),
        ("PILOT_REPO", owned.join("pilot").display().to_string()),
    ] {
        set(&mut environment, name, value);
    }
    let lifeline = Lifeline::new();
    let mut daemon = hostile.start(&environment, &lifeline);
    let deadline = Instant::now() + Duration::from_secs(30);
    while !try_http(port, "GET", "/health", None, None).is_ok_and(|r| r.status == 200) {
        assert!(
            !exits(&mut daemon, Duration::from_millis(50)) && Instant::now() < deadline,
            "the daemon did not serve: {}",
            hostile.stderr()
        );
    }
    let health = try_http(port, "GET", "/health", None, None).unwrap().body;
    let name = health["instance"].as_str().unwrap().to_string();
    assert_eq!(name, "guard-test");
    let acquired = common::http(
        port,
        "POST",
        "/acquire",
        Some(&json!({"machine": name, "purpose": "guard", "env": "none"})),
        None,
    );
    assert_eq!(
        acquired.status,
        200,
        "{}\n{}",
        acquired.body,
        hostile.stderr()
    );
    let lease = acquired.body["lease_id"].as_str().unwrap().to_string();
    let ran = common::http_timeout(
        port,
        "POST",
        &format!("/machines/{name}/exec"),
        Some(
            &json!({"lease_id": lease, "argv": ["/bin/sh", "-c", "/bin/sleep 300 >/dev/null 2>&1 & echo ok"]}),
        ),
        None,
        Duration::from_secs(120),
    );
    assert_eq!(ran.status, 200, "{}", ran.body);

    let related = daemon.related();
    let pids: Vec<i32> = related.iter().map(|row| row.pid).collect();
    let ports = listening(&pids);
    assert!(!ports.is_empty(), "nothing listens");
    for address in &ports {
        assert!(
            address.starts_with("127.0.0.1:") || address.starts_with("[::1]:"),
            "{address} is outside loopback: {ports:?}"
        );
        for installed in INSTALLED_PORTS {
            assert!(!address.ends_with(&format!(":{installed}")), "{address}");
        }
    }
    assert_eq!(hostile.recorded(), "", "a tool outside the table ran");
    assert!(
        related.len() >= 2,
        "the lease's command is the daemon's: {related:?}"
    );

    drop(daemon);
    drop(lifeline);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(living(&related), vec![], "a process outlived its handle");
    assert_eq!(hostile.recorded(), "", "a tool outside the table ran");
    assert!(!account_installed_touched(&owned));
}

/// Whether any owned path resolves into the account's installed folders,
/// which a test of this guard must never write.
fn account_installed_touched(owned: &Path) -> bool {
    let home = account_home().unwrap();
    service_layout::test_build::installed_folder(&home, owned).is_some()
}

/// When the test is gone, without its harness ending anything, the
/// daemon's own watch of its lifeline ends the daemon and every process the
/// lease started, its own session included (DC-256).
#[test]
fn a_severed_lifeline_ends_the_daemon_and_everything_it_started() {
    let dir = tempfile::tempdir().unwrap();
    let root = canonical_dir(dir.path());
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join(".zprofile"), "export PATH=/usr/bin:/bin\n").unwrap();
    let port = free_port();
    let lifeline = Lifeline::new();
    let err = std::fs::File::create(root.join("daemon.err")).unwrap();
    let mut command = common::daemon_command_on(&root, &lifeline);
    command
        .env("HOME", &home)
        .env("USER", "guard")
        .env("MACHINE_SERVICE_PORT", port.to_string())
        .env("MACHINE_SERVICE_STATE", root.join("state"))
        .env("MACHINE_SERVICE_EXPOSE_DURABLE", "on")
        .env("TART_HOME", root.join("tart"))
        .env("PILOT_REPO", root.join("pilot"))
        .stdout(Stdio::null())
        .stderr(err);
    let mut daemon = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !try_http(port, "GET", "/health", None, None).is_ok_and(|r| r.status == 200) {
        assert!(Instant::now() < deadline, "the daemon did not serve");
        std::thread::sleep(Duration::from_millis(50));
    }
    let name = try_http(port, "GET", "/health", None, None).unwrap().body["instance"]
        .as_str()
        .unwrap()
        .to_string();
    let acquired: Value = common::http(
        port,
        "POST",
        "/acquire",
        Some(&json!({"machine": name, "purpose": "lifeline", "env": "none"})),
        None,
    )
    .body;
    let lease = acquired["lease_id"].as_str().unwrap().to_string();
    // A command that leaves its session and its parent, as a daemonised
    // process does.
    let ran = common::http_timeout(
        port,
        "POST",
        &format!("/machines/{name}/exec"),
        Some(&json!({"lease_id": lease, "argv": ["/bin/sh", "-c",
            "/usr/bin/perl -MPOSIX -e 'POSIX::setsid(); exec q(/bin/sleep), q(300)' >/dev/null 2>&1 & echo $!"]})),
        None,
        Duration::from_secs(120),
    );
    assert_eq!(ran.status, 200, "{}", ran.body);
    let escaped: i32 = ran.body["output"].as_str().unwrap().trim().parse().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let related = daemon.related();
    assert!(related.iter().any(|row| row.pid == escaped), "{related:?}");

    lifeline.sever();
    assert!(
        exits(&mut daemon, Duration::from_secs(10)),
        "the daemon outlived its lifeline"
    );
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(living(&related), vec![], "a process outlived the lifeline");
}
