//! Guest operations over a guest session (`docs/arch/guest-session.md`).
//!
//! The service runs against [`common::guest::FakeGuest`], which serves each
//! channel with the real agent request loop in a temporary guest home, so the
//! outcomes below come from real frames, real commands and real files. Faults
//! are cut into a channel at a chosen byte, either side of the commit point.
//! The acceptance criteria are in `docs/acceptance/vm-service.acceptance`.

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use guest_session::agent::hex;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use common::guest::{Cut, AGENT};
use common::Fixture;
use machine_service_core::config::GuestLink;

/// A running lease on a fixture whose guest link is `link`.
fn leased(link: GuestLink) -> (Fixture, String) {
    let fixture = Fixture::with_guest_link(link);
    let record = fixture.acquire("guest-session", "ubuntu2404", "none");
    let vm = record["vm"].as_str().expect("vm name").to_string();
    (fixture, vm)
}

/// A running lease whose guest session is already open, so plans apply to
/// the next channel.
fn warm(link: GuestLink) -> (Fixture, String) {
    let (fixture, vm) = leased(link);
    let answer = exec(&fixture, &vm, &["true"]).expect("first exec");
    assert_eq!(answer["rc"], 0);
    (fixture, vm)
}

fn exec(
    fixture: &Fixture,
    vm: &str,
    argv: &[&str],
) -> Result<Value, machine_service_core::error::OpError> {
    fixture.service.guest_exec(vm, &json!({"argv": argv}))
}

fn opens(fixture: &Fixture) -> usize {
    fixture.host.guest.opens.load(Ordering::SeqCst)
}

fn uploads(fixture: &Fixture) -> usize {
    fixture.host.guest.uploads.load(Ordering::SeqCst)
}

fn guest_path(fixture: &Fixture, name: &str) -> PathBuf {
    fixture.host.guest.home.join(name)
}

fn host_path(fixture: &Fixture, name: &str) -> PathBuf {
    let path = fixture
        .dir
        .path()
        .canonicalize()
        .expect("root")
        .join("host");
    std::fs::create_dir_all(&path).expect("host dir");
    path.join(name)
}

fn lines(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|text| text.lines().count())
        .unwrap_or(0)
}

/// Bytes that differ at every offset a cut could land on.
fn pattern(size: usize) -> Vec<u8> {
    (0..size).map(|index| (index * 31 % 251) as u8).collect()
}

fn service_log(fixture: &Fixture) -> String {
    let root = fixture.dir.path().canonicalize().expect("root");
    std::fs::read_to_string(root.join("service.log")).unwrap_or_default()
}

// ------------------------------------------------------------- AC-1, AC-2

#[test]
fn one_session_carries_every_exec_push_and_pull_of_a_lease() {
    let (fixture, vm) = leased(GuestLink::SshSession);
    for index in 0..20 {
        let answer = exec(&fixture, &vm, &["echo", &format!("call {index}")]).expect("exec");
        assert_eq!(answer["rc"], 0);
        assert_eq!(answer["output"], format!("call {index}\n"));
    }
    for index in 0..5 {
        let local = host_path(&fixture, &format!("push-{index}.bin"));
        let bytes = pattern(70_000 + index);
        std::fs::write(&local, &bytes).expect("local file");
        let remote = format!("pushed-{index}.bin");
        let answer = fixture
            .service
            .guest_push(&vm, &json!({"local_path": local, "remote_path": remote}))
            .expect("push");
        assert_eq!(answer["bytes"], bytes.len());
        assert_eq!(
            std::fs::read(guest_path(&fixture, &remote)).expect("guest"),
            bytes
        );

        let back = host_path(&fixture, &format!("pull-{index}.bin"));
        let answer = fixture
            .service
            .guest_pull(&vm, &json!({"local_path": back, "remote_path": remote}))
            .expect("pull");
        assert_eq!(answer["sha256"], hex(&Sha256::digest(&bytes)));
        assert_eq!(std::fs::read(&back).expect("pulled"), bytes);
    }
    // The first open finds no agent; the upload installs it; the second open
    // serves all thirty operations.
    assert_eq!(opens(&fixture), 2, "channels opened");
    assert_eq!(uploads(&fixture), 1, "agent uploads");
    assert_eq!(
        fixture.host.guest.installed.lock().unwrap().as_deref(),
        Some(AGENT)
    );
    assert!(
        fixture.host.ssh.scp_calls.lock().unwrap().is_empty(),
        "no scp"
    );
}

#[test]
fn the_ssh_link_never_opens_a_session() {
    let (fixture, vm) = leased(GuestLink::Ssh);
    exec(&fixture, &vm, &["true"]).expect("exec");
    assert_eq!(opens(&fixture), 0);
    assert_eq!(uploads(&fixture), 0);
    assert!(!fixture.host.ssh.ssh_calls.lock().unwrap().is_empty());
}

#[test]
fn an_exec_answers_with_rc_and_output_like_ssh() {
    let (fixture, vm) = leased(GuestLink::SshSession);
    let answer = exec(
        &fixture,
        &vm,
        &["sh", "-c", "echo out; echo err >&2; exit 3"],
    )
    .expect("exec");
    assert_eq!(answer["vm"], vm);
    assert_eq!(answer["rc"], 3);
    // The session keeps stdout and stderr in the order they were written.
    assert_eq!(answer["output"], "out\nerr\n");

    let answer = fixture
        .service
        .guest_exec(&vm, &json!({"script": "echo one\necho two >&2\nexit 4\n"}))
        .expect("script");
    assert_eq!(answer["rc"], 4);
    assert_eq!(answer["output"], "one\ntwo\n");
}

#[test]
fn a_command_ended_by_a_signal_reports_rc_255_and_the_signal() {
    let (fixture, vm) = leased(GuestLink::SshSession);
    let answer = exec(&fixture, &vm, &["sh", "-c", "kill -TERM $$"]).expect("exec");
    assert_eq!(answer["rc"], 255);
    assert_eq!(answer["signal"], libc::SIGTERM);
}

#[test]
fn a_command_past_its_timeout_is_killed_and_reported() {
    let (fixture, vm) = leased(GuestLink::SshSession);
    let started = Instant::now();
    let error = fixture
        .service
        .guest_exec(&vm, &json!({"argv": ["sleep", "30"], "timeout": 1}))
        .expect_err("timeout");
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(
        error.message,
        "ssh failed: the command timed out after 1s and was killed"
    );
    assert!(!error.uncertain);
    // The session survives a timeout.
    exec(&fixture, &vm, &["true"]).expect("next exec");
    assert_eq!(opens(&fixture), 2);
}

// ------------------------------------------------------------------ GS-3

#[test]
fn an_agent_of_another_build_is_replaced_once() {
    let (fixture, vm) = warm(GuestLink::SshSession);
    fixture.service.close_guest_session(&vm);
    *fixture.host.guest.offered.lock().unwrap() = b"fake guest-session-agent build 2".to_vec();
    exec(&fixture, &vm, &["true"]).expect("exec");
    // The stale agent answers, is closed, is replaced, and the new one serves.
    assert_eq!(opens(&fixture), 4);
    assert_eq!(uploads(&fixture), 2);
    assert!(service_log(&fixture)
        .contains("uploading the guest session agent (the guest runs agent build"));
}

#[test]
fn a_failed_upload_is_reported_and_runs_nothing() {
    let (fixture, vm) = leased(GuestLink::SshSession);
    *fixture.host.ssh_hook.lock().unwrap() = Some(Arc::new(|_, _, _, _, _, _| {
        Some((1, "mkdir: Permission denied".to_string()))
    }));
    let marker = guest_path(&fixture, "ran");
    let error =
        exec(&fixture, &vm, &["touch", marker.to_str().unwrap()]).expect_err("upload fails");
    assert_eq!(
        error.message,
        "guest session agent upload failed: rc=1 mkdir: Permission denied"
    );
    assert!(!error.uncertain);
    assert!(!marker.exists());
    assert_eq!(opens(&fixture), 1);
}

// ------------------------------------------------------------------ GS-5

#[test]
fn an_exec_lost_after_its_end_frame_is_uncertain_and_never_resent() {
    let (fixture, vm) = warm(GuestLink::SshSession);
    fixture.service.close_guest_session(&vm);
    // The cut lands after the command printed, so it certainly ran once.
    fixture.host.guest.plan(Some(Cut::AgentOutputAfterHello(1)));
    let marker = guest_path(&fixture, "marker");
    let command = format!("echo ran >> {}; echo started; sleep 30", marker.display());
    let before = opens(&fixture);
    let error = exec(&fixture, &vm, &["sh", "-c", &command]).expect_err("lost");
    assert!(error.uncertain, "{}", error.message);
    assert!(
        error.message.contains("whether it ran is unknown"),
        "{}",
        error.message
    );
    assert_eq!(opens(&fixture), before + 1, "no second channel: not resent");
    assert_eq!(lines(&marker), 1);
    fixture.host.guest.wait_no_agents(Duration::from_secs(10));
}

#[test]
fn an_exec_lost_before_its_end_frame_is_resent_once() {
    let (fixture, vm) = warm(GuestLink::SshSession);
    fixture.service.close_guest_session(&vm);
    fixture.host.guest.plan(Some(Cut::HostInputAfter(64 << 10)));
    fixture.host.guest.plan(None);
    let marker = guest_path(&fixture, "marker");
    // The script is sent as standard input; the cut lands inside it.
    let mut script = format!("echo ran >> {}\n", marker.display());
    script.push_str(&format!("# {}\n", "x".repeat(1 << 20)));
    script.push_str("echo done\n");
    let before = opens(&fixture);
    let answer = fixture
        .service
        .guest_exec(&vm, &json!({"script": script}))
        .expect("resent");
    assert_eq!(answer["rc"], 0);
    assert_eq!(answer["output"], "done\n");
    assert_eq!(opens(&fixture), before + 2);
    assert_eq!(lines(&marker), 1, "the command ran exactly once");
    assert!(service_log(&fixture).contains("resending a Exec on a new guest session"));
}

#[test]
fn a_pull_lost_midway_is_resent_once() {
    let (fixture, vm) = warm(GuestLink::SshSession);
    fixture.service.close_guest_session(&vm);
    let bytes = pattern(2 << 20);
    std::fs::write(guest_path(&fixture, "big.bin"), &bytes).expect("guest file");
    fixture
        .host
        .guest
        .plan(Some(Cut::AgentOutputAfterHello(256 << 10)));
    fixture.host.guest.plan(None);
    let local = host_path(&fixture, "big.bin");
    let before = opens(&fixture);
    let answer = fixture
        .service
        .guest_pull(&vm, &json!({"local_path": local, "remote_path": "big.bin"}))
        .expect("resent pull");
    assert_eq!(answer["bytes"], bytes.len());
    assert_eq!(std::fs::read(&local).expect("pulled"), bytes);
    assert_eq!(opens(&fixture), before + 2);
}

#[test]
fn a_push_lost_after_its_end_frame_is_uncertain_and_never_resent() {
    let (fixture, vm) = warm(GuestLink::SshSession);
    fixture.service.close_guest_session(&vm);
    // The cut lands inside the agent's Done frame, after the file is in place.
    fixture.host.guest.plan(Some(Cut::AgentOutputAfterHello(1)));
    let local = host_path(&fixture, "file.bin");
    let bytes = pattern(100_000);
    std::fs::write(&local, &bytes).expect("local file");
    let before = opens(&fixture);
    let error = fixture
        .service
        .guest_push(
            &vm,
            &json!({"local_path": local, "remote_path": "file.bin"}),
        )
        .expect_err("lost");
    assert!(error.uncertain, "{}", error.message);
    assert!(error
        .message
        .starts_with("push failed after its bytes were sent"));
    assert_eq!(opens(&fixture), before + 1, "not resent");
    assert_eq!(
        std::fs::read(guest_path(&fixture, "file.bin")).expect("guest"),
        bytes
    );
}

#[test]
fn a_push_lost_before_its_end_frame_leaves_nothing_and_is_resent() {
    let (fixture, vm) = warm(GuestLink::SshSession);
    fixture.service.close_guest_session(&vm);
    fixture
        .host
        .guest
        .plan(Some(Cut::HostInputAfter(200 << 10)));
    fixture.host.guest.plan(None);
    let local = host_path(&fixture, "file.bin");
    let bytes = pattern(1 << 20);
    std::fs::write(&local, &bytes).expect("local file");
    fixture
        .service
        .guest_push(
            &vm,
            &json!({"local_path": local, "remote_path": "file.bin"}),
        )
        .expect("resent push");
    assert_eq!(
        std::fs::read(guest_path(&fixture, "file.bin")).expect("guest"),
        bytes
    );
    let entries: Vec<_> = std::fs::read_dir(&fixture.host.guest.home)
        .expect("guest home")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries, ["file.bin"], "the cut write left a temporary file");
}

#[test]
fn a_remote_error_is_not_uncertain_and_keeps_the_session() {
    let (fixture, vm) = warm(GuestLink::SshSession);
    let local = host_path(&fixture, "missing.bin");
    let before = opens(&fixture);
    let error = fixture
        .service
        .guest_pull(
            &vm,
            &json!({"local_path": local, "remote_path": "no/such/file"}),
        )
        .expect_err("not found");
    assert!(!error.uncertain);
    assert!(
        error.message.starts_with("pull failed: "),
        "{}",
        error.message
    );
    exec(&fixture, &vm, &["true"]).expect("exec");
    assert_eq!(opens(&fixture), before, "the session is reused");
}

#[test]
fn a_dead_session_is_replaced_before_the_next_request() {
    let (fixture, vm) = warm(GuestLink::SshSession);
    fixture.host.guest.cut_all();
    fixture.host.guest.wait_no_agents(Duration::from_secs(10));
    let marker = guest_path(&fixture, "marker");
    let before = opens(&fixture);
    // The idle session's ping fails, or its request is lost before the commit
    // point; either way the command runs once on a new session.
    exec(
        &fixture,
        &vm,
        &["sh", "-c", &format!("echo ran >> {}", marker.display())],
    )
    .expect("exec");
    assert_eq!(lines(&marker), 1);
    assert_eq!(opens(&fixture), before + 1);
}

/// A fresh session whose agent sends alive frames every `beat`, with `cut`
/// planned for it and a clean channel after it.
fn heartbeat_session(fixture: &Fixture, vm: &str, beat: Duration, cut: Cut) {
    fixture.service.close_guest_session(vm);
    *fixture.host.guest.heartbeat.lock().expect("heartbeat") = Some(beat);
    fixture.host.guest.plan(Some(cut));
    fixture.host.guest.plan(None);
}

#[test]
fn an_exec_whose_link_goes_silent_after_sending_is_uncertain_within_the_silence_limit() {
    let (fixture, vm) = warm(GuestLink::SshSession);
    let beat = Duration::from_millis(100);
    heartbeat_session(&fixture, &vm, beat, Cut::SilenceAfterHello);
    let marker = guest_path(&fixture, "marker");
    let before = opens(&fixture);
    let started = Instant::now();
    // The request reaches the agent, but nothing comes back and nothing ends.
    let error = fixture
        .service
        .guest_exec(
            &vm,
            &json!({"argv": ["sh", "-c", format!("echo ran >> {}", marker.display())], "timeout": 600}),
        )
        .expect_err("silent");
    let elapsed = started.elapsed();
    assert!(error.uncertain, "{}", error.message);
    assert!(
        error.message.contains("nothing from the agent"),
        "{}",
        error.message
    );
    let limit = beat * guest_session::client::MISSED_HEARTBEATS;
    assert!(
        elapsed >= limit && elapsed < limit + Duration::from_secs(2),
        "{elapsed:?}, not near the {limit:?} silence limit"
    );
    assert_eq!(opens(&fixture), before + 1, "no second channel: not resent");
    fixture.host.guest.wait_no_agents(Duration::from_secs(10));
    assert_eq!(lines(&marker), 1);
    // The next request opens a new session and works.
    exec(&fixture, &vm, &["true"]).expect("next exec");
    assert_eq!(opens(&fixture), before + 2);
}

#[test]
fn a_request_whose_link_stalls_before_its_end_frame_is_resent_once() {
    let (fixture, vm) = warm(GuestLink::SshSession);
    let beat = Duration::from_millis(100);
    heartbeat_session(&fixture, &vm, beat, Cut::StallAfterHello);
    let marker = guest_path(&fixture, "marker");
    let before = opens(&fixture);
    let started = Instant::now();
    // The first write blocks, as on a link whose send buffer never drains;
    // only the silence watchdog ending the channel frees it.
    let answer = exec(
        &fixture,
        &vm,
        &[
            "sh",
            "-c",
            &format!("echo ran >> {}; echo done", marker.display()),
        ],
    )
    .expect("resent");
    let elapsed = started.elapsed();
    assert_eq!(answer["output"], "done\n");
    assert_eq!(lines(&marker), 1, "the command ran exactly once");
    assert_eq!(opens(&fixture), before + 2);
    let limit = beat * guest_session::client::MISSED_HEARTBEATS;
    assert!(
        elapsed >= limit && elapsed < limit + Duration::from_secs(2),
        "{elapsed:?}, not near the {limit:?} silence limit"
    );
}

#[test]
fn an_idle_session_kept_alive_by_its_heartbeat_is_reused() {
    let (fixture, vm) = leased(GuestLink::SshSession);
    *fixture.host.guest.heartbeat.lock().expect("heartbeat") = Some(Duration::from_millis(50));
    exec(&fixture, &vm, &["true"]).expect("first exec");
    let before = opens(&fixture);
    std::thread::sleep(Duration::from_millis(50) * guest_session::client::MISSED_HEARTBEATS * 3);
    exec(&fixture, &vm, &["true"]).expect("exec");
    assert_eq!(opens(&fixture), before, "the session is reused");
}

// ------------------------------------------------------------------ GS-6

#[test]
fn directories_still_travel_by_scp() {
    let (fixture, vm) = warm(GuestLink::SshSession);
    let local = host_path(&fixture, "tree");
    std::fs::create_dir_all(local.join("sub")).expect("tree");
    fixture
        .service
        .guest_push(&vm, &json!({"local_path": local, "remote_path": "tree"}))
        .expect("push dir");
    std::fs::create_dir_all(guest_path(&fixture, "remote-tree")).expect("guest tree");
    let back = host_path(&fixture, "back");
    fixture
        .service
        .guest_pull(
            &vm,
            &json!({"local_path": back, "remote_path": "remote-tree"}),
        )
        .expect("pull dir");
    assert_eq!(fixture.host.ssh.scp_calls.lock().unwrap().len(), 2);
}

// ------------------------------------------------------------- AC-12, AC-13

#[test]
fn release_stops_a_running_command_and_the_agent() {
    let (fixture, vm) = warm(GuestLink::SshSession);
    let pid_file = guest_path(&fixture, "pid");
    let command = format!("echo $$ > {}; exec sleep 600", pid_file.display());
    let fixture = Arc::new(fixture);
    let running = {
        let fixture = Arc::clone(&fixture);
        let vm = vm.clone();
        std::thread::spawn(move || exec(&fixture, &vm, &["sh", "-c", &command]))
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let pid = loop {
        if let Some(pid) = std::fs::read_to_string(&pid_file)
            .ok()
            .and_then(|text| text.trim().parse::<i32>().ok())
        {
            break pid;
        }
        assert!(Instant::now() < deadline, "the command never started");
        std::thread::sleep(Duration::from_millis(20));
    };
    let started = Instant::now();
    fixture.release(&vm);
    let error = running.join().expect("thread").expect_err("cancelled");
    assert_eq!(
        error.message,
        format!("command cancelled by release of {vm}")
    );
    assert!(started.elapsed() < Duration::from_secs(10));
    let deadline = Instant::now() + Duration::from_secs(5);
    // SAFETY: signal 0 only checks that the process exists.
    while unsafe { libc::kill(pid, 0) } == 0 {
        assert!(Instant::now() < deadline, "sleep {pid} still runs");
        std::thread::sleep(Duration::from_millis(20));
    }
    fixture.host.guest.wait_no_agents(Duration::from_secs(10));
    assert!(service_log(&fixture).contains("running command cancelled by release"));
}

#[test]
fn release_closes_an_idle_session() {
    let (fixture, vm) = warm(GuestLink::SshSession);
    assert_eq!(fixture.host.guest.live.load(Ordering::SeqCst), 1);
    fixture.release(&vm);
    fixture.host.guest.wait_no_agents(Duration::from_secs(10));
    assert!(service_log(&fixture).contains(&format!("{vm}: guest session closed")));
}

// ------------------------------------------------------------------ AC-16

#[test]
fn the_guest_link_names_are_parsed_strictly() {
    assert_eq!(GuestLink::parse("ssh"), Ok(GuestLink::Ssh));
    assert_eq!(GuestLink::parse("ssh-session"), Ok(GuestLink::SshSession));
    assert_eq!(GuestLink::parse("relay"), Ok(GuestLink::Relay));
    assert_eq!(GuestLink::Relay.name(), "relay");
    assert_eq!(GuestLink::default(), GuestLink::Ssh);
    for value in ["tart-session", "SSH", "", "websocket"] {
        assert_eq!(
            GuestLink::parse(value),
            Err(format!(
                "MACHINE_SERVICE_GUEST_LINK must be one of ssh, ssh-session, relay; got {value:?}"
            ))
        );
    }
}
