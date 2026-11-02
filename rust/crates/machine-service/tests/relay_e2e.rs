//! TL-2 end to end: the real `machine-service` daemon with
//! `MACHINE_SERVICE_GUEST_LINK=relay`, and the real `relay-agent` binary as a
//! local process standing for the guest's desktop session (RA-7, BK-1 to
//! BK-4, RP-1 to RP-11).
//!
//! The fixture's fake `ssh` runs the relay bootstrap's steps for real against
//! a fake guest home: it inspects and uploads the relay agent and writes its
//! configuration. It does not run the start step, which would load a
//! LaunchAgent into this Mac's own desktop session; the test starts the
//! relay agent itself before acquiring, as an image that ships it would,
//! waiting for its configuration (pilot-images RA-I2).
//!
//! `through_the_rig_under_n1` sends both the relay agent's and the client's
//! sessions through the TL-7 network rig under N1; it is `#[ignore]`d
//! because it fetches the rig's pinned tools:
//!
//! ```sh
//! cargo test -p machine-service --test relay_e2e -- --ignored
//! ```

mod common;
#[path = "../../relay-broker/tests/common/mod.rs"]
mod relay_common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use relay_agent::protocol::{ExecutionState, Op, OpKind, Outcome};
use relay_common::{client_config, exec, TestClient};
use serde_json::{json, Value};

const WAIT: Duration = Duration::from_secs(60);

/// The fake `ssh` in front of the fixture's: relay bootstrap steps run in
/// the fake guest home, the start step is answered without running.
const RELAY_SSH: &str = r#"#!/bin/sh
case "$*" in
  *machine-service-relay-start*)
    cat >"$FAKE_GUEST_HOME/.relay-start-script"
    printf 'start\n' >> "$FAKE_ROOT/relay-calls.log"
    echo "relay-start: state = running (the test's own relay agent)"
    exit 0 ;;
  *machine-service-relay-*)
    last=""
    for a in "$@"; do last="$a"; done
    printf '%s\n' "$last" >> "$FAKE_ROOT/relay-calls.log"
    HOME="$FAKE_GUEST_HOME" exec sh -c "$last" ;;
esac
exec "$(dirname "$0")/ssh-fake" "$@"
"#;

/// The relay agent binary, run as the guest's desktop session would run it.
struct AgentProcess {
    child: Child,
    log: PathBuf,
}

impl AgentProcess {
    fn start(home: &Path, env: &[(String, String)]) -> AgentProcess {
        AgentProcess::start_from(relay_agent_bin(), home, env)
    }

    /// Start the agent from `executable`, as launchd or systemd starts the
    /// one installed in the guest.
    fn start_from(executable: &Path, home: &Path, env: &[(String, String)]) -> AgentProcess {
        let log = home.join("agent.log");
        let child = Command::new(executable)
            .arg("run")
            .env("HOME", home)
            .env("SHELL", "/bin/sh")
            .envs(env.iter().cloned())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .unwrap();
        AgentProcess { child, log }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

impl Drop for AgentProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Lab {
    fixture: LegacyFixture,
    guest_home: PathBuf,
    _guest: tempfile::TempDir,
    release: PathBuf,
    _release: tempfile::TempDir,
    env: Vec<(String, String)>,
}

impl Lab {
    /// A fixture whose daemon serves the relay link on `session_port`, and
    /// whose guests are told to dial `guest_url`.
    fn new(session_port: u16, guest_url: &str) -> Lab {
        let fixture = LegacyFixture::new();
        write_exec(&fixture.bin, "ssh-fake", SSH_SHIM);
        write_exec(&fixture.bin, "ssh", RELAY_SSH);
        let guest = tempfile::tempdir().unwrap();
        let guest_home = guest.path().canonicalize().unwrap();
        // DC-140: the daemon sends only a release's executable.
        let release_dir = tempfile::tempdir().unwrap();
        let release = write_relay_release(&release_dir.path().join("release"), relay_agent_bin());
        let env = vec![
            ("MACHINE_SERVICE_GUEST_LINK".into(), "relay".into()),
            (
                "MACHINE_SERVICE_SESSION_PORT".into(),
                session_port.to_string(),
            ),
            (
                "MACHINE_SERVICE_RELAY_GUEST_URL".into(),
                guest_url.to_string(),
            ),
            (
                "MACHINE_SERVICE_RELAY_URL".into(),
                format!("wss://localhost:{session_port}/relay"),
            ),
            ("MACHINE_SERVICE_RELAY_REGISTER_TIMEOUT".into(), "30".into()),
            ("MACHINE_SERVICE_RELAY_AGENT_WAIT".into(), "30".into()),
            (
                "MACHINE_SERVICE_RELAY_AGENT_RELEASE".into(),
                release.display().to_string(),
            ),
            ("FAKE_GUEST_HOME".into(), guest_home.display().to_string()),
        ];
        Lab {
            fixture,
            guest_home,
            _guest: guest,
            release,
            _release: release_dir,
            env,
        }
    }

    fn start(&self) -> Daemon {
        self.fixture.start_daemon_with(&self.env)
    }

    fn post(&self, path: &str, body: &Value) -> Response {
        http_timeout(
            self.fixture.port(),
            "POST",
            path,
            Some(body),
            None,
            Duration::from_secs(180),
        )
    }

    fn relay_calls(&self) -> String {
        std::fs::read_to_string(self.fixture.tart_home.join("relay-calls.log")).unwrap_or_default()
    }
}

fn acquire(lab: &Lab) -> Value {
    let response = lab.post(
        "/acquire",
        &json!({"purpose": "relay-e2e", "image": "ubuntu2404", "env": "none"}),
    );
    assert_eq!(response.status, 200, "{}", response.body);
    response.body
}

fn trust(answer: &Value) -> Arc<rustls::ClientConfig> {
    let pem = answer["relay"]["ca_pem"].as_str().unwrap();
    assert_eq!(
        relay_broker::tls::pem_sha256(pem).as_deref(),
        answer["relay"]["ca_sha256"].as_str(),
        "the authority matches its fingerprint (RP-1)"
    );
    remote_session::tls::client_config(&[pem.as_bytes()]).unwrap()
}

fn connect(url: &str, credential: &str, tls: Arc<rustls::ClientConfig>) -> TestClient {
    let mut config = client_config(url, credential);
    config.tls = Some(tls);
    TestClient::connect_with(config).unwrap()
}

fn free_session_port() -> u16 {
    free_port()
}

#[test]
fn a_relay_lease_runs_its_actions_through_the_relay_agent() {
    let session_port = free_session_port();
    let lab = Lab::new(
        session_port,
        &format!("wss://localhost:{session_port}/relay"),
    );
    let agent = AgentProcess::start(&lab.guest_home, &[]);
    let mut daemon = lab.start();

    // Discovery (BK-4, RP-11).
    let health = http(lab.fixture.port(), "GET", "/health", None, None).body;
    assert_eq!(
        health["relay_url"],
        json!(format!("wss://localhost:{session_port}/relay"))
    );
    let fingerprint = health["relay_ca_sha256"].as_str().unwrap().to_string();
    let (status, _, pem) = raw_http(lab.fixture.port(), "GET", "/relay/ca.pem", &[], b"");
    assert_eq!(status, 200);
    assert_eq!(
        relay_broker::tls::pem_sha256(&String::from_utf8(pem).unwrap()).as_deref(),
        Some(fingerprint.as_str())
    );

    // RA-7: the clone's relay agent registers before the lease runs.
    let answer = acquire(&lab);
    let vm = answer["vm"].as_str().unwrap().to_string();
    assert_eq!(answer["state"], "running");
    assert_eq!(
        answer["link"],
        "relay",
        "{answer}\nagent log:\n{}",
        agent.log()
    );
    assert_eq!(answer["relay_agent"]["os"], json!(std::env::consts::OS));
    assert_eq!(answer["relay"]["url"], health["relay_url"]);
    let credential = answer["relay"]["credential"].as_str().unwrap().to_string();
    let calls = lab.relay_calls();
    for step in ["inspect", "upload", "config"] {
        assert!(
            calls.contains(&format!("machine-service-relay-{step}")),
            "{calls}"
        );
    }
    assert!(calls.contains("start"));
    // The configuration is private (DC-126), and the upload is the build.
    let config = lab.guest_home.join(".config/relay-agent/config.json");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&config).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let uploaded = std::fs::read(
        lab.guest_home
            .join(".local/libexec/relay-agent/relay-agent"),
    )
    .unwrap();
    // DC-140: the upload is exactly the release archive's executable.
    let released = machine_service_core::service_layout::release::Release::open(&lab.release)
        .unwrap()
        .executable(&machine_service_core::service_layout::release::platform_for("linux"))
        .unwrap();
    assert_eq!(uploaded, released);
    assert!(
        std::fs::read_to_string(lab.guest_home.join(".relay-start-script"))
            .unwrap()
            .contains("graphical-session.target")
    );
    // The credential is in the answer only.
    let listed = http(
        lab.fixture.port(),
        "GET",
        &format!("/machines/{vm}"),
        None,
        None,
    )
    .body;
    assert_eq!(listed["link"], "relay");
    assert!(!listed.to_string().contains(&credential));
    assert!(listed.get("relay").is_none());

    // HTTP exec, push and pull go through the relay agent (RA-5).
    let ran = lab.post(
        &format!("/machines/{vm}/exec"),
        &json!({"argv": ["sh", "-c", "echo run >> runs; pwd; wc -l < runs | tr -d ' '"], "execution_id": "e2e-once"}),
    );
    assert_eq!(ran.status, 200, "{}", ran.body);
    assert_eq!(ran.body["link"], "relay");
    assert_eq!(
        ran.body["output"],
        json!(format!("{}\n1\n", lab.guest_home.display()))
    );
    let again = lab.post(
        &format!("/machines/{vm}/exec"),
        &json!({"argv": ["sh", "-c", "echo run >> runs; pwd; wc -l < runs | tr -d ' '"], "execution_id": "e2e-once"}),
    );
    assert_eq!(again.body["output"], ran.body["output"], "never runs twice");
    let script = lab.post(
        &format!("/machines/{vm}/exec"),
        &json!({"script": "echo from-script; exit 3"}),
    );
    assert_eq!(script.body["rc"], 3);
    assert_eq!(script.body["output"], "from-script\n");
    let local = lab.fixture.root.join("push.bin");
    let bytes: Vec<u8> = (0..300_000u32).map(|i| (i % 253) as u8).collect();
    std::fs::write(&local, &bytes).unwrap();
    let pushed = lab.post(
        &format!("/machines/{vm}/push"),
        &json!({"local_path": local, "remote_path": "pushed.bin"}),
    );
    assert_eq!(pushed.status, 200, "{}", pushed.body);
    assert_eq!(pushed.body["link"], "relay");
    assert_eq!(
        std::fs::read(lab.guest_home.join("pushed.bin")).unwrap(),
        bytes
    );
    let back = lab.fixture.root.join("pulled.bin");
    let pulled = lab.post(
        &format!("/machines/{vm}/pull"),
        &json!({"local_path": back, "remote_path": "pushed.bin"}),
    );
    assert_eq!(pulled.status, 200, "{}", pulled.body);
    assert_eq!(pulled.body["sha256"], json!(sha256_hex(&bytes)));
    assert_eq!(std::fs::read(&back).unwrap(), bytes);

    // A folder goes as a tar stream through the relay agent too (RP-13),
    // placed as `scp -r` places it and replaced whole.
    let folder = lab.fixture.root.join("tree");
    std::fs::create_dir_all(folder.join("nested")).unwrap();
    std::fs::write(folder.join("nested/big.bin"), &bytes).unwrap();
    std::fs::write(folder.join("top.txt"), "top").unwrap();
    std::fs::create_dir_all(lab.guest_home.join("work/tree")).unwrap();
    std::fs::write(lab.guest_home.join("work/tree/stale.txt"), "stale").unwrap();
    let scp_calls =
        || std::fs::read_to_string(lab.fixture.tart_home.join("scp-calls.log")).unwrap_or_default();
    let scp_before = scp_calls();
    let pushed = lab.post(
        &format!("/machines/{vm}/push"),
        &json!({"local_path": folder, "remote_path": "work"}),
    );
    assert_eq!(pushed.status, 200, "{}", pushed.body);
    assert_eq!(pushed.body["link"], "relay");
    assert_eq!(pushed.body["folder"], true);
    assert_eq!(
        std::fs::read(lab.guest_home.join("work/tree/nested/big.bin")).unwrap(),
        bytes
    );
    assert!(
        !lab.guest_home.join("work/tree/stale.txt").exists(),
        "the folder is replaced whole"
    );
    let landing = lab.fixture.root.join("landing");
    std::fs::create_dir_all(&landing).unwrap();
    let pulled = lab.post(
        &format!("/machines/{vm}/pull"),
        &json!({"local_path": landing, "remote_path": "work/tree"}),
    );
    assert_eq!(pulled.status, 200, "{}", pulled.body);
    assert_eq!(pulled.body["link"], "relay");
    assert_eq!(pulled.body["folder"], true);
    assert_eq!(
        std::fs::read(landing.join("tree/nested/big.bin")).unwrap(),
        bytes
    );
    assert_eq!(
        std::fs::read_to_string(landing.join("tree/top.txt")).unwrap(),
        "top"
    );
    let entries: Vec<_> = std::fs::read_dir(&landing)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(entries, ["tree"], "nothing is left beside the folder");
    assert_eq!(scp_calls(), scp_before, "no folder went by scp");

    // machctl's exec takes the same route (it calls the HTTP API).
    let cli = run_machctl(
        &["exec", &vm, "--", "sh", "-c", "pwd"],
        &lab.fixture.machctl_env(),
    );
    assert!(
        cli.status.success(),
        "{}",
        String::from_utf8_lossy(&cli.stderr)
    );
    assert!(String::from_utf8_lossy(&cli.stdout).contains(&lab.guest_home.display().to_string()));

    // A client of the lease reaches the same relay agent and the same
    // records (RP-3, RP-7).
    let url = answer["relay"]["url"].as_str().unwrap().to_string();
    let tls = trust(&answer);
    let client = connect(&url, &credential, tls.clone());
    assert_eq!(client.ready.lease, vm);
    client.call(1, "e2e-once", exec("never runs"), b"");
    let replay = client.wait(1, WAIT);
    assert!(replay.result.unwrap().replayed);
    assert_eq!(
        replay.bytes,
        format!("{}\n1\n", lab.guest_home.display()).into_bytes()
    );
    client.call(
        2,
        "client-1",
        exec("echo client >> client.log; cat client.log"),
        b"",
    );
    assert_eq!(client.wait(2, WAIT).bytes, b"client\n");

    // Through a broker restart, the lease keeps its link and its records.
    drop(client);
    daemon.stop();
    let _daemon = lab.start();
    let client = connect(&url, &credential, tls.clone());
    client.query(1, "client-1");
    assert!(matches!(
        client.wait(1, WAIT).query,
        Some(ExecutionState::Completed {
            op: OpKind::Exec,
            ..
        })
    ));
    client.call(
        2,
        "client-1",
        exec("echo client >> client.log; cat client.log"),
        b"",
    );
    let replay = client.wait(2, WAIT);
    assert!(replay.result.unwrap().replayed);
    assert_eq!(replay.bytes, b"client\n");
    let after = lab.post(
        &format!("/machines/{vm}/exec"),
        &json!({"argv": ["cat", "client.log"]}),
    );
    assert_eq!(after.body["output"], "client\n", "{}", after.body);

    // Release revokes the credentials at once (RP-10).
    let released = lab.post(&format!("/machines/{vm}/release"), &json!({}));
    assert_eq!(released.status, 200, "{}", released.body);
    assert!(client.wait_end(WAIT));
    assert_eq!(client.revoked.lock().unwrap().as_deref(), Some("released"));
    let mut refused = client_config(&url, &credential);
    refused.tls = Some(tls);
    assert!(matches!(
        TestClient::connect_with(refused),
        Err(remote_session::Error::Rejected { .. })
    ));
    drop(agent);
}

#[test]
fn a_lease_whose_agent_never_registers_falls_back_to_ssh() {
    let session_port = free_session_port();
    let lab = Lab::new(
        session_port,
        &format!("wss://localhost:{session_port}/relay"),
    );
    let mut env = lab.env.clone();
    env.push(("MACHINE_SERVICE_RELAY_REGISTER_TIMEOUT".into(), "2".into()));
    let _daemon = lab.fixture.start_daemon_with(&env);
    // No relay agent runs.
    let answer = acquire(&lab);
    assert_eq!(answer["state"], "running");
    assert_eq!(answer["link"], "ssh-fallback");
    assert!(answer["link_reason"]
        .as_str()
        .unwrap()
        .contains("did not register within 2s"));
    assert!(answer.get("relay").is_none());
    let vm = answer["vm"].as_str().unwrap();
    let ran = lab.post(
        &format!("/machines/{vm}/exec"),
        &json!({"argv": ["echo", "hi"]}),
    );
    assert_eq!(ran.status, 200);
    assert!(
        ran.body.get("link").is_none(),
        "served over SSH: {}",
        ran.body
    );
    assert!(ran.body["output"].as_str().unwrap().starts_with("ran: "));
}

/// DC-140: a release whose archive does not match its `SHA256SUMS` is never
/// sent; the lease is served over SSH and its record says why, and nothing
/// is uploaded into the guest.
#[test]
fn a_release_that_does_not_match_its_sums_is_never_sent() {
    let session_port = free_session_port();
    let lab = Lab::new(
        session_port,
        &format!("wss://localhost:{session_port}/relay"),
    );
    let sums = lab.release.join("SHA256SUMS");
    let text = std::fs::read_to_string(&sums).unwrap();
    let file = text
        .lines()
        .find(|line| line.contains("-linux-"))
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap()
        .to_string();
    let mut bytes = std::fs::read(lab.release.join(&file)).unwrap();
    bytes.extend_from_slice(b"tampered");
    std::fs::write(lab.release.join(&file), bytes).unwrap();
    let _daemon = lab.start();
    let answer = acquire(&lab);
    assert_eq!(answer["state"], "running");
    assert_eq!(answer["link"], "ssh-fallback");
    let reason = answer["link_reason"].as_str().unwrap();
    assert!(reason.contains("does not match SHA256SUMS"), "{reason}");
    assert!(!lab
        .guest_home
        .join(".local/libexec/relay-agent/relay-agent")
        .exists());
    assert!(!lab.relay_calls().contains("machine-service-relay-upload"));
}

#[test]
#[ignore = "fetches the network rig's pinned tools; run with --ignored"]
fn through_the_rig_under_n1() {
    use network_rig::{Profile, Rig, RigConfig};
    let session_port = free_session_port();
    let run_dir = tempfile::tempdir().unwrap();
    // The broker's authority exists before the daemon starts, so the rig's
    // edge can trust it upstream.
    let lab_state = |lab: &Lab| lab.fixture.state_dir.join("relay/tls");
    let probe = Lab::new(session_port, "wss://unused.invalid/relay");
    let authority = relay_broker::tls::Authority::load_or_create(&lab_state(&probe)).unwrap();
    let rig = Rig::start(RigConfig::new(
        Profile::builtin("N1").unwrap(),
        format!("127.0.0.1:{session_port}").parse().unwrap(),
        authority.cert_pem(),
        run_dir.path(),
    ))
    .unwrap();
    let side = rig.client_side().clone();
    let mut lab = probe;
    for (key, value) in lab.env.iter_mut() {
        if key == "MACHINE_SERVICE_RELAY_GUEST_URL" || key == "MACHINE_SERVICE_RELAY_URL" {
            *value = side.url.clone();
        }
    }
    // The relay agent dials the rig by name and trusts only the edge.
    let agent = AgentProcess::start(
        &lab.guest_home,
        &[
            (
                relay_agent::session_link::CA_BUNDLE_VARIABLE.into(),
                side.ca_bundle.display().to_string(),
            ),
            (
                relay_agent::session_link::RESOLVE_VARIABLE.into(),
                side.resolve(),
            ),
        ],
    );
    let _daemon = lab.start();
    let answer = acquire(&lab);
    assert_eq!(
        answer["link"],
        "relay",
        "{answer}\nagent log:\n{}",
        agent.log()
    );
    assert_eq!(answer["relay"]["url"], json!(side.url));
    let vm = answer["vm"].as_str().unwrap().to_string();

    let edge = std::fs::read_to_string(&side.ca_bundle).unwrap();
    let mut config = client_config(&side.url, answer["relay"]["credential"].as_str().unwrap());
    config.tls = Some(remote_session::tls::client_config(&[edge.as_bytes()]).unwrap());
    config.resolver = Some(remote_session::fixed_resolver(&side.resolve()).unwrap());
    let client = TestClient::connect_with(config).unwrap();
    assert_eq!(client.ready.lease, vm);
    // 3 MiB each way crosses the 1 MiB message limit only in chunks (TC-8).
    let bytes: Vec<u8> = (0..3 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    client.call(
        1,
        "rig-write",
        Op::Write {
            path: "rig.bin".into(),
            name: String::new(),
            mode: None,
        },
        &bytes,
    );
    assert!(matches!(
        client.wait(1, WAIT).result.unwrap().outcome,
        Outcome::Written(_)
    ));
    client.call(
        2,
        "rig-read",
        Op::Read {
            path: "rig.bin".into(),
        },
        b"",
    );
    let read = client.wait(2, WAIT);
    assert!(read.end_matches());
    assert_eq!(read.bytes, bytes);
    client.call(3, "rig-exec", exec("echo through-the-edge"), b"");
    assert_eq!(client.wait(3, WAIT).bytes, b"through-the-edge\n");
    let started = Instant::now();
    let ran = lab.post(
        &format!("/machines/{vm}/exec"),
        &json!({"argv": ["cat", "rig.bin"]}),
    );
    assert_eq!(ran.status, 200);
    eprintln!("an HTTP exec through the rig took {:?}", started.elapsed());
    let released = lab.post(&format!("/machines/{vm}/release"), &json!({}));
    assert_eq!(released.status, 200);
    drop(agent);
}

/// RA-6 for a lease that lives on: the instance restarts with another
/// relay agent build than the lease's agent runs. When the agent registers
/// again, the instance writes its build over the agent's executable through
/// the relay and restarts the agent once no call runs; the command that ran
/// across the instance's restart completes, and the records persist.
#[test]
fn a_long_lived_lease_gets_the_instances_new_relay_agent_build() {
    let session_port = free_session_port();
    let lab = Lab::new(
        session_port,
        &format!("wss://localhost:{session_port}/relay"),
    );
    // The build this instance holds, and the guest's installed agent: the
    // same bytes at first, so bootstrap uploads nothing.
    // The instance holds its build as a release (DC-140); the lab's release
    // already holds this build.
    let release = lab
        .env
        .iter()
        .find(|(key, _)| key == "MACHINE_SERVICE_RELAY_AGENT_RELEASE")
        .map(|(_, value)| std::path::PathBuf::from(value))
        .unwrap();
    let held = lab.fixture.root.join("held-relay-agent");
    let installed = lab
        .guest_home
        .join(".local/libexec/relay-agent/relay-agent");
    std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
    std::fs::copy(relay_agent_bin(), &installed).unwrap();
    let agent = AgentProcess::start_from(&installed, &lab.guest_home, &[]);
    let mut daemon = lab.start();
    let answer = acquire(&lab);
    assert_eq!(
        answer["link"],
        "relay",
        "{answer}\nagent log:\n{}",
        agent.log()
    );
    let vm = answer["vm"].as_str().unwrap().to_string();
    let old = sha256_hex(&std::fs::read(relay_agent_bin()).unwrap());
    assert_eq!(answer["relay_agent"]["build"], json!(old));
    assert!(!lab.relay_calls().contains("machine-service-relay-upload"));

    let url = answer["relay"]["url"].as_str().unwrap().to_string();
    let credential = answer["relay"]["credential"].as_str().unwrap().to_string();
    let tls = trust(&answer);
    let client = connect(&url, &credential, tls.clone());
    client.call(1, "before", exec("echo before"), b"");
    assert_eq!(client.wait(1, WAIT).bytes, b"before\n");
    client.call(2, "across", exec("sleep 4; echo across"), b"");
    assert!(wait_until_e2e(|| String::from_utf8_lossy(
        &std::fs::read(
            lab.guest_home
                .join(".local/state/relay-agent/executions")
                .join(sha256_hex(b"across"))
                .join("record.json")
        )
        .unwrap_or_default()
    )
    .contains("across")));
    drop(client);

    // The instance comes back with a new build.
    let mut build = std::fs::read(relay_agent_bin()).unwrap();
    build.extend_from_slice(b"a newer build");
    let new = sha256_hex(&build);
    std::fs::write(&held, &build).unwrap();
    write_relay_release(&release, &held);
    daemon.stop();
    let _daemon = lab.start();

    // The command that ran across the instance's restart completes; the
    // agent was not restarted under it.
    let client = connect(&url, &credential, tls.clone());
    client.call(1, "across", exec("never runs"), b"");
    let across = client.wait(1, WAIT);
    assert_eq!(across.bytes, b"across\n", "agent log:\n{}", agent.log());
    assert!(matches!(
        across.result.unwrap().outcome,
        Outcome::Exit { rc: Some(0), .. }
    ));

    // Then the agent runs the new build, in the same process.
    let machine = || {
        http(
            lab.fixture.port(),
            "GET",
            &format!("/machines/{vm}"),
            None,
            None,
        )
        .body
    };
    assert!(
        wait_until_e2e(|| machine()["relay_agent"]["build"] == json!(new)),
        "{}\nagent log:\n{}",
        machine(),
        agent.log()
    );
    assert_eq!(std::fs::read(&installed).unwrap(), build);
    assert!(agent.log().contains("restarting onto"), "{}", agent.log());
    let ran = lab.post(
        &format!("/machines/{vm}/exec"),
        &json!({"argv": ["echo", "after"]}),
    );
    assert_eq!(ran.body["output"], "after\n", "{}", ran.body);
    // The records persist across the agent's restart.
    let client = connect(&url, &credential, tls);
    client.call(2, "before", exec("never runs"), b"");
    let replay = client.wait(2, WAIT);
    assert!(replay.result.unwrap().replayed);
    assert_eq!(replay.bytes, b"before\n");
    let released = lab.post(&format!("/machines/{vm}/release"), &json!({}));
    assert_eq!(released.status, 200);
    drop(agent);
}

fn wait_until_e2e(mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    condition()
}
