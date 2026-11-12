//! Guest sessions at the daemon process boundary (test layer TL-2,
//! `docs/arch/test-layers.md`).
//!
//! The real `machine-service` binary runs against the fixture's fake `tart` and a
//! "local guest": `ssh` and `scp` shims that run the remote command, and copy
//! the remote path, on this machine under a temporary guest home. The
//! `ssh-session` link therefore uploads and starts the real
//! `guest-session-agent` binary, and both links run real commands, so their
//! answers can be compared. The acceptance criteria are in
//! `docs/acceptance/vm-service.acceptance`.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use common::{Daemon, LegacyFixture, Response};
use serde_json::{json, Value};

/// `ssh` that runs the remote command here, in `$GUEST_HOME`.
///
/// The remote command is every argument after the destination, joined by
/// spaces, as `ssh` sends it. The destination is `user@host`, or the argument
/// after `--` when the user is given with `-l`.
const LOCAL_SSH_SHIM: &str = r#"#!/bin/sh
set -u
root="${FAKE_ROOT:-/tmp}"
mkdir -p "$root"
printf '%s\n' "$*" >> "$root/ssh-calls.log"
command=""; found=""; separator=""
for arg in "$@"; do
  if [ -n "$found" ]; then command="${command:+$command }$arg"; continue; fi
  if [ -n "$separator" ]; then found=1; continue; fi
  case "$arg" in
    --) separator=1 ;;
    *@*) found=1 ;;
  esac
done
cd "$GUEST_HOME" || exit 255
# `sshd` names the shell by its file name: "sh", not "/bin/sh".
HOME="$GUEST_HOME" SHELL=/bin/sh exec perl -e 'exec { "/bin/sh" } "sh", "-c", $ARGV[0] or exit 255' "$command"
"#;

/// `scp` that copies here, taking a relative remote path from `$GUEST_HOME`.
const LOCAL_SCP_SHIM: &str = r#"#!/bin/sh
set -u
root="${FAKE_ROOT:-/tmp}"
mkdir -p "$root"
printf '%s\n' "$*" >> "$root/scp-calls.log"
src=""; dst=""
for arg in "$@"; do src="$dst"; dst="$arg"; done
local_path() {
  case "$1" in
    *@*:*)
      remote="${1#*:}"
      case "$remote" in
        /*) printf '%s' "$remote" ;;
        *) printf '%s/%s' "$GUEST_HOME" "$remote" ;;
      esac ;;
    *) printf '%s' "$1" ;;
  esac
}
exec cp -R "$(local_path "$src")" "$(local_path "$dst")"
"#;

/// A fixture whose guest is this machine, and its guest home.
struct LocalGuest {
    fixture: LegacyFixture,
    guest_home: PathBuf,
}

impl LocalGuest {
    fn new() -> Self {
        let fixture = LegacyFixture::new();
        common::write_exec(&fixture.bin, "ssh", LOCAL_SSH_SHIM);
        common::write_exec(&fixture.bin, "scp", LOCAL_SCP_SHIM);
        let guest_home = fixture.root.join("guest");
        std::fs::create_dir_all(&guest_home).expect("guest home");
        Self {
            fixture,
            guest_home,
        }
    }

    /// Start the daemon with `MACHINE_SERVICE_GUEST_LINK=link`.
    fn start(&self, link: &str) -> Daemon {
        self.fixture.start_daemon_with(&[
            ("MACHINE_SERVICE_GUEST_LINK".into(), link.into()),
            ("GUEST_HOME".into(), self.guest_home.display().to_string()),
            (
                "VM_GUEST_SESSION_AGENT".into(),
                common::guest_agent_bin().display().to_string(),
            ),
        ])
    }

    fn acquire(&self) -> String {
        let response = self.fixture.request(
            "POST",
            "/acquire",
            Some(&json!({"purpose": "guest-session", "image": "ubuntu2404", "env": "none"})),
        );
        assert_eq!(response.status, 200, "{}", response.body);
        response.body["vm"].as_str().expect("vm").to_string()
    }

    fn post(&self, vm: &str, route: &str, body: Value) -> Response {
        self.fixture
            .request("POST", &format!("/machines/{vm}/{route}"), Some(&body))
    }

    fn exec(&self, vm: &str, body: Value) -> Response {
        self.post(vm, "exec", body)
    }

    fn ssh_lines(&self) -> Vec<String> {
        self.fixture
            .ssh_calls()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn agent_pattern(&self) -> String {
        format!(
            "{}/.vm-service-session/agent serve",
            self.guest_home.display()
        )
    }
}

fn pgrep(pattern: &str) -> Vec<i32> {
    let output = Command::new("/usr/bin/pgrep")
        .args(["-f", pattern])
        .output()
        .expect("pgrep");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

fn wait_until(limit: Duration, what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn lines(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|text| text.lines().count())
        .unwrap_or(0)
}

// ------------------------------------------------------------------ AC-1

#[test]
fn a_lease_runs_every_operation_on_one_ssh_session() {
    let guest = LocalGuest::new();
    let _daemon = guest.start("ssh-session");
    let vm = guest.acquire();
    let after_acquire = guest.ssh_lines().len();

    for index in 0..10 {
        let response = guest.exec(&vm, json!({"argv": ["echo", format!("call {index}")]}));
        assert_eq!(response.status, 200, "{}", response.body);
        assert_eq!(response.body["output"], format!("call {index}\n"));
    }
    let local = guest.fixture.root.join("payload.bin");
    let bytes: Vec<u8> = (0..300_000u32).map(|n| (n % 253) as u8).collect();
    std::fs::write(&local, &bytes).expect("payload");
    let response = guest.post(
        &vm,
        "push",
        json!({"local_path": local, "remote_path": "payload.bin"}),
    );
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(response.body["bytes"], bytes.len());
    let back = guest.fixture.root.join("back.bin");
    let response = guest.post(
        &vm,
        "pull",
        json!({"local_path": back, "remote_path": "payload.bin"}),
    );
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(std::fs::read(&back).expect("pulled"), bytes);

    // A first open costs the start that finds no agent, the upload, and the
    // start that serves every operation.
    let session_calls = guest.ssh_lines()[after_acquire..].to_vec();
    assert_eq!(session_calls.len(), 3, "{session_calls:#?}");
    assert!(session_calls
        .iter()
        .all(|line| line.contains(".vm-service-session")));
    assert_eq!(pgrep(&guest.agent_pattern()).len(), 1, "one agent runs");
    let log = std::fs::read_to_string(guest.fixture.state_file().with_file_name("service.log"))
        .expect("service log");
    assert!(
        log.contains("uploading the guest session agent (")
            && log.contains("no agent in the guest (exit 127)"),
        "{log}"
    );
    let scp =
        std::fs::read_to_string(guest.fixture.tart_home.join("scp-calls.log")).unwrap_or_default();
    assert!(
        !scp.contains("payload.bin"),
        "the payload went by scp: {scp}"
    );
}

// ------------------------------------------------------------------ AC-2

/// One request and the parts of its answer both links must agree on.
fn answer(guest: &LocalGuest, vm: &str, route: &str, body: Value) -> Value {
    let response = guest.post(vm, route, body);
    let mut body = response.body.clone();
    if let Some(object) = body.as_object_mut() {
        // Only the session reports these.
        object.remove("bytes");
        object.remove("sha256");
        object.remove("vm");
        // Paths differ between the two fixtures.
        object.remove("pushed");
        object.remove("to");
        object.remove("pulled");
    }
    // Each fixture has its own root, which reaches the guest in `PATH`.
    let text = body
        .to_string()
        .replace(&guest.fixture.root.display().to_string(), "<root>");
    let body: Value = serde_json::from_str(&text).expect("answer json");
    json!({"status": response.status, "body": body})
}

fn scenario(guest: &LocalGuest, vm: &str) -> Vec<(String, Value)> {
    let root = guest.fixture.root.clone();
    std::fs::write(root.join("text.txt"), "héllo\nworld\n").expect("text");
    std::fs::create_dir_all(root.join("tree/sub")).expect("tree");
    std::fs::write(root.join("tree/sub/leaf"), "leaf").expect("leaf");
    let requests = [
        // The AC-2 examples.
        ("echo", "exec", json!({"argv": ["echo", "a b", "$HOME"]})),
        ("exit", "exec", json!({"argv": ["sh", "-c", "exit 7"]})),
        (
            "missing-command",
            "exec",
            json!({"argv": ["no-such-command"]}),
        ),
        (
            "script",
            "exec",
            json!({"script": "id -un; echo $PATH; echo err >&2"}),
        ),
        (
            "timeout",
            "exec",
            json!({"argv": ["sleep", "5"], "timeout": 1}),
        ),
        (
            "push-dir",
            "push",
            json!({"local_path": root.join("tree"), "remote_path": "tree"}),
        ),
        (
            "pushed-tree",
            "exec",
            json!({"argv": ["cat", "tree/sub/leaf"]}),
        ),
        (
            "push",
            "push",
            json!({"local_path": root.join("text.txt"), "remote_path": "text.txt"}),
        ),
        (
            "pull",
            "pull",
            json!({"local_path": root.join("pulled.txt"), "remote_path": "text.txt"}),
        ),
        // Quoting, encodings, streams, the working directory and refusals.
        (
            "quoting",
            "exec",
            json!({"argv": ["printf", "%s|", "a b", "c'd", "$HOME"]}),
        ),
        ("unicode", "exec", json!({"argv": ["echo", "日本語 ✓"]})),
        (
            "stderr",
            "exec",
            json!({"argv": ["sh", "-c", "echo only-err >&2"]}),
        ),
        (
            "mixed",
            "exec",
            json!({"argv": ["sh", "-c", "echo a; sleep 0.1; echo b >&2; sleep 0.1; echo c"]}),
        ),
        (
            // Above 32 KiB the HTTP answer is chunked; an exec keeps the last
            // 64000 characters on both links.
            "large-output",
            "exec",
            json!({"script": "head -c 100000 /dev/zero | tr '\\0' a; echo END\n"}),
        ),
        (
            "arithmetic",
            "exec",
            json!({"script": "set -e\nx=$((6*7))\necho \"$x\"\n"}),
        ),
        (
            "cwd",
            "exec",
            json!({"argv": ["sh", "-c", "pwd | sed 's|.*/||'"]}),
        ),
        (
            "pushed-content",
            "exec",
            json!({"argv": ["cat", "text.txt"]}),
        ),
        (
            "pull-missing",
            "pull",
            json!({"local_path": root.join("none.txt"), "remote_path": "no-such-file"}),
        ),
    ];
    let mut answers: Vec<(String, Value)> = requests
        .into_iter()
        .map(|(name, route, body)| (name.to_string(), answer(guest, vm, route, body)))
        .collect();
    answers.push((
        "pulled-content".into(),
        json!(std::fs::read_to_string(root.join("pulled.txt")).ok()),
    ));
    answers
}

#[test]
fn both_links_answer_the_same_requests_alike() {
    let mut results = Vec::new();
    for link in ["ssh", "ssh-session"] {
        let guest = LocalGuest::new();
        let _daemon = guest.start(link);
        let vm = guest.acquire();
        results.push(scenario(&guest, &vm));
    }
    let (ssh, session) = (&results[0], &results[1]);
    // The answers are real, not two identical failures.
    let by_name = |name: &str| &ssh.iter().find(|(key, _)| key == name).expect(name).1;
    let ok = |output: &str| json!({"status": 200, "body": {"rc": 0, "output": output}});
    // Each argv element is quoted, so `$HOME` reaches the guest unexpanded.
    assert_eq!(by_name("echo"), &ok("a b $HOME\n"));
    assert_eq!(by_name("exit")["body"]["rc"], 7);
    assert_eq!(by_name("missing-command")["body"]["rc"], 127);
    assert_eq!(
        by_name("missing-command")["body"]["output"],
        "sh: no-such-command: command not found\n"
    );
    let script = by_name("script")["body"]["output"]
        .as_str()
        .unwrap_or_default();
    assert!(
        script.ends_with("\nerr\n") && script.lines().count() == 3,
        "{script}"
    );
    assert_eq!(by_name("timeout")["body"]["error"], "ssh failed");
    assert_eq!(by_name("pushed-tree"), &ok("leaf"));
    assert_eq!(by_name("quoting"), &ok("a b|c'd|$HOME|"));
    assert_eq!(by_name("unicode"), &ok("日本語 ✓\n"));
    assert_eq!(by_name("stderr"), &ok("only-err\n"));
    assert_eq!(by_name("arithmetic"), &ok("42\n"));
    assert_eq!(
        by_name("large-output"),
        &ok(&format!("{}END\n", "a".repeat(64000 - 4)))
    );
    assert_eq!(by_name("cwd"), &ok("guest\n"));
    assert_eq!(by_name("pushed-content"), &ok("héllo\nworld\n"));
    assert_eq!(by_name("pulled-content"), &json!("héllo\nworld\n"));
    for ((name, by_ssh), (_, by_session)) in ssh.iter().zip(session) {
        if name == "mixed" {
            // The same lines; the session keeps the order they were written
            // in, where `ssh` gives all standard output first (GS-6).
            let sorted = |answer: &Value| {
                let mut lines: Vec<String> = answer["body"]["output"]
                    .as_str()
                    .unwrap_or_default()
                    .lines()
                    .map(str::to_string)
                    .collect();
                lines.sort();
                lines
            };
            assert_eq!(sorted(by_ssh), sorted(by_session));
            assert_eq!(by_session["body"]["output"], "a\nb\nc\n");
            assert_eq!(by_ssh["body"]["output"], "a\nc\nb\n");
            continue;
        }
        if name == "pull-missing" || name == "timeout" {
            // Both refuse with the same status; the messages differ (GS-6).
            assert_eq!(by_ssh["status"], 409);
            assert_eq!(by_session["status"], 409, "{by_session}");
            for field in ["rc", "output"] {
                assert_eq!(by_ssh["body"][field], by_session["body"][field], "{name}");
            }
            continue;
        }
        assert_eq!(by_ssh, by_session, "{name}");
    }
}

// ------------------------------------------------------------ AC-12, AC-13

#[test]
fn release_stops_the_running_command_and_the_agent() {
    let guest = LocalGuest::new();
    let daemon = guest.start("ssh-session");
    let vm = guest.acquire();
    let warm = guest.exec(&vm, json!({"argv": ["true"]}));
    assert_eq!(warm.status, 200, "{}", warm.body);

    let unique = format!("600.{}", std::process::id());
    let sleeper = format!("sleep {unique}");
    let port = daemon.port();
    let path = format!("/machines/{vm}/exec");
    let body = json!({"argv": ["sleep", unique]});
    let running = std::thread::spawn(move || {
        common::http_timeout(
            port,
            "POST",
            &path,
            Some(&body),
            None,
            Duration::from_secs(60),
        )
    });
    wait_until(Duration::from_secs(10), "the command to start", || {
        !pgrep(&sleeper).is_empty()
    });

    let started = Instant::now();
    let released =
        guest
            .fixture
            .request("POST", &format!("/machines/{vm}/release"), Some(&json!({})));
    assert_eq!(released.status, 200, "{}", released.body);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "release took {:?}",
        started.elapsed()
    );
    let outcome = running.join().expect("exec thread");
    assert_eq!(outcome.status, 409, "{}", outcome.body);
    assert_eq!(
        outcome.body["error"],
        format!("command cancelled by release of {vm}")
    );
    wait_until(Duration::from_secs(5), "the command to end", || {
        pgrep(&sleeper).is_empty()
    });
    wait_until(Duration::from_secs(5), "the agent to end", || {
        pgrep(&guest.agent_pattern()).is_empty()
    });
}

#[test]
fn release_of_an_idle_lease_ends_its_agent() {
    let guest = LocalGuest::new();
    let _daemon = guest.start("ssh-session");
    let vm = guest.acquire();
    assert_eq!(guest.exec(&vm, json!({"argv": ["true"]})).status, 200);
    assert_eq!(pgrep(&guest.agent_pattern()).len(), 1);
    let released =
        guest
            .fixture
            .request("POST", &format!("/machines/{vm}/release"), Some(&json!({})));
    assert_eq!(released.status, 200, "{}", released.body);
    wait_until(Duration::from_secs(5), "the agent to end", || {
        pgrep(&guest.agent_pattern()).is_empty()
    });
}

// ------------------------------------------------------------------ GS-5

#[test]
fn an_exec_whose_link_dies_after_it_started_is_reported_uncertain() {
    let guest = LocalGuest::new();
    let _daemon = guest.start("ssh-session");
    let vm = guest.acquire();
    assert_eq!(guest.exec(&vm, json!({"argv": ["true"]})).status, 200);
    let calls = guest.ssh_lines().len();

    let marker = guest.guest_home.join("marker");
    let unique = format!("300.{}", std::process::id());
    let command = format!("echo ran >> {}; sleep {unique}", marker.display());
    let port = guest.fixture.port();
    let path = format!("/machines/{vm}/exec");
    let body = json!({"argv": ["sh", "-c", command]});
    let running = std::thread::spawn(move || {
        common::http_timeout(
            port,
            "POST",
            &path,
            Some(&body),
            None,
            Duration::from_secs(60),
        )
    });
    wait_until(Duration::from_secs(10), "the command to start", || {
        lines(&marker) == 1
    });
    // The link dies as an `ssh` connection does: the agent is gone.
    for pid in pgrep(&guest.agent_pattern()) {
        // SAFETY: the pid is this test's agent, found by its unique path.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    let outcome = running.join().expect("exec thread");
    // The command's group outlives a killed agent; end it.
    for pid in pgrep(&format!("sleep {unique}")) {
        // SAFETY: the pid is this test's sleep, found by its unique argument.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    assert_eq!(outcome.status, 409, "{}", outcome.body);
    assert_eq!(outcome.body["outcome"], "uncertain", "{}", outcome.body);
    assert_eq!(lines(&marker), 1, "the command ran once");
    assert_eq!(guest.ssh_lines().len(), calls, "the command was not resent");

    // The next request opens a new session.
    let next = guest.exec(&vm, json!({"argv": ["echo", "again"]}));
    assert_eq!(next.status, 200, "{}", next.body);
    assert_eq!(guest.ssh_lines().len(), calls + 1);
}

// ------------------------------------------------------------------ AC-16

#[test]
fn an_unknown_guest_link_refuses_to_start() {
    let guest = LocalGuest::new();
    let port = common::free_port();
    let mut env = guest.fixture.env();
    env.push(("MACHINE_SERVICE_PORT".into(), port.to_string()));
    env.push(("MACHINE_SERVICE_GUEST_LINK".into(), "tart-session".into()));
    let error = match Daemon::try_start(&guest.fixture.root, port, &[], &env) {
        Ok(_) => panic!("the daemon started with an unknown guest link"),
        Err(error) => error,
    };
    assert!(error.contains("daemon exited early"), "{error}");
    assert!(
        error.contains(
            r#"MACHINE_SERVICE_GUEST_LINK must be one of ssh, ssh-session, relay; got "tart-session""#
        ),
        "{error}"
    );
}

#[test]
fn the_start_log_names_the_guest_link() {
    let guest = LocalGuest::new();
    let daemon = guest.start("ssh-session");
    let log =
        std::fs::read_to_string(guest.fixture.state_dir.join("service.log")).unwrap_or_default();
    assert!(
        log.contains("guest link: ssh-session")
            || daemon.stderr().contains("guest link: ssh-session"),
        "log: {log}\nstderr: {}",
        daemon.stderr()
    );
}
