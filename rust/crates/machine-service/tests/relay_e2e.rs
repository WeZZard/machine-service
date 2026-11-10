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
use std::process::Stdio;
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
    # The fake guest's own /usr/bin/codesign, when the test gives it one.
    if [ -x "$FAKE_ROOT/guest-codesign" ]; then
      last=$(printf '%s' "$last" | sed "s#/usr/bin/codesign#$FAKE_ROOT/guest-codesign#g")
    fi
    HOME="$FAKE_GUEST_HOME" exec sh -c "$last" ;;
esac
exec "$(dirname "$0")/ssh-fake" "$@"
"#;

/// The relay agent binary, run as the guest's desktop session would run it.
struct AgentProcess {
    child: common::Owned,
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
        let child = common::harness::command(executable)
            .arg("run")
            .env("HOME", home)
            .env("SHELL", "/bin/sh")
            // RA-10: the agent runs each command as a (fake) launchd job.
            .envs(
                service_layout::test_build::harness::fake_launchd(&home.join(".fake-launchd"))
                    .unwrap(),
            )
            .envs(env.iter().cloned())
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
        self.child.end();
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
    // DC-173: the VM's records went with the VM; release says so.
    assert_eq!(released.body["relay_records"]["purged"], false);
    assert!(released.body["relay_records"]["skipped"]
        .as_str()
        .unwrap()
        .contains("deleted with the VM"));
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

/// RA-8: a lease whose relay agent never registers within its bound is
/// served over SSH. The acquisition's answer and the lease record both say
/// `link: ssh-fallback` with the reason; the answer carries no `relay`; the
/// relay credentials the lease was given are revoked, so the agent
/// credential written into the guest is refused; and exec, a script, push
/// and pull all still work, over SSH and without a `link: relay`.
#[test]
fn ra8_a_lease_whose_agent_never_registers_falls_back_to_ssh() {
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
    let reason = answer["link_reason"].as_str().unwrap().to_string();
    assert!(reason.contains("did not register within 2s"), "{reason}");
    assert!(answer.get("relay").is_none());
    let vm = answer["vm"].as_str().unwrap().to_string();

    // The lease record keeps the link and its reason, and no credential.
    let record = http(
        lab.fixture.port(),
        "GET",
        &format!("/machines/{vm}"),
        None,
        None,
    );
    assert_eq!(record.status, 200, "{}", record.body);
    assert_eq!(record.body["link"], "ssh-fallback", "{}", record.body);
    assert_eq!(record.body["link_reason"], json!(reason));
    for field in ["relay_agent", "relay_agent_sha256", "relay_client_sha256"] {
        assert!(record.body.get(field).is_none(), "{field}: {}", record.body);
    }
    let log =
        std::fs::read_to_string(lab.fixture.state_dir.join("service.log")).unwrap_or_default();
    assert!(log.contains("link ssh-fallback, RA-8"), "{log}");

    // Bootstrap wrote an agent credential into the guest; it is revoked.
    let config: Value = serde_json::from_slice(
        &std::fs::read(lab.guest_home.join(".config/relay-agent/config.json")).unwrap(),
    )
    .unwrap();
    let pem = config["broker_ca_pem"].as_str().unwrap();
    let mut hello = relay_common::client_config(
        config["broker_url"].as_str().unwrap(),
        config["agent_credential"].as_str().unwrap(),
    );
    hello.attributes = json!({"role": "agent"});
    hello.tls = Some(remote_session::tls::client_config(&[pem.as_bytes()]).unwrap());
    match remote_session::connect(hello) {
        Err(remote_session::Error::Rejected { code, .. }) => {
            assert_eq!(code, remote_session::RejectCode::Unauthorized)
        }
        Err(other) => panic!("expected a refusal, got {other}"),
        Ok(_) => panic!("the fallen-back lease's agent credential was accepted"),
    }

    // Every operation still works, over SSH.
    let ran = lab.post(
        &format!("/machines/{vm}/exec"),
        &json!({"argv": ["echo", "hi"]}),
    );
    assert_eq!(ran.status, 200, "{}", ran.body);
    assert!(
        ran.body.get("link").is_none(),
        "served over SSH: {}",
        ran.body
    );
    assert!(ran.body["output"].as_str().unwrap().starts_with("ran: "));
    let script = lab.post(
        &format!("/machines/{vm}/exec"),
        &json!({"script": "echo from-script"}),
    );
    assert_eq!(script.status, 200, "{}", script.body);
    assert!(script.body.get("link").is_none(), "{}", script.body);
    let local = lab.fixture.root.join("fallback.bin");
    let bytes: Vec<u8> = (0..70_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&local, &bytes).unwrap();
    let pushed = lab.post(
        &format!("/machines/{vm}/push"),
        &json!({"local_path": local, "remote_path": "fallback.bin"}),
    );
    assert_eq!(pushed.status, 200, "{}", pushed.body);
    assert!(pushed.body.get("link").is_none(), "{}", pushed.body);
    let back = lab.fixture.root.join("fallback-back.bin");
    let pulled = lab.post(
        &format!("/machines/{vm}/pull"),
        &json!({"local_path": back, "remote_path": "fallback.bin"}),
    );
    assert_eq!(pulled.status, 200, "{}", pulled.body);
    assert!(pulled.body.get("link").is_none(), "{}", pulled.body);
    assert_eq!(std::fs::read(&back).unwrap(), bytes);
    let scp =
        std::fs::read_to_string(lab.fixture.tart_home.join("scp-calls.log")).unwrap_or_default();
    assert!(
        scp.contains("fallback.bin"),
        "push and pull went by scp: {scp}"
    );
    let released = lab.post(&format!("/machines/{vm}/release"), &json!({}));
    assert_eq!(released.status, 200, "{}", released.body);
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
    // Only the Linux build is new: the macOS one must stay a signable
    // executable for the daemon to trust the release (DC-231).
    write_relay_release_of(&release, "0.0.0-adhoc", relay_agent_bin(), &held);
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

/// A script that shows the recorded `codesign` output `fixture` of
/// service-layout's tests.
fn recorded_codesign(fixture: &str) -> String {
    let recorded = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../service-layout/tests/fixtures/codesign")
        .join(fixture)
        .canonicalize()
        .unwrap();
    format!("#!/bin/sh\ncat '{}'\n", recorded.display())
}

/// The fake guest's `/usr/bin/codesign`: the fake `ssh`, which stands for
/// the guest, runs it wherever a command names `/usr/bin/codesign`, so the
/// guest's relay agent reads as signed as `fixture` shows. The daemon's
/// command is the production one; only the guest is fake
/// (DC-239).
fn guest_codesign(lab: &Lab, fixture: &str) {
    write_exec(
        &lab.fixture.tart_home,
        "guest-codesign",
        &recorded_codesign(fixture),
    );
}

/// Put `bytes` in place as the guest's relay agent.
fn guest_agent(lab: &Lab, bytes: &[u8]) -> PathBuf {
    let installed = lab
        .guest_home
        .join(".local/libexec/relay-agent/relay-agent");
    std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
    std::fs::write(&installed, bytes).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&installed, std::fs::Permissions::from_mode(0o755)).unwrap();
    installed
}

fn service_log(lab: &Lab) -> String {
    std::fs::read_to_string(lab.fixture.state_dir.join("service.log")).unwrap_or_default()
}

/// DC-232 at bootstrap: a guest whose relay agent is signed with Developer
/// ID is never given the instance's ad hoc build; the lease is served over
/// SSH with the reason, and the guest's agent is left as it is.
#[test]
fn bootstrap_never_downgrades_a_developer_id_agent() {
    let session_port = free_session_port();
    let lab = Lab::new(
        session_port,
        &format!("wss://localhost:{session_port}/relay"),
    );
    guest_codesign(&lab, "developer-id.txt");
    let installed = guest_agent(&lab, b"the image's Developer ID build");
    let _daemon = lab.start();
    let answer = acquire(&lab);
    assert_eq!(answer["state"], "running");
    assert_eq!(answer["link"], "ssh-fallback", "{answer}");
    let reason = answer["link_reason"].as_str().unwrap();
    assert!(reason.contains("signing never downgrades"), "{reason}");
    assert!(reason.contains("DC-232"), "{reason}");
    assert!(
        !reason.contains("EXAMPLE123"),
        "a fingerprint, not the team: {reason}"
    );
    assert_eq!(
        std::fs::read(&installed).unwrap(),
        b"the image's Developer ID build"
    );
    let calls = lab.relay_calls();
    assert!(calls.contains("machine-service-relay-signature"), "{calls}");
    assert!(!calls.contains("machine-service-relay-upload"), "{calls}");
    let vm = answer["vm"].as_str().unwrap();
    let released = lab.post(&format!("/machines/{vm}/release"), &json!({}));
    assert_eq!(released.status, 200, "{}", released.body);
}

/// DC-232 allows ad hoc to ad hoc: a guest whose agent is another ad hoc
/// build gets the instance's build, and the lease runs over the relay.
#[test]
fn bootstrap_replaces_an_ad_hoc_agent_with_an_ad_hoc_build() {
    let session_port = free_session_port();
    let lab = Lab::new(
        session_port,
        &format!("wss://localhost:{session_port}/relay"),
    );
    guest_codesign(&lab, "adhoc.txt");
    let installed = guest_agent(&lab, b"an earlier ad hoc build");
    let agent = AgentProcess::start(&lab.guest_home, &[]);
    let _daemon = lab.start();
    let answer = acquire(&lab);
    assert_eq!(
        answer["link"],
        "relay",
        "{answer}\nagent log:\n{}",
        agent.log()
    );
    let calls = lab.relay_calls();
    assert!(calls.contains("machine-service-relay-signature"), "{calls}");
    assert!(calls.contains("machine-service-relay-upload"), "{calls}");
    assert_eq!(
        std::fs::read(&installed).unwrap(),
        std::fs::read(relay_agent_bin()).unwrap()
    );
    // The real agent names its own signature in its hello (protocol 3).
    if cfg!(target_os = "macos") {
        assert_eq!(answer["relay_agent"]["signing"], "adhoc", "{answer}");
    }
    let vm = answer["vm"].as_str().unwrap();
    let released = lab.post(&format!("/machines/{vm}/release"), &json!({}));
    assert_eq!(released.status, 200, "{}", released.body);
    drop(agent);
}

/// DC-232 for a lease that lives on: an agent that registers with another
/// build and says in its hello that it is signed with Developer ID is not
/// given the instance's ad hoc build; nothing is written, and the lease
/// keeps its agent.
#[test]
fn a_long_lived_lease_never_downgrades_a_developer_id_agent() {
    let session_port = free_session_port();
    let lab = Lab::new(
        session_port,
        &format!("wss://localhost:{session_port}/relay"),
    );
    let installed = guest_agent(&lab, &std::fs::read(relay_agent_bin()).unwrap());
    let agent = AgentProcess::start_from(&installed, &lab.guest_home, &[]);
    let _daemon = lab.start();
    let answer = acquire(&lab);
    assert_eq!(
        answer["link"],
        "relay",
        "{answer}\nagent log:\n{}",
        agent.log()
    );
    let vm = answer["vm"].as_str().unwrap().to_string();
    let build = answer["relay_agent"]["build"].clone();
    drop(agent);

    // The lease's agent comes back as another build, signed with Developer
    // ID by its own account, in a home of its own.
    let config: Value = serde_json::from_slice(
        &std::fs::read(lab.guest_home.join(".config/relay-agent/config.json")).unwrap(),
    )
    .unwrap();
    let home = tempfile::tempdir().unwrap();
    let home = home.path().canonicalize().unwrap();
    let mut options = relay_common::agent_options(&home);
    options.signing = Some(relay_agent::protocol::CodeSigning {
        kind: "developer-id".into(),
        team: Some("EXAMPLE123".into()),
    });
    let signed = relay_agent::Agent::open(options).unwrap();
    let dial_config = relay_agent::config::Config {
        broker_url: config["broker_url"].as_str().unwrap().to_string(),
        agent_credential: config["agent_credential"].as_str().unwrap().to_string(),
        broker_ca_pem: config["broker_ca_pem"].as_str().map(str::to_string),
    };
    let serving = signed.clone();
    std::thread::spawn(move || {
        relay_agent::dial::run(
            &serving,
            &dial_config,
            |c| {
                relay_agent::session_link::dial(
                    c,
                    &relay_agent::session_link::DialOptions {
                        timing: Some(relay_common::fast_timing()),
                        ..Default::default()
                    },
                )
            },
            |e| eprintln!("signed agent: {e}"),
        )
    });
    assert!(
        wait_until_e2e(|| service_log(&lab).contains("the relay agent was not replaced")),
        "{}",
        service_log(&lab)
    );
    let log = service_log(&lab);
    assert!(log.contains("signing never downgrades"), "{log}");
    assert!(log.contains("DC-232"), "{log}");
    assert!(!log.contains("replacing it (RA-6)"), "{log}");
    assert!(!home.join(".local/libexec/relay-agent/relay-agent").exists());
    let record = http(
        lab.fixture.port(),
        "GET",
        &format!("/machines/{vm}"),
        None,
        None,
    )
    .body;
    assert_eq!(record["relay_agent"]["build"], build, "{record}");
    // The lease keeps working through the agent it has.
    let ran = lab.post(
        &format!("/machines/{vm}/exec"),
        &json!({"argv": ["echo", "kept"]}),
    );
    assert_eq!(ran.body["output"], "kept\n", "{}", ran.body);
    let released = lab.post(&format!("/machines/{vm}/release"), &json!({}));
    assert_eq!(released.status, 200, "{}", released.body);
    signed.shutdown(Duration::from_secs(1));
}

/// DC-239: the guest's signature check runs `/usr/bin/codesign`;
/// a `codesign` that the guest's `PATH` finds first, claiming Developer ID,
/// is never run. The guest's agent reads as the unsigned file it is, so the
/// instance's build replaces it and the lease runs over the relay.
#[test]
fn a_codesign_on_the_guests_path_is_never_run() {
    let session_port = free_session_port();
    let lab = Lab::new(
        session_port,
        &format!("wss://localhost:{session_port}/relay"),
    );
    write_exec(
        &lab.fixture.bin,
        "codesign",
        &recorded_codesign("developer-id.txt"),
    );
    let installed = guest_agent(&lab, b"an earlier unsigned build");
    let agent = AgentProcess::start(&lab.guest_home, &[]);
    let _daemon = lab.start();
    let answer = acquire(&lab);
    assert_eq!(
        answer["link"],
        "relay",
        "{answer}\nagent log:\n{}",
        agent.log()
    );
    let calls = lab.relay_calls();
    assert!(calls.contains("machine-service-relay-signature"), "{calls}");
    assert!(calls.contains("machine-service-relay-upload"), "{calls}");
    assert_eq!(
        std::fs::read(&installed).unwrap(),
        std::fs::read(relay_agent_bin()).unwrap()
    );
    let vm = answer["vm"].as_str().unwrap();
    let released = lab.post(&format!("/machines/{vm}/release"), &json!({}));
    assert_eq!(released.status, 200, "{}", released.body);
    drop(agent);
}

/// The lab's release made a release, `1.0.0`, signed ad hoc: refused by
/// the trust check, because only a pre-release may be ad hoc (DC-224).
fn untrusted_release(lab: &Lab) {
    write_relay_release_of(&lab.release, "1.0.0", relay_agent_bin(), relay_agent_bin());
}

fn health(lab: &Lab) -> Value {
    http(lab.fixture.port(), "GET", "/health", None, None).body
}

/// DC-236 at startup: an instance that runs the relay link
/// without durable exposure checks its release against its trust as the
/// installer would. A release that fails is never sent: `/health` says so
/// with the reason, and every lease is served over SSH with it (RA-8),
/// before anything is read from or uploaded into the guest.
#[test]
fn an_untrusted_release_serves_every_lease_over_ssh() {
    let session_port = free_session_port();
    let lab = Lab::new(
        session_port,
        &format!("wss://localhost:{session_port}/relay"),
    );
    untrusted_release(&lab);
    let _daemon = lab.start();
    let shown = health(&lab)["relay_agent_release"].clone();
    assert_eq!(shown["trusted"], false, "{shown}");
    assert!(
        shown["reason"].as_str().unwrap().contains("DC-224"),
        "{shown}"
    );
    assert!(
        service_log(&lab).contains("WARN: relay: the relay agent release is not trusted"),
        "{}",
        service_log(&lab)
    );
    let answer = acquire(&lab);
    assert_eq!(answer["state"], "running");
    assert_eq!(answer["link"], "ssh-fallback", "{answer}");
    let reason = answer["link_reason"].as_str().unwrap();
    assert!(reason.contains("not trusted"), "{reason}");
    assert!(reason.contains("DC-224"), "{reason}");
    assert!(answer.get("relay").is_none(), "{answer}");
    let calls = lab.relay_calls();
    assert!(!calls.contains("machine-service-relay-"), "{calls}");
    let vm = answer["vm"].as_str().unwrap();
    let released = lab.post(&format!("/machines/{vm}/release"), &json!({}));
    assert_eq!(released.status, 200, "{}", released.body);
}

/// DC-236 before every upload: a release trusted at startup and
/// then replaced in place by one that fails is checked again at the next
/// acquisition, which is served over SSH, and `/health` follows.
#[test]
fn a_release_is_checked_again_before_every_upload() {
    let session_port = free_session_port();
    let lab = Lab::new(
        session_port,
        &format!("wss://localhost:{session_port}/relay"),
    );
    let _daemon = lab.start();
    let shown = health(&lab)["relay_agent_release"].clone();
    assert_eq!(
        shown,
        json!({"trusted": true, "version": "0.0.0-adhoc", "signing": "adhoc"})
    );
    untrusted_release(&lab);
    let answer = acquire(&lab);
    assert_eq!(answer["link"], "ssh-fallback", "{answer}");
    assert!(
        answer["link_reason"]
            .as_str()
            .unwrap()
            .contains("not trusted"),
        "{answer}"
    );
    assert!(!lab.relay_calls().contains("machine-service-relay-upload"));
    assert_eq!(health(&lab)["relay_agent_release"]["trusted"], false);
    let vm = answer["vm"].as_str().unwrap();
    let released = lab.post(&format!("/machines/{vm}/release"), &json!({}));
    assert_eq!(released.status, 200, "{}", released.body);
}

/// DC-236 before every replacement: an instance that restarts
/// with a release that fails its trust never writes that release's build
/// over a long-lived lease's agent; the lease keeps the agent it has.
#[test]
fn a_long_lived_lease_never_gets_an_untrusted_build() {
    let session_port = free_session_port();
    let lab = Lab::new(
        session_port,
        &format!("wss://localhost:{session_port}/relay"),
    );
    let installed = guest_agent(&lab, &std::fs::read(relay_agent_bin()).unwrap());
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

    // The instance comes back with a new Linux build in a release that
    // fails its trust.
    let held = lab.fixture.root.join("held-relay-agent");
    let mut build = std::fs::read(relay_agent_bin()).unwrap();
    build.extend_from_slice(b"an untrusted build");
    std::fs::write(&held, &build).unwrap();
    write_relay_release_of(&lab.release, "1.0.0", relay_agent_bin(), &held);
    daemon.stop();
    let _daemon = lab.start();
    assert!(
        wait_until_e2e(|| service_log(&lab).contains("the relay agent is not replaced")),
        "{}\nagent log:\n{}",
        service_log(&lab),
        agent.log()
    );
    let log = service_log(&lab);
    assert!(log.contains("not trusted"), "{log}");
    assert!(!log.contains("replacing it (RA-6)"), "{log}");
    assert_eq!(
        std::fs::read(&installed).unwrap(),
        std::fs::read(relay_agent_bin()).unwrap()
    );
    let ran = lab.post(
        &format!("/machines/{vm}/exec"),
        &json!({"argv": ["echo", "kept"]}),
    );
    assert_eq!(ran.body["output"], "kept\n", "{}", ran.body);
    let released = lab.post(&format!("/machines/{vm}/release"), &json!({}));
    assert_eq!(released.status, 200, "{}", released.body);
    drop(agent);
}
