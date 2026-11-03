//! TL-2: the durable launcher, `machine-service --durable-launch`, as the
//! relay agent runs it: in a process group of its own, with standard output
//! and standard error on one pipe (DM-4, DC-220, DC-221).

mod common;

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Command, Stdio};

use common::DAEMON;
use machine_service_core::durable::agents::Secret;
use machine_service_core::durable::exec::Program;
use machine_service_core::durable::launch::{self, Launch};
use machine_service_core::durable::session_env::SessionEnv;
use serde_json::Value;

const KEY: &str = "sk-fake-launch-key-0123456789abcdef";

fn session(keys: bool) -> SessionEnv {
    SessionEnv {
        vars: BTreeMap::from([
            ("PATH".to_string(), "/usr/bin:/bin".to_string()),
            ("ANTHROPIC_API_KEY".to_string(), KEY.to_string()),
        ]),
        secrets: if keys {
            vec![Secret::new(KEY)]
        } else {
            Vec::new()
        },
    }
}

fn line(text: &str) -> Program {
    Program::Line {
        shell: "/bin/sh".into(),
        line: text.into(),
    }
}

/// Run the launcher on `execution`'s request as the relay agent would:
/// `$SHELL -c <line>` in a new process group, output and errors combined.
fn agent_runs(command: &str, input: &[u8]) -> (std::process::ExitStatus, Vec<u8>, i32) {
    let (mut output, writer) = std::io::pipe().unwrap();
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(writer.try_clone().unwrap())
        .stderr(writer)
        .spawn()
        .unwrap();
    let group = child.id() as i32;
    let mut stdin = child.stdin.take().unwrap();
    let input = input.to_vec();
    let feeding = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut output, &mut bytes).unwrap();
    let status = child.wait().unwrap();
    feeding.join().unwrap();
    (status, bytes, group)
}

fn prepared(
    lease: &Path,
    execution: &str,
    launch: Launch<'_>,
    keys: bool,
) -> (launch::Files, String) {
    let files = launch::files(lease, execution);
    let command =
        launch::prepare(&files, Path::new(DAEMON), launch, &session(keys), lease).unwrap();
    assert!(!command.contains(KEY), "{command}");
    (files, command)
}

/// DC-221: with loaded keys the launcher runs the shell as its
/// child and passes the output on with every key redacted, also a key that
/// the command writes in pieces; it ends with the command's exit status,
/// or with its signal. Without keys it becomes the shell itself.
#[test]
fn the_launcher_redacts_keys_and_ends_as_the_command_ended() {
    let lease = tempfile::tempdir().unwrap();
    let lease = lease.path().canonicalize().unwrap();
    let (half, rest) = KEY.split_at(12);
    let program = line(&format!(
        "printf '%s' '{half}'; sleep 0.3; printf '%s\\n' '{rest}'; echo \"env=$ANTHROPIC_API_KEY\" >&2; pwd; exit 5"
    ));
    let (files, command) = prepared(&lease, "split", Launch::Program(&program), true);
    let (status, bytes, group) = agent_runs(&command, b"");
    let text = String::from_utf8(bytes).unwrap();
    assert_eq!(status.code(), Some(5), "{text}");
    assert_eq!(
        text,
        format!("[redacted]\nenv=[redacted]\n{}\n", lease.display())
    );
    assert!(!files.request.exists(), "the launcher claimed its request");
    let recorded: Value = serde_json::from_slice(&std::fs::read(&files.group).unwrap()).unwrap();
    assert_eq!(recorded["sid"], group, "{recorded}");
    assert_eq!(recorded["group"], true);

    // A signal that ends the command ends the launcher the same way.
    let program = line("kill -TERM $$");
    let (_, command) = prepared(&lease, "signal", Launch::Program(&program), true);
    let (status, _, _) = agent_runs(&command, b"");
    assert_eq!(status.signal(), Some(libc::SIGTERM));

    // A client's script arrives on standard input.
    let (_, command) = prepared(&lease, "script", Launch::ScriptOnInput, true);
    let (status, bytes, _) = agent_runs(&command, b"echo \"script $ANTHROPIC_API_KEY\"\nexit 3\n");
    assert_eq!(status.code(), Some(3));
    assert_eq!(String::from_utf8(bytes).unwrap(), "script [redacted]\n");

    // Without keys to redact the launcher is the shell itself.
    let program = line("echo $$; exit 4");
    let (_, command) = prepared(&lease, "plain", Launch::Program(&program), false);
    let (status, bytes, group) = agent_runs(&command, b"");
    assert_eq!(status.code(), Some(4));
    assert_eq!(String::from_utf8(bytes).unwrap().trim(), group.to_string());
}

/// DC-220: a request a release withdrew before the launcher took
/// it starts nothing, and leaves no process group's record behind.
#[test]
fn a_withdrawn_request_starts_nothing() {
    let lease = tempfile::tempdir().unwrap();
    let lease = lease.path().canonicalize().unwrap();
    let marker = lease.join("ran");
    let program = line(&format!("touch {}", marker.display()));
    let (files, command) = prepared(&lease, "withdrawn", Launch::Program(&program), true);
    launch::withdraw_requests(&lease);
    let (status, bytes, _) = agent_runs(&command, b"");
    assert_eq!(status.code(), Some(launch::FAILED));
    assert!(String::from_utf8_lossy(&bytes).contains("no lease command"));
    assert!(!marker.exists());
    assert!(!files.group.exists());
    assert!(launch::recorded_groups(&lease).is_empty());
}
