//! TL-2: the real `lab-host` against a fake GitHub and a fake lab instance
//! on loopback, through the test build's seams (LR-3, DC-251). Nothing here
//! reaches GitHub, an instance, the keychain or the account's home.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const LAB_HOST: &str = env!("CARGO_BIN_EXE_lab-host");
const REPO: &str = "/repos/WeZZard/vm-service";

/// One request a fake answered.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    url: String,
    body: Vec<u8>,
    headers: BTreeMap<String, String>,
}

type Handler = dyn Fn(&Seen) -> (u16, Value) + Send + Sync;

/// A loopback HTTP fake that records every request.
struct Fake {
    url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Fake {
    fn start(handler: Arc<Handler>) -> Fake {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("bind loopback");
        let url = format!(
            "http://{}",
            server.server_addr().to_ip().expect("ip address")
        );
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&seen);
        std::thread::spawn(move || {
            for mut request in server.incoming_requests() {
                let mut body = Vec::new();
                let _ = request.as_reader().read_to_end(&mut body);
                let entry = Seen {
                    method: request.method().to_string(),
                    url: request.url().to_string(),
                    body,
                    headers: request
                        .headers()
                        .iter()
                        .map(|header| {
                            (
                                header.field.as_str().as_str().to_ascii_lowercase(),
                                header.value.as_str().to_string(),
                            )
                        })
                        .collect(),
                };
                let (status, answer) = handler(&entry);
                record.lock().unwrap().push(entry);
                let _ = request.respond(
                    tiny_http::Response::from_string(answer.to_string())
                        .with_status_code(status)
                        .with_header(
                            "Content-Type: application/json"
                                .parse::<tiny_http::Header>()
                                .unwrap(),
                        ),
                );
            }
        });
        Fake { url, seen }
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    fn count(&self, method: &str, fragment: &str) -> usize {
        self.seen()
            .iter()
            .filter(|seen| seen.method == method && seen.url.contains(fragment))
            .count()
    }
}

/// GitHub with one queued lab job, which leaves the queue once its JIT
/// config is made, and a runner that is busy until it is deleted.
fn github(labels: Value, extra_runs: bool) -> (Fake, Arc<Mutex<bool>>) {
    let jit_made = Arc::new(Mutex::new(false));
    let made = Arc::clone(&jit_made);
    let handler: Arc<Handler> = Arc::new(move |seen: &Seen| {
        let made_now = *made.lock().unwrap();
        let url = seen.url.as_str();
        if url == "/meta" {
            return (
                200,
                json!({
                    "web": ["140.82.112.0/20"], "api": ["140.82.112.0/20"], "git": [], "packages": [],
                    "domains": {"actions_inbound": {"full_domains": ["github.com", "results-receiver.actions.githubusercontent.com"]}}
                }),
            );
        }
        if seen.headers.get("authorization").map(String::as_str) != Some("Bearer test-token-value")
        {
            return (401, json!({"message": "Bad credentials"}));
        }
        if url.starts_with(&format!("{REPO}/actions/runs?")) {
            let runs = if made_now && !extra_runs {
                json!([])
            } else {
                json!([{"id": 100}])
            };
            return (200, json!({"workflow_runs": runs}));
        }
        if url.starts_with(&format!("{REPO}/actions/runs/100/jobs")) {
            let status = if made_now { "in_progress" } else { "queued" };
            return (
                200,
                json!({"jobs": [{"id": 7, "status": status, "labels": labels.clone()}]}),
            );
        }
        if url.starts_with(&format!("{REPO}/actions/runners/generate-jitconfig")) {
            *made.lock().unwrap() = true;
            return (
                201,
                json!({"runner": {"id": 55}, "encoded_jit_config": "ZW5jb2RlZC1qaXQ="}),
            );
        }
        if url.starts_with(&format!("{REPO}/actions/runners/55")) && seen.method == "DELETE" {
            return (204, Value::Null);
        }
        if url.starts_with(&format!("{REPO}/actions/runners")) {
            let runners = if made_now {
                json!([{"id": 55, "name": "lab-9f86d081-7", "status": "online", "busy": true, "labels": [{"name": "self-hosted"}, {"name": "lab-ubuntu"}]}])
            } else {
                json!([])
            };
            return (200, json!({"runners": runners}));
        }
        (404, json!({"message": "Not Found"}))
    });
    (Fake::start(handler), jit_made)
}

/// A lab instance with `taken` macOS slots and `leases`.
fn instance(taken: u32, leases: Value) -> Fake {
    let handler: Arc<Handler> = Arc::new(move |seen: &Seen| {
        let url = seen.url.as_str();
        match (seen.method.as_str(), url) {
            ("GET", "/images") => (
                200,
                json!({"capacity": {"macos_limit": 2, "macos_taken": taken, "foreign_macos_guests": 0}}),
            ),
            ("GET", "/machines") => (200, json!({"machines": leases.clone()})),
            ("POST", "/acquire") => {
                let body: Value = serde_json::from_slice(&seen.body).unwrap_or(Value::Null);
                (
                    200,
                    json!({"vm": "si-lab-runner-1", "lease_id": "L1", "purpose": body["purpose"]}),
                )
            }
            ("PUT", url) if url.starts_with("/machines/si-lab-runner-1/files?") => {
                let digest: String = Sha256::digest(&seen.body)
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect();
                if seen.headers.get("x-content-sha256") != Some(&digest) {
                    return (400, json!({"error": "digest"}));
                }
                (200, json!({"ok": true}))
            }
            ("POST", "/machines/si-lab-runner-1/exec") => (
                200,
                json!({"rc": 0, "output": "lab-runner: starting runner"}),
            ),
            ("POST", url) if url.ends_with("/release") => (200, json!({"released": true})),
            _ => (404, json!({"error": "not found"})),
        }
    });
    Fake::start(handler)
}

struct Run {
    _folder: tempfile::TempDir,
    root: PathBuf,
    stderr: String,
    status: std::process::ExitStatus,
}

fn run_lab_host(github: &Fake, instance: &Fake, token: Option<&str>) -> Run {
    let folder = tempfile::tempdir().unwrap();
    let root = folder.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("egress")).unwrap();
    let archive = root.join("runner.tar.gz");
    std::fs::write(&archive, b"a runner archive").unwrap();
    let sha: String = Sha256::digest(b"a runner archive")
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let mut seams = json!({
        "github": github.url,
        "instance": instance.url,
        "state": root.join("state"),
        "egress": root.join("egress"),
        "dns": {"github.com": ["140.82.112.3"], "results-receiver.actions.githubusercontent.com": ["20.42.73.1"]},
        "runners": {"osx-arm64": {"path": archive, "sha256": sha}, "linux-arm64": {"path": archive, "sha256": sha}},
        "endpoints": [{"name": "mac-a", "address": "192.0.2.3", "own": true}, {"name": "mac-b", "address": "192.0.2.4"}],
        "rounds": 3,
        "interval_s": 0
    });
    if let Some(token) = token {
        std::fs::write(root.join("token"), token).unwrap();
        seams["token"] = json!(root.join("token"));
    }
    std::fs::write(root.join("seams.json"), seams.to_string()).unwrap();
    let output = service_layout::test_build::harness::command(LAB_HOST)
        .arg("run")
        .env("HOME", &root)
        .env("LAB_HOST_TEST_SEAMS", root.join("seams.json"))
        .output()
        .expect("run lab-host");
    Run {
        root,
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        status: output.status,
        _folder: folder,
    }
}

fn put_paths(instance: &Fake) -> Vec<String> {
    instance
        .seen()
        .iter()
        .filter(|seen| seen.method == "PUT")
        .filter_map(|seen| seen.url.split("path=").nth(1).map(str::to_string))
        .collect()
}

#[test]
fn one_queued_job_gets_one_runner_vm_one_runner_and_one_release() {
    let (github, _) = github(json!(["self-hosted", "lab-ubuntu"]), false);
    let instance = instance(0, json!({}));
    let run = run_lab_host(&github, &instance, Some("test-token-value\n"));
    assert!(run.status.success(), "{}", run.stderr);

    // One egress acquisition, for the job, with no credential pack.
    let acquisitions: Vec<Value> = instance
        .seen()
        .iter()
        .filter(|seen| seen.url == "/acquire")
        .map(|seen| serde_json::from_slice(&seen.body).unwrap())
        .collect();
    assert_eq!(acquisitions.len(), 1, "{}", run.stderr);
    assert_eq!(acquisitions[0]["purpose"], "lab-7");
    assert_eq!(acquisitions[0]["image"], "ubuntu2404");
    assert_eq!(acquisitions[0]["network"], "internet");
    assert_eq!(acquisitions[0]["egress"], "allowlist");
    assert_eq!(acquisitions[0]["env"], "none");

    // The runner, its plan, the lab hosts' names and the JIT config.
    let paths = put_paths(&instance);
    for name in ["runner.tar.gz", "lab-plan.json", "hosts", "jit"] {
        assert!(
            paths.iter().any(|path| path.ends_with(name)),
            "{name} not placed: {paths:?}"
        );
    }
    let plan = instance
        .seen()
        .into_iter()
        .find(|seen| seen.url.contains("lab-plan.json"))
        .map(|seen| serde_json::from_slice::<Value>(&seen.body).unwrap())
        .unwrap();
    assert_eq!(plan["vm_target"]["name"], "mac-a");
    assert_eq!(plan["job"], 7);

    // Exactly the job's labels, and the config never in an exec or a log.
    let jit = github
        .seen()
        .into_iter()
        .find(|seen| seen.url.contains("generate-jitconfig"))
        .unwrap();
    let body: Value = serde_json::from_slice(&jit.body).unwrap();
    assert_eq!(body["labels"], json!(["self-hosted", "lab-ubuntu"]));
    let tag: String = Sha256::digest(b"test")
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(body["name"], format!("lab-{}-7", &tag[..8]));
    let exec = instance
        .seen()
        .into_iter()
        .find(|seen| seen.url.ends_with("/exec"))
        .unwrap();
    let exec_body = String::from_utf8_lossy(&exec.body);
    assert!(
        !exec_body.contains("ZW5jb2RlZC1qaXQ="),
        "the JIT config went into an exec"
    );
    assert!(exec_body.contains("ACTIONS_RUNNER_INPUT_JITCONFIG"));
    assert!(!run.stderr.contains("ZW5jb2RlZC1qaXQ="));
    assert!(
        !run.stderr.contains("test-token-value"),
        "the token reached the log"
    );

    // Released, and its registration removed; nothing left in its job files.
    assert_eq!(
        instance.count("POST", "/machines/si-lab-runner-1/release"),
        1
    );
    assert_eq!(github.count("DELETE", "/actions/runners/55"), 1);
    assert_eq!(
        std::fs::read_dir(run.root.join("state/jobs"))
            .map(|d| d.count())
            .unwrap_or(0),
        0
    );

    // The egress allowlist names its check name and holds the resolved names.
    let allow = std::fs::read_to_string(run.root.join("egress/egress-allow")).unwrap();
    assert_eq!(
        guest_firewall_check(&allow).as_deref(),
        Some("api.github.com")
    );
    for entry in [
        "140.82.112.0/20",
        "140.82.112.3",
        "20.42.73.1",
        "1.1.1.1",
        "8.8.8.8",
    ] {
        assert!(
            allow.lines().any(|line| line == entry),
            "{entry} missing: {allow}"
        );
    }
    let lab = std::fs::read_to_string(run.root.join("egress/egress-lab")).unwrap();
    assert!(
        lab.lines().any(|line| line == "192.0.2.3") && lab.lines().any(|line| line == "192.0.2.4")
    );
}

fn guest_firewall_check(text: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.strip_prefix("# check: "))
        .map(str::to_string)
}

#[test]
fn a_job_without_room_stays_queued() {
    let (github, _) = github(json!(["self-hosted", "lab-macos"]), true);
    let instance = instance(2, json!({}));
    let run = run_lab_host(&github, &instance, Some("test-token-value"));
    assert!(run.status.success(), "{}", run.stderr);
    assert_eq!(instance.count("POST", "/acquire"), 0, "{}", run.stderr);
    assert_eq!(github.count("POST", "generate-jitconfig"), 0);
}

#[test]
fn without_the_token_nothing_is_served_and_the_log_says_how_to_store_it() {
    let (github, _) = github(json!(["self-hosted", "lab-ubuntu"]), true);
    let instance = instance(0, json!({}));
    let run = run_lab_host(&github, &instance, None);
    assert!(run.status.success(), "{}", run.stderr);
    assert_eq!(instance.count("POST", "/acquire"), 0);
    assert!(
        run.stderr.contains("security add-generic-password -U -a WeZZard/vm-service -s com.wezzard.vm-service.lab.github-token -w"),
        "{}",
        run.stderr
    );
    // Said once, not every round.
    assert_eq!(
        run.stderr.matches("serving no job").count(),
        1,
        "{}",
        run.stderr
    );
    assert_eq!(
        github
            .seen()
            .iter()
            .filter(|seen| seen.url.starts_with(REPO))
            .count(),
        0
    );
}

#[test]
fn a_runner_vm_an_earlier_lab_host_left_is_released_at_start() {
    let (github, _) = github(json!(["ubuntu-latest"]), true);
    let leases = json!({
        "si-lab-runner-old": {"vm": "si-lab-runner-old", "lease_id": "x", "purpose": "lab-99"},
        "si-lab-target": {"vm": "si-lab-target", "lease_id": "y", "purpose": "acc-macos-client-macos-vm-n0-5"}
    });
    let instance = instance(0, leases);
    let run = run_lab_host(&github, &instance, Some("test-token-value"));
    assert!(run.status.success(), "{}", run.stderr);
    assert_eq!(
        instance.count("POST", "/machines/si-lab-runner-old/release"),
        1,
        "{}",
        run.stderr
    );
    assert_eq!(
        instance.count("POST", "/machines/si-lab-target/release"),
        0,
        "a job's own target is the job's"
    );
    // A job that is not a lab job is never served.
    assert_eq!(instance.count("POST", "/acquire"), 0);
}

#[test]
fn a_test_build_without_seams_reaches_nothing() {
    let output = service_layout::test_build::harness::command(LAB_HOST)
        .arg("run")
        .env("HOME", Path::new("/nonexistent"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("LAB_HOST_TEST_SEAMS"));
}
