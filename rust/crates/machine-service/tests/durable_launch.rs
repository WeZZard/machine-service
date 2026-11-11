//! TL-2: a durable lease's job as machine-service makes and feeds it, run
//! as the relay agent runs it: a (fake) launchd job whose files another
//! process made (RA-10, DM-4, DC-220, DC-221, DC-314).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Once;
use std::time::Duration;

use guest_actions::launchd::{label_for, EnvWriter, Launchd};
use guest_actions::{spawn_job, Exit};
use machine_service_core::durable::agents::Secret;
use machine_service_core::durable::exec::Program;
use machine_service_core::durable::launch::{self, Launch};
use machine_service_core::durable::session_env::SessionEnv;
use serde_json::Value;

const KEY: &str = "sk-fake-launch-key-0123456789abcdef";

/// This test process's tool table: the read-only system tools and the
/// fake `launchctl`, with the fake's state of its own.
fn fake_launchd() -> Launchd {
    static SETUP: Once = Once::new();
    SETUP.call_once(|| {
        let root = std::env::temp_dir().join(format!("ms-durable-launch-{}", std::process::id()));
        let variables = service_layout::test_build::harness::fake_launchd(&root).unwrap();
        for (name, value) in variables {
            // SAFETY: set once, before any test here reads them.
            unsafe { std::env::set_var(name, value) };
        }
    });
    Launchd::resolve().unwrap()
}

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
        shell: "/bin/zsh".into(),
        line: text.into(),
    }
}

/// Make `execution`'s job and run it as the relay agent would, with
/// `input` on its standard input when given.
fn run(
    lease: &Path,
    execution: &str,
    launch: Launch<'_>,
    keys: bool,
    input: Option<&[u8]>,
    before: impl FnOnce(&launch::Files),
) -> (Exit, String, i32, launch::Files) {
    let launchd = fake_launchd();
    let files = launch::files(lease, execution);
    let job = launch::prepare(&files, launch, &session(keys), lease, |message| {
        eprintln!("{message}")
    })
    .unwrap();
    assert!(!job.dir.contains(KEY));
    assert_eq!(job.redacted, keys);
    before(&files);
    let spawned = spawn_job(
        &launchd,
        &label_for("tl2", execution),
        Path::new("/bin/zsh"),
        Path::new(&job.dir),
        "",
        input.is_some(),
        EnvWriter::Elsewhere {
            redacted: job.redacted,
        },
    )
    .unwrap();
    let group = spawned.group;
    let stdin = input.map(|bytes| Box::new(std::io::Cursor::new(bytes.to_vec())) as _);
    let mut output = Vec::new();
    let exit = spawned.supervise_from(stdin, Duration::from_secs(30), |bytes| {
        output.extend_from_slice(bytes);
        true
    });
    (exit, String::from_utf8(output).unwrap(), group, files)
}

/// DC-221: with loaded keys the job's output reaches the agent with every
/// key redacted, also a key the command writes in pieces, and the agent
/// sees the command's exit status after all of its output. Without keys
/// the job writes to the agent directly. The command runs in the lease's
/// working directory with the session's variables, and its group is
/// recorded and its claim taken before it runs (DC-220).
#[test]
fn a_job_runs_in_the_session_with_its_keys_redacted() {
    let lease = tempfile::tempdir().unwrap();
    let lease = lease.path().canonicalize().unwrap();
    let (half, rest) = KEY.split_at(12);
    let program = line(&format!(
        "printf '%s' '{half}'; sleep 0.3; printf '%s\\n' '{rest}'; echo \"env=$ANTHROPIC_API_KEY\" >&2; pwd; exit 5"
    ));
    let (exit, text, group, files) = run(
        &lease,
        "split",
        Launch::Program(&program),
        true,
        None,
        |_| {},
    );
    assert_eq!(exit.rc, Some(5), "{text}");
    assert_eq!(
        text,
        format!("[redacted]\nenv=[redacted]\n{}\n", lease.display())
    );
    assert!(!files.claim.exists(), "the feeder took the claim");
    let recorded: Value = serde_json::from_slice(&std::fs::read(&files.group).unwrap()).unwrap();
    assert_eq!(recorded["sid"], group, "{recorded}");
    assert_eq!(recorded["group"], true);

    // A client's script arrives on standard input.
    let (exit, text, _, _) = run(
        &lease,
        "script",
        Launch::ScriptOnInput,
        true,
        Some(b"echo \"script $ANTHROPIC_API_KEY\"\nexit 3\n"),
        |_| {},
    );
    assert_eq!(exit.rc, Some(3), "{text}");
    assert_eq!(text, "script [redacted]\n");

    // A script machine-service's own exec has.
    let script = Program::Script(b"echo \"own $ANTHROPIC_API_KEY\"\n".to_vec());
    let (exit, text, _, _) = run(&lease, "own", Launch::Program(&script), true, None, |_| {});
    assert_eq!(exit.rc, Some(0), "{text}");
    assert_eq!(text, "own [redacted]\n");

    // Without keys to redact, the job writes to the agent itself.
    // The command's shell is in the job's group, which its prelude leads.
    let program = line("/bin/ps -o pgid= -p $$; exit 4");
    let (exit, text, group, _) = run(
        &lease,
        "plain",
        Launch::Program(&program),
        false,
        None,
        |_| {},
    );
    assert_eq!(exit.rc, Some(4));
    assert_eq!(text.trim(), group.to_string());
}

/// DC-220: a job a release withdrew before its feeder took the claim runs
/// nothing: its shell ends with the launch failure, and its group, recorded
/// first, is one a release may stop.
#[test]
fn a_withdrawn_job_runs_nothing() {
    let lease = tempfile::tempdir().unwrap();
    let lease = lease.path().canonicalize().unwrap();
    let marker = lease.join("ran");
    let program = line(&format!("touch {}", marker.display()));
    let (exit, text, _, files) = run(
        &lease,
        "withdrawn",
        Launch::Program(&program),
        true,
        None,
        |_| launch::withdraw_requests(&lease),
    );
    assert_eq!(exit.rc, Some(launch::FAILED), "{text}");
    assert!(!marker.exists());
    assert!(!files.claim.exists());
}

/// A second call with the same execution ID while the first's job lives
/// names the same job and starts no second feeder, so the first job still
/// gets its command, its redacted output and its exit status.
#[test]
fn a_repeated_execution_names_the_same_job() {
    let lease = tempfile::tempdir().unwrap();
    let lease = lease.path().canonicalize().unwrap();
    let program = line("echo \"once $ANTHROPIC_API_KEY\"; exit 6");
    let (exit, text, _, files) = run(
        &lease,
        "again",
        Launch::Program(&program),
        true,
        None,
        |files| {
            let again = launch::prepare(
                files,
                Launch::Program(&line("echo twice")),
                &session(true),
                &lease,
                |message| eprintln!("{message}"),
            )
            .unwrap();
            assert_eq!(Path::new(&again.dir), files.job.as_path());
            assert!(again.redacted);
        },
    );
    assert_eq!(exit.rc, Some(6), "{text}");
    assert_eq!(text, "once [redacted]\n");
    assert!(!files.claim.exists());
}
