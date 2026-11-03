//! Live conformance of the guest session link (`docs/arch/test-layers.md`,
//! TL-4; AC-2, AC-3, AC-12, AC-13, AC-15, AC-18).
//!
//! Each test leases real VMs from two second instances built from this
//! worktree, one per guest link, and never from the installed daemon:
//!
//! ```sh
//! scripts/build-guest-agents.sh target/live
//! cp target/release/machine-service target/release/console-worker \
//!   target/release/second-instance target/live/
//! export LIVE_TART_HOME=<store>
//! LIVE_PORT=6250 LIVE_BROKER_PORT=6260 scripts/live-instance.sh start ssh-session
//! LIVE_PORT=6251 LIVE_BROKER_PORT=6261 scripts/live-instance.sh start ssh
//! cargo test -p machine-service --test live_guest_session -- --ignored --test-threads=1
//! ```
//!
//! `LIVE_SESSION_PORT`, `LIVE_SSH_PORT` and `LIVE_IMAGE` (default
//! `ubuntu2404`) select the instances and the image. Each test writes its
//! answers and timings to a new directory under `test-evidence/live/`.
//! One test at a time keeps at most two leases on the host.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(900);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);
const INSTALLED_PORT: u16 = 6240;

fn port(variable: &str, default: u16) -> u16 {
    std::env::var(variable)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn image() -> String {
    std::env::var("LIVE_IMAGE").unwrap_or_else(|_| "ubuntu2404".to_string())
}

/// The two instances under test, by guest link.
fn links() -> [(&'static str, u16); 2] {
    [
        ("ssh", port("LIVE_SSH_PORT", 6251)),
        ("ssh-session", port("LIVE_SESSION_PORT", 6250)),
    ]
}

/// The instance on `port` must answer and run guest link `link`.
fn require_instance(link: &str, port: u16) {
    let health = common::try_http(port, "GET", "/health", None, None);
    assert!(
        matches!(&health, Ok(response) if response.status == 200),
        "no machine-service on port {port}; start it with scripts/live-instance.sh"
    );
    // Only a second instance, never the installed daemon (DC-1xx-LIVE-3).
    if let Err(refused) = second_instance::guard(port, &health.as_ref().unwrap().body) {
        panic!("{refused}");
    }
    let log = repository()
        .join(format!("rust/target/live/{port}/state/service.log"))
        .canonicalize()
        .ok()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default();
    let started = log
        .lines()
        .rev()
        .find(|line| line.contains("machine-service starting on"))
        .unwrap_or_default();
    assert!(
        started.contains(&format!("guest link: {link})")),
        "the instance on port {port} does not run guest link {link}: {started}"
    );
}

fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

/// A new evidence directory for one test run.
fn evidence(test: &str) -> PathBuf {
    let stamp = Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H-%M-%SZ"])
        .output()
        .expect("date");
    let stamp = String::from_utf8_lossy(&stamp.stdout).trim().to_string();
    let directory = repository().join(format!("test-evidence/live/{stamp}-{test}"));
    std::fs::create_dir_all(&directory).expect("evidence directory");
    directory
}

fn write_report(directory: &Path, report: &Value) {
    std::fs::write(
        directory.join("report.json"),
        serde_json::to_string_pretty(report).expect("report json"),
    )
    .expect("report");
}

// ------------------------------------------------------------------ AC-18

/// What the installed daemon looked like before a test.
struct Installed {
    pid: Option<String>,
}

impl Installed {
    fn snapshot() -> Self {
        Self {
            pid: listener_pid(INSTALLED_PORT),
        }
    }

    /// The installed daemon kept its process and holds no test lease.
    fn check(&self) {
        assert_eq!(
            listener_pid(INSTALLED_PORT),
            self.pid,
            "the installed daemon's process changed"
        );
        let state = std::env::var_os("HOME")
            .map(PathBuf::from)
            .expect("HOME")
            .join(".local/state/machine-service/state.json");
        if let Ok(text) = std::fs::read_to_string(state) {
            let state: Value = serde_json::from_str(&text).expect("installed state");
            for record in state["vms"]
                .as_object()
                .into_iter()
                .flat_map(|vms| vms.values())
            {
                let purpose = record["purpose"].as_str().unwrap_or_default();
                assert!(
                    !purpose.starts_with("st-live"),
                    "a test lease reached the installed daemon: {record}"
                );
            }
        }
    }
}

fn listener_pid(port: u16) -> Option<String> {
    let output = Command::new("lsof")
        .args(["-t", &format!("-iTCP:{port}"), "-sTCP:LISTEN"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

// ------------------------------------------------------------------ leases

/// A lease on one instance, released on drop.
struct Lease {
    link: &'static str,
    port: u16,
    vm: String,
    ip: String,
}

impl Lease {
    fn acquire(link: &'static str, port: u16, purpose: &str) -> Lease {
        require_instance(link, port);
        let response = common::http_timeout(
            port,
            "POST",
            "/acquire",
            Some(&json!({
                "purpose": purpose,
                "image": image(),
                "ttl_hours": 1,
                "cpu": 4,
                "memory_mb": 4096,
            })),
            None,
            ACQUIRE_TIMEOUT,
        );
        assert_eq!(response.status, 200, "acquire on {link}: {}", response.body);
        Lease {
            link,
            port,
            vm: response.body["vm"].as_str().expect("vm").to_string(),
            ip: response.body["ip"].as_str().unwrap_or_default().to_string(),
        }
    }

    /// One lease per link, acquired at the same time.
    fn pair(purpose: &str) -> (Lease, Lease) {
        let [(ssh, ssh_port), (session, session_port)] = links();
        let purpose_ssh = purpose.to_string();
        let by_ssh = std::thread::spawn(move || Lease::acquire(ssh, ssh_port, &purpose_ssh));
        let by_session = Lease::acquire(session, session_port, purpose);
        (by_ssh.join().expect("ssh lease"), by_session)
    }

    fn post(&self, route: &str, body: &Value) -> (common::Response, Duration) {
        let started = Instant::now();
        let response = common::http_timeout(
            self.port,
            "POST",
            &format!("/machines/{}/{route}", self.vm),
            Some(body),
            None,
            REQUEST_TIMEOUT,
        );
        (response, started.elapsed())
    }

    fn ok(&self, route: &str, body: Value) -> Value {
        let (response, _) = self.post(route, &body);
        assert_eq!(
            response.status, 200,
            "{} {route} {body}: {}",
            self.link, response.body
        );
        response.body
    }

    fn release(&self) -> common::Response {
        common::http_timeout(
            self.port,
            "POST",
            &format!("/machines/{}/release", self.vm),
            Some(&json!({"reason": "live-test"})),
            None,
            REQUEST_TIMEOUT,
        )
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

// ------------------------------------------------------------------- AC-2

/// The parts of an answer both links must agree on.
fn comparable(response: &common::Response, scratch: &Path) -> Value {
    let mut body = response.body.clone();
    if let Some(object) = body.as_object_mut() {
        for key in ["bytes", "sha256", "vm", "pushed", "to", "pulled"] {
            object.remove(key);
        }
    }
    let text = body
        .to_string()
        .replace(&scratch.display().to_string(), "<scratch>");
    json!({"status": response.status, "body": serde_json::from_str::<Value>(&text).expect("json")})
}

fn parity_requests(scratch: &Path) -> Vec<(&'static str, &'static str, Value)> {
    vec![
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
            "home",
            "exec",
            json!({"script": "echo $HOME; pwd; echo $SHELL"}),
        ),
        (
            "locale",
            "exec",
            json!({"script": "locale 2>/dev/null | head -3"}),
        ),
        (
            "push-dir",
            "push",
            json!({"local_path": scratch.join("tree"), "remote_path": "st-tree"}),
        ),
        (
            "pushed-tree",
            "exec",
            json!({"argv": ["cat", "st-tree/sub/leaf"]}),
        ),
        (
            "push",
            "push",
            json!({"local_path": scratch.join("text.txt"), "remote_path": "st-text.txt"}),
        ),
        (
            "pull",
            "pull",
            json!({"local_path": scratch.join("pulled.txt"), "remote_path": "st-text.txt"}),
        ),
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
            "pushed-content",
            "exec",
            json!({"argv": ["cat", "st-text.txt"]}),
        ),
        (
            "pull-missing",
            "pull",
            json!({"local_path": scratch.join("none.txt"), "remote_path": "no-such-file"}),
        ),
    ]
}

#[test]
#[ignore = "live: leases two VMs from the second instances on ports 6250 (ssh-session) and 6251 (ssh); see the module documentation"]
fn live_the_session_link_answers_like_the_ssh_link() {
    let installed = Installed::snapshot();
    let directory = evidence("parity");
    let (by_ssh, by_session) = Lease::pair("st-live-parity");
    let mut answers = serde_json::Map::new();
    let mut compared: Vec<Vec<(String, Value)>> = Vec::new();
    for lease in [&by_ssh, &by_session] {
        let scratch = directory.join(lease.link);
        std::fs::create_dir_all(scratch.join("tree/sub")).expect("tree");
        std::fs::write(scratch.join("tree/sub/leaf"), "leaf").expect("leaf");
        std::fs::write(scratch.join("text.txt"), "héllo\nworld\n").expect("text");
        let mut rows = Vec::new();
        let mut recorded = Vec::new();
        for (name, route, body) in parity_requests(&scratch) {
            let (response, elapsed) = lease.post(route, &body);
            recorded.push(json!({
                "name": name, "status": response.status, "body": response.body,
                "ms": millis(elapsed),
            }));
            rows.push((name.to_string(), comparable(&response, &scratch)));
        }
        rows.push((
            "pulled-content".into(),
            json!(std::fs::read_to_string(scratch.join("pulled.txt")).ok()),
        ));
        answers.insert(lease.link.to_string(), Value::Array(recorded));
        compared.push(rows);
    }
    write_report(&directory, &json!({"image": image(), "answers": answers}));

    let (ssh, session) = (&compared[0], &compared[1]);
    let by_name = |name: &str| &session.iter().find(|(n, _)| n == name).expect(name).1;
    assert_eq!(by_name("pulled-content"), &json!("héllo\nworld\n"));
    assert_eq!(by_name("pushed-tree")["body"]["output"], "leaf");
    for ((name, by_ssh), (_, by_session)) in ssh.iter().zip(session) {
        match name.as_str() {
            "mixed" => {
                // The session keeps the order the lines were written in,
                // where `ssh` gives all standard output first (GS-6).
                assert_eq!(by_session["body"]["output"], "a\nb\nc\n");
                assert_eq!(by_ssh["body"]["output"], "a\nc\nb\n");
            }
            "pull-missing" | "timeout" => {
                // Both refuse with the same status; the messages differ (GS-6).
                assert_eq!(by_ssh["status"], 409, "{name} {by_ssh}");
                assert_eq!(by_session["status"], 409, "{name} {by_session}");
                for field in ["rc", "output"] {
                    assert_eq!(by_ssh["body"][field], by_session["body"][field], "{name}");
                }
            }
            _ => assert_eq!(by_ssh, by_session, "{name}"),
        }
    }
    drop((by_ssh, by_session));
    installed.check();
}

// ------------------------------------------------------------------- AC-3

#[test]
#[ignore = "live: leases two VMs from the second instances on ports 6250 (ssh-session) and 6251 (ssh); see the module documentation"]
fn live_64_mib_crosses_each_way_intact() {
    let installed = Installed::snapshot();
    let directory = evidence("transfer");
    let (by_ssh, by_session) = Lease::pair("st-live-transfer");
    let size: usize = 64 * 1024 * 1024;
    let mut payload = vec![0u8; size];
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    for chunk in payload.chunks_mut(8) {
        // xorshift: incompressible bytes without a random-number crate.
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        chunk.copy_from_slice(&state.to_le_bytes()[..chunk.len()]);
    }
    // The payload and its copies stay out of the evidence directory.
    let scratch = tempfile::TempDir::new().expect("scratch");
    let local = scratch.path().join("payload.bin");
    std::fs::write(&local, &payload).expect("payload");
    let digest = common::sha256_hex(&payload);
    let mut report = serde_json::Map::new();
    for lease in [&by_ssh, &by_session] {
        let back = scratch.path().join(format!("back-{}.bin", lease.link));
        let (pushed, push_time) = lease.post(
            "push",
            &json!({"local_path": local, "remote_path": "st-payload.bin"}),
        );
        assert_eq!(pushed.status, 200, "{} push: {}", lease.link, pushed.body);
        let guest_digest = lease.ok("exec", json!({"argv": ["sha256sum", "st-payload.bin"]}));
        assert!(
            guest_digest["output"]
                .as_str()
                .unwrap_or_default()
                .starts_with(&digest),
            "{} guest digest: {guest_digest}",
            lease.link
        );
        let (pulled, pull_time) = lease.post(
            "pull",
            &json!({"local_path": back, "remote_path": "st-payload.bin"}),
        );
        assert_eq!(pulled.status, 200, "{} pull: {}", lease.link, pulled.body);
        let returned = std::fs::read(&back).expect("pulled");
        assert_eq!(returned.len(), size, "{}", lease.link);
        assert_eq!(common::sha256_hex(&returned), digest, "{}", lease.link);
        std::fs::remove_file(&back).expect("remove pulled copy");
        // An exec answers with the last 64000 characters of its output, on
        // both links; the whole 64 MiB still crosses the link.
        let (output, output_time) = lease.post(
            "exec",
            &json!({"script": "head -c 67108864 /dev/zero | tr '\\0' a; echo END", "timeout": 300}),
        );
        let text = output.body["output"].as_str().unwrap_or_default();
        let tail: String = text
            .chars()
            .rev()
            .take(80)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let expected = format!("{}END\n", "a".repeat(64000 - 4));
        let output_matches = output.status == 200 && text == expected;
        report.insert(
            format!("{}-output", lease.link),
            json!({"status": output.status, "chars": text.chars().count(), "tail": tail,
                   "rc": output.body["rc"], "error": output.body["error"]}),
        );
        write_report(
            &directory,
            &json!({"image": image(), "bytes": size, "sha256": digest, "links": report}),
        );
        assert!(
            output_matches,
            "{} output: status {} with {} characters ending {tail:?}",
            lease.link,
            output.status,
            text.chars().count()
        );
        report.insert(
            lease.link.to_string(),
            json!({
                "push_ms": millis(push_time),
                "pull_ms": millis(pull_time),
                "output_ms": millis(output_time),
                "push_answer": pushed.body,
                "pull_answer": pulled.body,
            }),
        );
    }
    write_report(
        &directory,
        &json!({"image": image(), "bytes": size, "sha256": digest, "links": report}),
    );
    drop((by_ssh, by_session));
    installed.check();
}

// ------------------------------------------------------------------ AC-15

/// Mean, median and 95th percentile, in milliseconds.
fn summary(samples: &mut [f64]) -> Value {
    samples.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
    let at = |share: f64| samples[((samples.len() - 1) as f64 * share).round() as usize];
    json!({
        "n": samples.len(),
        "mean": samples.iter().sum::<f64>() / samples.len() as f64,
        "p50": at(0.5),
        "p95": at(0.95),
        "max": samples[samples.len() - 1],
    })
}

#[test]
#[ignore = "live: leases two VMs from the second instances on ports 6250 (ssh-session) and 6251 (ssh); see the module documentation"]
fn live_round_trip_time_per_operation() {
    let installed = Installed::snapshot();
    let directory = evidence("round-trip");
    let (by_ssh, by_session) = Lease::pair("st-live-timing");
    let small = directory.join("small.bin");
    let large = directory.join("large.bin");
    std::fs::write(&small, vec![7u8; 1024]).expect("small");
    std::fs::write(&large, vec![7u8; 1024 * 1024]).expect("large");
    let operations = [
        ("exec-true", "exec", json!({"argv": ["/usr/bin/true"]})),
        ("script-true", "exec", json!({"script": "true"})),
        (
            "exec-node",
            "exec",
            json!({"script": "command -v node >/dev/null && node -e 0 || true"}),
        ),
        (
            "push-1KiB",
            "push",
            json!({"local_path": small, "remote_path": "st-small.bin"}),
        ),
        (
            "pull-1KiB",
            "pull",
            json!({"local_path": directory.join("small-back.bin"), "remote_path": "st-small.bin"}),
        ),
        (
            "push-1MiB",
            "push",
            json!({"local_path": large, "remote_path": "st-large.bin"}),
        ),
    ];
    let rounds = 30;
    let mut report = serde_json::Map::new();
    for lease in [&by_ssh, &by_session] {
        // The first request opens the session and may upload the agent.
        let (_, first) = lease.post("exec", &json!({"argv": ["/usr/bin/true"]}));
        let mut link = serde_json::Map::new();
        link.insert("first_request_ms".into(), json!(millis(first)));
        for (name, route, body) in &operations {
            let mut samples = Vec::new();
            for _ in 0..rounds {
                let (response, elapsed) = lease.post(route, body);
                assert_eq!(
                    response.status, 200,
                    "{} {name}: {}",
                    lease.link, response.body
                );
                samples.push(millis(elapsed));
            }
            link.insert(name.to_string(), summary(&mut samples));
        }
        report.insert(lease.link.to_string(), Value::Object(link));
    }
    let report = json!({"image": image(), "rounds": rounds, "links": report});
    write_report(&directory, &report);
    println!("{}", serde_json::to_string_pretty(&report).expect("json"));
    drop((by_ssh, by_session));
    installed.check();
}

// ------------------------------------------------------------ AC-12, AC-13

/// Host processes whose command line names the guest session agent and `ip`.
fn host_session_channels(ip: &str) -> Vec<String> {
    let output = Command::new("pgrep")
        .args(["-fl", ".vm-service-session/agent"])
        .output()
        .expect("pgrep");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.contains(ip))
        .map(str::to_string)
        .collect()
}

#[test]
#[ignore = "live: leases a VM from the second instance on port 6250 (ssh-session); see the module documentation"]
fn live_release_cancels_a_running_command_and_closes_the_session() {
    let installed = Installed::snapshot();
    let directory = evidence("release");
    let (link, port) = links()[1];
    let lease = Lease::acquire(link, port, "st-live-release");
    let agents = lease.ok(
        "exec",
        json!({"script": "pgrep -fc '.vm-service-session/agent serve'"}),
    );
    assert_eq!(agents["output"], "1\n", "one agent before release");
    assert!(
        !host_session_channels(&lease.ip).is_empty(),
        "no host channel for {}",
        lease.ip
    );
    let running_port = lease.port;
    let vm = lease.vm.clone();
    let running = std::thread::spawn(move || {
        let started = Instant::now();
        let response = common::http_timeout(
            running_port,
            "POST",
            &format!("/machines/{vm}/exec"),
            Some(&json!({"argv": ["sleep", "600"], "timeout": 900})),
            None,
            REQUEST_TIMEOUT,
        );
        (response, started.elapsed())
    });
    std::thread::sleep(Duration::from_secs(3));
    let released_at = Instant::now();
    let released = lease.release();
    let release_time = released_at.elapsed();
    let (cancelled, ran_for) = running.join().expect("running exec");
    write_report(
        &directory,
        &json!({
            "image": image(),
            "release": {"status": released.status, "body": released.body, "ms": millis(release_time)},
            "exec": {"status": cancelled.status, "body": cancelled.body, "ms": millis(ran_for)},
        }),
    );
    assert_eq!(released.status, 200, "release: {}", released.body);
    assert_ne!(
        cancelled.status, 200,
        "the exec ended normally: {}",
        cancelled.body
    );
    assert!(
        cancelled.body.to_string().contains("cancelled by release"),
        "{}",
        cancelled.body
    );
    assert!(
        ran_for < Duration::from_secs(60),
        "the exec ran {ran_for:?}"
    );
    assert_eq!(host_session_channels(&lease.ip), Vec::<String>::new());
    drop(lease);
    installed.check();
}

// ------------------------------------------------------------------- TL-4

#[test]
#[ignore = "live: leases two VMs from the second instances on ports 6250 (ssh-session) and 6251 (ssh) and turns their network off for 30 s; see the module documentation"]
fn live_a_request_while_the_guest_network_is_off_fails_by_the_silence_limit() {
    let installed = Installed::snapshot();
    let directory = evidence("network-off");
    let (by_ssh, by_session) = Lease::pair("st-live-network");
    let outage = 30;
    // AC-19: 5 missed 2 s heartbeats, plus the watchdog's check and a margin.
    let silence = Duration::from_secs(15);
    let mut report = serde_json::Map::new();
    for lease in [&by_ssh, &by_session] {
        lease.ok("exec", json!({"argv": ["sudo", "-n", "true"]}));
        let script = format!(
            "iface=$(ip -o route show default | awk '{{print $5}}'); \
             sudo -n nohup sh -c \"sleep 2; ip link set $iface down; sleep {outage}; \
             ip link set $iface up\" >/dev/null 2>&1 &"
        );
        lease.ok("exec", json!({"script": script}));
        std::thread::sleep(Duration::from_secs(4));
        let off_at = Instant::now();
        // The request's own timeout is far past the outage, so only the
        // link's own failure detection can end it early.
        let (during, during_time) =
            lease.post("exec", &json!({"argv": ["echo", "during"], "timeout": 120}));
        // Past the outage: the next request must work again.
        let wait = Duration::from_secs(outage + 8).saturating_sub(off_at.elapsed());
        std::thread::sleep(wait);
        let (after, after_time) =
            lease.post("exec", &json!({"argv": ["echo", "after"], "timeout": 60}));
        report.insert(
            lease.link.to_string(),
            json!({
                "during": {"status": during.status, "body": during.body, "ms": millis(during_time)},
                "after": {"status": after.status, "body": after.body, "ms": millis(after_time)},
            }),
        );
        write_report(
            &directory,
            &json!({"image": image(), "outage_s": outage, "links": report}),
        );
        if lease.link == "ssh-session" {
            assert_ne!(during.status, 200, "{}: {}", lease.link, during.body);
            assert_eq!(during.body["outcome"], "uncertain", "{}", during.body);
            assert!(
                during.body["error"]
                    .as_str()
                    .is_some_and(|error| error.contains("nothing from the agent")),
                "{}",
                during.body
            );
            assert!(
                during_time < silence,
                "a request on a dead link took {during_time:?}"
            );
        } else {
            // The ssh link opens a new connection per request: its connect
            // times out and `ssh` exits 255, or it ends by the request's
            // timeout, or it answers once the network is back.
            let connect_failed = during.status == 200
                && during.body["rc"] == 255
                && during.body["output"]
                    .as_str()
                    .is_some_and(|output| output.starts_with("ssh: connect to host"));
            assert!(
                during.status != 200 || connect_failed || during.body["output"] == "during\n",
                "{}",
                during.body
            );
        }
        assert_eq!(after.status, 200, "{}: {}", lease.link, after.body);
        assert_eq!(after.body["output"], "after\n", "{}", lease.link);
    }
    drop((by_ssh, by_session));
    installed.check();
}
