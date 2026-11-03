//! TL-2 component integration (`docs/arch/test-layers.md`): the real
//! `relay-agent` binary as a child process. `relay-agent stdio` serves one
//! link on its standard input and output; `relay-agent run` waits for its
//! configuration.

use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use relay_agent::driver::{Answer, Driver};
use relay_agent::protocol::{
    CancelFound, ExecutionState, Op, OpKind, Outcome, MAX_CHUNK, PROTOCOL,
};
use relay_agent::{Link, StreamLink};

const AGENT: &str = env!("CARGO_BIN_EXE_relay-agent");
const LONG: f64 = 60.0;

struct Process {
    child: service_layout::test_build::harness::Owned,
    driver: Driver,
}

fn start(home: &Path) -> Process {
    start_on(home, &service_layout::test_build::harness::Lifeline::new())
}

/// [`start`] on a lifeline the test keeps, so what a killed agent left
/// running outlives its handle until the test ends (DC-1xx-ISO-6).
fn start_on(home: &Path, lifeline: &service_layout::test_build::harness::Lifeline) -> Process {
    let mut child = service_layout::test_build::harness::command_on(AGENT, lifeline)
        .arg("stdio")
        .env("HOME", home)
        .env("SHELL", "/bin/sh")
        .current_dir(home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("start the relay agent");
    let link: StreamLink<ChildStdout, ChildStdin> = StreamLink::new(
        child.child().stdout.take().unwrap(),
        child.child().stdin.take().unwrap(),
    );
    let link: Arc<dyn Link> = Arc::new(link);
    let driver = Driver::open(link).expect("a hello");
    Process { child, driver }
}

impl Process {
    fn signal(&self, signal: i32) {
        // SAFETY: `kill` takes plain integers.
        unsafe {
            libc::kill(self.child.id() as i32, signal);
        }
    }

    fn wait(mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "the agent did not exit");
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.child.end();
    }
}

fn exec(command: &str) -> Op {
    Op::Exec {
        command: command.into(),
        stdin: false,
        timeout_s: LONG,
    }
}

fn outcome(answer: &Answer) -> &Outcome {
    &answer.result.as_ref().expect("a result").outcome
}

fn first_line(driver: &mut Driver, call: u64) -> String {
    loop {
        if let Some(answer) = driver.received(call) {
            let output = String::from_utf8_lossy(&answer.bytes).into_owned();
            if let Some((line, _)) = output.split_once('\n') {
                return line.to_string();
            }
        }
        driver.pump().unwrap();
    }
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn wait_gone(pid: i32) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while alive(pid) {
        assert!(Instant::now() < deadline, "process {pid} is still running");
        thread::sleep(Duration::from_millis(20));
    }
}

fn kill_group(group: i32) {
    // SAFETY: `killpg` takes plain integers.
    unsafe {
        libc::killpg(group, libc::SIGKILL);
    }
}

#[test]
fn the_hello_carries_the_sha256_of_the_executable() {
    let home = tempfile::tempdir().unwrap();
    let process = start(home.path());
    let hello = process.driver.hello();
    assert_eq!(hello.protocol, PROTOCOL);
    assert_eq!(
        hello.build,
        guest_actions::sha256_hex(&std::fs::read(AGENT).unwrap())
    );
    assert_eq!(hello.os, std::env::consts::OS);
    // Protocol 3: the hello names the executable's own signature, as
    // `codesign` shows it, on macOS only (DC-232).
    let expected = cfg!(target_os = "macos").then(|| {
        let signature = service_layout::signing::inspect(std::path::Path::new(AGENT)).unwrap();
        relay_agent::protocol::CodeSigning {
            kind: signature.kind().to_string(),
            team: signature.team().map(str::to_string),
        }
    });
    assert_eq!(hello.signing, expected);
}

#[test]
fn every_operation_runs_through_the_binary() {
    let home = tempfile::tempdir().unwrap();
    let mut process = start(home.path());
    let driver = &mut process.driver;

    let answer = driver
        .run("x1", exec("echo $0; echo err >&2; exit 2"), b"")
        .unwrap();
    assert_eq!(answer.bytes, b"sh\nerr\n");
    assert!(answer.end_matches());
    assert!(matches!(
        outcome(&answer),
        Outcome::Exit { rc: Some(2), .. }
    ));

    let answer = driver
        .run("s1", Op::Script { timeout_s: LONG }, b"echo script\n")
        .unwrap();
    assert_eq!(answer.bytes, b"script\n");

    let bytes: Vec<u8> = (0..3_300_000u32).map(|n| (n % 253) as u8).collect();
    let write = Op::Write {
        path: "~/payload.bin".into(),
        name: String::new(),
        mode: Some(0o600),
    };
    let written = driver.run("w1", write, &bytes).unwrap();
    assert!(
        matches!(outcome(&written), Outcome::Written(digest) if digest.size == bytes.len() as u64)
    );
    let read = driver
        .run(
            "r1",
            Op::Read {
                path: "payload.bin".into(),
            },
            b"",
        )
        .unwrap();
    assert_eq!(read.bytes, bytes);
    assert!(read.end_matches());
    assert!(read.chunks.iter().all(|size| *size <= MAX_CHUNK));

    let call = driver
        .start("c1", exec("sleep 60 & echo $!; wait"), b"")
        .unwrap();
    let pid: i32 = first_line(driver, call).parse().unwrap();
    let status = driver.run("st1", Op::Status, b"").unwrap();
    assert!(matches!(outcome(&status), Outcome::Status(status) if status.running == ["c1"]));
    let cancel = driver
        .run(
            "k1",
            Op::Cancel {
                target: "c1".into(),
            },
            b"",
        )
        .unwrap();
    assert!(matches!(
        outcome(&cancel),
        Outcome::Cancel {
            found: CancelFound::Signalled,
            ..
        }
    ));
    let cancelled = driver.finish(call).unwrap();
    assert!(matches!(
        outcome(&cancelled),
        Outcome::Exit {
            cancelled: true,
            ..
        }
    ));
    wait_gone(pid);

    assert!(matches!(
        driver.query("x1").unwrap(),
        ExecutionState::Completed {
            op: OpKind::Exec,
            ..
        }
    ));
    assert_eq!(driver.query("never").unwrap(), ExecutionState::Unknown);
}

#[test]
fn a_crash_keeps_every_record_and_runs_nothing_twice() {
    let home = tempfile::tempdir().unwrap();
    let mut process = start(home.path());
    let first = process
        .driver
        .run("once", exec("echo run >> runs; echo hi"), b"")
        .unwrap();
    let call = process
        .driver
        .start("crashed", exec("echo $$; sleep 60; echo run >> runs"), b"")
        .unwrap();
    let group: i32 = first_line(&mut process.driver, call).parse().unwrap();
    process.signal(libc::SIGKILL);
    process.wait();
    // A crash cannot kill the command's group; the restarted agent does
    // (DC-139).

    let mut process = start(home.path());
    wait_gone(group);
    let reports = &process.driver.hello().in_flight;
    assert!(reports.iter().any(|report| report.execution == "crashed"
        && matches!(
            report.state,
            ExecutionState::Completed {
                outcome: Outcome::Interrupted { .. },
                ..
            }
        )));
    let again = process
        .driver
        .run("once", exec("echo run >> runs; echo hi"), b"")
        .unwrap();
    assert!(again.result.as_ref().unwrap().replayed);
    assert_eq!(again.bytes, first.bytes);
    assert_eq!(outcome(&again), outcome(&first));
    let interrupted = process
        .driver
        .run("crashed", exec("echo run >> runs"), b"")
        .unwrap();
    assert!(matches!(outcome(&interrupted), Outcome::Interrupted { .. }));
    assert_eq!(
        std::fs::read_to_string(home.path().join("runs")).unwrap(),
        "run\n"
    );
}

/// A daemon that double-forks and leaves the command's process group and
/// session, writing its process ID to `daemon.pid`.
const DAEMON: &str = r#"perl -MPOSIX -e 'fork and exit; POSIX::setsid(); fork and exit; open STDIN, "</dev/null"; open STDOUT, ">/dev/null"; open STDERR, ">/dev/null"; open F, ">daemon.pid.tmp"; print F $$; close F; rename "daemon.pid.tmp", "daemon.pid"; exec "sleep", "300"'"#;

fn read_pid(path: &Path) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(pid) = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| text.trim().parse().ok())
        {
            return pid;
        }
        assert!(Instant::now() < deadline, "no {}", path.display());
        thread::sleep(Duration::from_millis(20));
    }
}

/// DC-139 (TL-2): the agent is killed with `SIGKILL` while a command runs,
/// so it cannot stop anything itself. The command's processes outlive it,
/// owned by the operating system; the restarted agent stops them, the
/// command's group and, where the command runs in a systemd scope, the
/// daemon that double-forked out of the group, and it leaves an unrelated
/// process alone.
#[test]
fn dc139_a_restarted_agent_stops_what_a_killed_agent_left_running() {
    let ownership = guest_actions::ownership::Ownership::detect();
    eprintln!("ownership: {}", ownership.describe());
    let home = tempfile::tempdir().unwrap();
    let mut unrelated = Command::new("sleep")
        .arg("300")
        .process_group(0)
        .spawn()
        .unwrap();
    let unrelated_pid = unrelated.id() as i32;
    let lifeline = service_layout::test_build::harness::Lifeline::new();
    let mut process = start_on(home.path(), &lifeline);
    let call = process
        .driver
        .start(
            "left-running",
            exec(&format!("echo $$; {DAEMON}; sleep 300 & wait")),
            b"",
        )
        .unwrap();
    let group: i32 = first_line(&mut process.driver, call).parse().unwrap();
    let daemon = read_pid(&home.path().join("daemon.pid"));
    // SAFETY: `getpgid` takes a plain integer.
    let daemon_group = unsafe { libc::getpgid(daemon) };
    assert_ne!(daemon_group, group, "the daemon left the command's group");
    process.signal(libc::SIGKILL);
    process.wait();
    thread::sleep(Duration::from_millis(200));
    assert!(alive(group), "the command outlives the killed agent");
    assert!(alive(daemon), "so does its daemon");

    let mut process = start_on(home.path(), &lifeline);
    wait_gone(group);
    eprintln!("the command's group {group} is gone after the restart");
    match ownership {
        guest_actions::ownership::Ownership::Scopes => {
            wait_gone(daemon);
            eprintln!("the double-forked daemon {daemon} is gone with its scope");
        }
        guest_actions::ownership::Ownership::Groups { .. } => {
            // A process that leaves its group escapes; the lease boundary
            // stops it (DC-139). The test stops it.
            assert!(alive(daemon), "a group cannot follow a daemon out of it");
            kill_group(daemon_group);
            wait_gone(daemon);
        }
    }
    assert!(alive(unrelated_pid), "an unrelated process is untouched");
    eprintln!("the unrelated process {unrelated_pid} still runs");
    match process.driver.query("left-running").unwrap() {
        ExecutionState::Completed {
            outcome: Outcome::Interrupted { message },
            ..
        } => {
            eprintln!("left-running: interrupted: {message}");
            assert!(message.contains("was stopped"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    let _ = unrelated.kill();
    let _ = unrelated.wait();
}

/// DC-139 (TL-2): a timeout kills the whole command; in a scope, that
/// includes a daemon that left the command's group.
#[test]
fn dc139_a_timeout_stops_the_command_and_its_scope() {
    let ownership = guest_actions::ownership::Ownership::detect();
    let home = tempfile::tempdir().unwrap();
    let mut process = start(home.path());
    let answer = process
        .driver
        .run(
            "timed",
            Op::Exec {
                command: format!("{DAEMON}; sleep 300"),
                stdin: false,
                timeout_s: 2.0,
            },
            b"",
        )
        .unwrap();
    assert!(matches!(
        outcome(&answer),
        Outcome::Exit {
            timed_out: true,
            ..
        }
    ));
    let daemon = read_pid(&home.path().join("daemon.pid"));
    if ownership == guest_actions::ownership::Ownership::Scopes {
        wait_gone(daemon);
        eprintln!("the timeout stopped the scope and its daemon {daemon}");
    } else {
        // SAFETY: `getpgid` takes a plain integer.
        kill_group(unsafe { libc::getpgid(daemon) });
        wait_gone(daemon);
    }
}

#[test]
fn a_stop_signal_kills_running_groups_and_records_them() {
    let home = tempfile::tempdir().unwrap();
    let mut process = start(home.path());
    let call = process
        .driver
        .start("long", exec("sleep 60 & echo $!; wait"), b"")
        .unwrap();
    let pid: i32 = first_line(&mut process.driver, call).parse().unwrap();
    process.signal(libc::SIGTERM);
    assert_eq!(process.wait().code(), Some(128 + libc::SIGTERM));
    wait_gone(pid);
    let mut process = start(home.path());
    assert!(matches!(
        process.driver.query("long").unwrap(),
        ExecutionState::Completed {
            outcome: Outcome::Interrupted { .. },
            ..
        }
    ));
}

#[test]
fn the_end_of_input_stops_the_agent_and_its_commands() {
    let home = tempfile::tempdir().unwrap();
    let mut process = start(home.path());
    let call = process
        .driver
        .start("long", exec("sleep 60 & echo $!; wait"), b"")
        .unwrap();
    let pid: i32 = first_line(&mut process.driver, call).parse().unwrap();
    process.driver.link().close();
    assert_eq!(process.wait().code(), Some(0));
    wait_gone(pid);
}

#[test]
fn commands_start_with_no_blocked_signals() {
    let home = tempfile::tempdir().unwrap();
    let mut process = start(home.path());
    let answer = process
        .driver
        .run("t1", exec("kill -TERM $$; sleep 5"), b"")
        .unwrap();
    assert!(matches!(
        outcome(&answer),
        Outcome::Exit {
            signal: Some(libc::SIGTERM),
            ..
        }
    ));
}

#[test]
fn run_waits_for_a_private_configuration() {
    let home = tempfile::tempdir().unwrap();
    let mut child = service_layout::test_build::harness::command(AGENT)
        .arg("run")
        .env("HOME", home.path())
        .env("SHELL", "/bin/sh")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (lines, logged) = mpsc::channel::<String>();
    let stderr = child.child().stderr.take().unwrap();
    thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { return };
            if lines.send(line).is_err() {
                return;
            }
        }
    });
    let expect = |needle: &str| {
        let line = logged
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|_| panic!("no log line with {needle:?}"));
        assert!(line.contains(needle), "{line:?} lacks {needle:?}");
    };
    expect("no configuration");
    let path = relay_agent::config::config_path(home.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        r#"{"broker_url": "wss://broker.example/relay", "agent_credential": "secret-token"}"#,
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    expect("must be 600");
    assert!(!relay_agent::agent::state_directory(home.path()).exists());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    expect("configuration loaded; broker wss://broker.example/relay");
    // It says how the operating system owns its commands (DC-139).
    expect("commands are owned by");
    // It dials now; `.example` never resolves (RFC 2606), so the dial fails
    // and is retried with backoff.
    expect("connect:");
    // SAFETY: `kill` takes plain integers.
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let status = child.wait().unwrap();
    assert_eq!(
        status.code(),
        Some(128 + libc::SIGTERM),
        "{status:?} {:?}",
        status.signal()
    );
    let rest: Vec<String> = logged.try_iter().collect();
    assert!(rest.iter().all(|line| !line.contains("secret-token")));
}

/// RA-6 (TL-2): a new build written over the agent's executable takes
/// effect at a restart, in the same process: the hello names the new build,
/// and the records written before the restart answer after it.
#[test]
fn a_restart_runs_the_build_written_over_the_executable() {
    let home = tempfile::tempdir().unwrap();
    let installed = home.path().join(".local/libexec/relay-agent/relay-agent");
    std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
    std::fs::copy(AGENT, &installed).unwrap();
    let mut child = service_layout::test_build::harness::command(&installed)
        .arg("stdio")
        .env("HOME", home.path())
        .env("SHELL", "/bin/sh")
        .current_dir(home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id();
    let link: Arc<dyn Link> = Arc::new(StreamLink::new(
        child.child().stdout.take().unwrap(),
        child.child().stdin.take().unwrap(),
    ));
    let mut driver = Driver::open(link).unwrap();
    let old = driver.hello().build.clone();
    let before = driver.run("before", exec("echo before"), b"").unwrap();

    let mut build = std::fs::read(AGENT).unwrap();
    build.extend_from_slice(b"another build");
    let new = guest_actions::sha256_hex(&build);
    assert_ne!(new, old);
    let written = driver
        .run(
            "write-build",
            Op::Write {
                path: ".local/libexec/relay-agent/relay-agent".into(),
                name: String::new(),
                mode: Some(0o755),
            },
            &build,
        )
        .unwrap();
    assert!(matches!(outcome(&written), Outcome::Written(_)));
    let restarted = driver.run("restart", Op::Restart, b"").unwrap();
    assert!(matches!(outcome(&restarted), Outcome::Status(status) if status.build == old));
    while driver.restarts == 0 {
        driver.pump().unwrap();
    }
    assert_eq!(driver.hello().build, new, "the new build runs");
    assert_eq!(child.try_wait().unwrap(), None, "in the same process");
    assert_eq!(child.id(), pid);
    let replay = driver.run("before", exec("echo again"), b"").unwrap();
    assert!(replay.result.as_ref().unwrap().replayed);
    assert_eq!(
        replay.bytes, before.bytes,
        "the records survive the restart"
    );
    let again = driver.run("restart", Op::Restart, b"").unwrap();
    assert!(
        again.result.as_ref().unwrap().replayed,
        "never restarts twice"
    );
    child.end();
}
