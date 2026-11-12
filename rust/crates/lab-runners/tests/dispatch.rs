//! TL-2: the real `lab-host` against a fake GitHub, a fake production and
//! fake test environments on loopback, through the test build's seams
//! (LR-3, LR-6, LR-10, DC-251). A run's build is a real zip of a real
//! gzipped tar whose `second-instance` is a fake launcher that records its
//! arguments. Nothing here reaches GitHub, an instance, the keychain or the
//! account's home.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const LAB_HOST: &str = env!("CARGO_BIN_EXE_lab-host");
const REPO: &str = "/repos/WeZZard/vm-service";
const OWNER: &str = "the-owner";
const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";
const TOKEN: &str = "test-token-value";

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// One request a fake answered.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    url: String,
    body: Vec<u8>,
    headers: BTreeMap<String, String>,
}

impl Seen {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

/// A fake's answer.
struct Reply {
    status: u16,
    body: Vec<u8>,
    location: Option<String>,
}

fn reply(status: u16, value: Value) -> Reply {
    Reply {
        status,
        body: if value.is_null() {
            Vec::new()
        } else {
            value.to_string().into_bytes()
        },
        location: None,
    }
}

type Handler = dyn Fn(&Seen, &str) -> Reply + Send + Sync;

/// A loopback HTTP fake that records every request.
struct Fake {
    url: String,
    port: u16,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Fake {
    fn start(handler: Arc<Handler>) -> Fake {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("bind loopback");
        let address = server.server_addr().to_ip().expect("ip address");
        let url = format!("http://{address}");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&seen);
        let base = url.clone();
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
                let answer = handler(&entry, &base);
                record.lock().unwrap().push(entry);
                let mut response =
                    tiny_http::Response::from_data(answer.body).with_status_code(answer.status);
                response.add_header(
                    "Content-Type: application/json"
                        .parse::<tiny_http::Header>()
                        .unwrap(),
                );
                if let Some(location) = answer.location {
                    response.add_header(
                        format!("Location: {location}")
                            .parse::<tiny_http::Header>()
                            .unwrap(),
                    );
                }
                let _ = request.respond(response);
            }
        });
        Fake {
            url,
            port: address.port(),
            seen,
        }
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

/// What a test changes in a run's build.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tamper {
    Nothing,
    /// The manifest records another archive's SHA-256.
    Archive,
    /// The manifest names another run.
    OtherRun,
    /// GitHub's digest is not the download's.
    Digest,
}

/// A run's build: a zip of `test-environment.json` and
/// `test-environment.tar.gz`, whose `second-instance` appends its arguments
/// to `log` and writes or removes a manifest under `--root`.
fn build(folder: &Path, run: u64, log: &Path, tamper: Tamper) -> Vec<u8> {
    let bundle = folder.join(format!("bundle-{run}"));
    std::fs::create_dir_all(&bundle).unwrap();
    let launcher = bundle.join("second-instance");
    std::fs::write(
        &launcher,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> '{log}'
root="$3"
case "$1" in
start)
    mkdir -p "$root"
    printf '{{"id":"%s","root":"%s","created_root":true,"port":1,"session_port":2,"bind":"127.0.0.1","tart_home":"/t","guest_link":"relay","durable":false,"durable_cell":null,"executable":"/e","env":{{}}}}' "$5" "$root" > "$root/second-instance.json" ;;
stop)
    rm -rf "$root" ;;
esac
"#,
            log = log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(
        &launcher,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    std::fs::write(bundle.join("machine-service"), b"not a daemon").unwrap();
    let stage = folder.join(format!("stage-{run}"));
    std::fs::create_dir_all(&stage).unwrap();
    let archive = stage.join("test-environment.tar.gz");
    tar(&[
        "-czf",
        archive.to_str().unwrap(),
        "-C",
        bundle.to_str().unwrap(),
        ".",
    ]);
    let archive_sha = match tamper {
        Tamper::Archive => "0".repeat(64),
        _ => sha256(&std::fs::read(&archive).unwrap()),
    };
    let manifest = json!({
        "run_id": if tamper == Tamper::OtherRun { run + 1 } else { run },
        "head_sha": HEAD,
        "archive": "test-environment.tar.gz",
        "sha256": archive_sha,
    });
    std::fs::write(stage.join("test-environment.json"), manifest.to_string()).unwrap();
    let zip = folder.join(format!("artifact-{run}.zip"));
    tar(&[
        "--format",
        "zip",
        "-cf",
        zip.to_str().unwrap(),
        "-C",
        stage.to_str().unwrap(),
        "test-environment.json",
        "test-environment.tar.gz",
    ]);
    std::fs::read(&zip).unwrap()
}

fn tar(args: &[&str]) {
    let output = service_layout::test_build::harness::command("/usr/bin/tar")
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// One run as the fake GitHub lists it.
#[derive(Clone)]
struct FakeRun {
    id: u64,
    event: &'static str,
    actor: &'static str,
    labels: Vec<&'static str>,
    zip: Vec<u8>,
    tamper: Tamper,
    /// How many `status=queued` listings show the run before it ends,
    /// unless a JIT config ends it first.
    listed: usize,
}

impl FakeRun {
    fn new(id: u64, event: &'static str, actor: &'static str, labels: &[&'static str]) -> FakeRun {
        FakeRun {
            id,
            event,
            actor,
            labels: labels.to_vec(),
            zip: Vec::new(),
            tamper: Tamper::Nothing,
            listed: usize::MAX,
        }
    }
}

fn run_json(run: &FakeRun, status: &str) -> Value {
    json!({
        "id": run.id,
        "event": run.event,
        "path": ".github/workflows/lab.yml",
        "head_sha": HEAD,
        "head_repository": {"full_name": "WeZZard/vm-service"},
        "actor": {"login": run.actor},
        "triggering_actor": {"login": run.actor},
        "status": status,
    })
}

/// GitHub with `runs`, each with one lab job (its ID is the run's), whose
/// run ends once its runner is deleted.
fn github(runs: Vec<FakeRun>) -> Fake {
    let jit_made = Arc::new(Mutex::new(false));
    let deleted = Arc::new(Mutex::new(false));
    let listings = Arc::new(AtomicUsize::new(0));
    let handler: Arc<Handler> = Arc::new(move |seen: &Seen, base: &str| {
        let url = seen.url.as_str();
        if url == "/meta" {
            return reply(
                200,
                json!({
                    "web": ["140.82.112.0/20"], "api": ["140.82.112.0/20"], "git": [], "packages": [],
                    "domains": {"actions_inbound": {"full_domains": ["github.com", "results-receiver.actions.githubusercontent.com"]}}
                }),
            );
        }
        if let Some(id) = url.strip_prefix("/blob/") {
            // The storage's signed URL: never GitHub's token.
            if seen.headers.contains_key("authorization") {
                return reply(403, json!({"message": "the token reached the storage"}));
            }
            let run = runs.iter().find(|run| run.id.to_string() == id).unwrap();
            return Reply {
                status: 200,
                body: run.zip.clone(),
                location: None,
            };
        }
        if seen.headers.get("authorization").map(String::as_str) != Some(&format!("Bearer {TOKEN}"))
        {
            return reply(401, json!({"message": "Bad credentials"}));
        }
        let made = *jit_made.lock().unwrap();
        let ended = *deleted.lock().unwrap();
        let status_of = |run: &FakeRun, listing: usize| {
            if ended || listing >= run.listed {
                "completed"
            } else if made {
                "in_progress"
            } else {
                "queued"
            }
        };
        if let Some(query) = url.strip_prefix(&format!("{REPO}/actions/runs?")) {
            let listing = if query.contains("status=queued") {
                listings.fetch_add(1, Ordering::SeqCst)
            } else {
                listings.load(Ordering::SeqCst).saturating_sub(1)
            };
            let want = if query.contains("status=queued") {
                "queued"
            } else {
                "in_progress"
            };
            let listed: Vec<Value> = runs
                .iter()
                .filter(|run| status_of(run, listing) == want)
                .map(|run| run_json(run, want))
                .collect();
            return reply(200, json!({"workflow_runs": listed}));
        }
        for run in &runs {
            let prefix = format!("{REPO}/actions/runs/{}", run.id);
            let listing = listings.load(Ordering::SeqCst).saturating_sub(1);
            if url == prefix {
                return reply(200, run_json(run, status_of(run, listing)));
            }
            if url.starts_with(&format!("{prefix}/jobs")) {
                let status = match status_of(run, listing) {
                    "queued" => "queued",
                    "in_progress" => "in_progress",
                    _ => "completed",
                };
                return reply(
                    200,
                    json!({"jobs": [{"id": run.id, "status": status, "labels": run.labels}]}),
                );
            }
            if url.starts_with(&format!("{prefix}/artifacts?")) {
                let digest = if run.tamper == Tamper::Digest {
                    format!("sha256:{}", "f".repeat(64))
                } else {
                    format!("sha256:{}", sha256(&run.zip))
                };
                return reply(
                    200,
                    json!({"artifacts": [{"id": run.id, "name": "lab-test-environment", "expired": false,
                        "digest": digest, "workflow_run": {"id": run.id, "head_sha": HEAD}}]}),
                );
            }
            if url == format!("{REPO}/actions/artifacts/{}/zip", run.id) {
                return Reply {
                    status: 302,
                    body: Vec::new(),
                    location: Some(format!("{base}/blob/{}", run.id)),
                };
            }
        }
        if url.starts_with(&format!("{REPO}/actions/runners/generate-jitconfig")) {
            *jit_made.lock().unwrap() = true;
            return reply(
                201,
                json!({"runner": {"id": 55}, "encoded_jit_config": "ZW5jb2RlZC1qaXQ="}),
            );
        }
        if url.starts_with(&format!("{REPO}/actions/runners/55")) && seen.method == "DELETE" {
            *deleted.lock().unwrap() = true;
            return reply(204, Value::Null);
        }
        if url.starts_with(&format!("{REPO}/actions/runners")) {
            let runners = if made && !ended {
                json!([{"id": 55, "name": "lab-x-1", "status": "online", "busy": true, "labels": [{"name": "self-hosted"}]}])
            } else {
                json!([])
            };
            return reply(200, json!({"runners": runners}));
        }
        reply(404, json!({"message": "Not Found"}))
    });
    Fake::start(handler)
}

/// Production with `taken` macOS slots and `leases`, whose acquisitions'
/// records show the pack `pack`, or which refuses them with `refusal`.
fn production(
    taken: u32,
    leases: Value,
    pack: Option<&'static str>,
    refusal: Option<&'static str>,
) -> Fake {
    let handler: Arc<Handler> = Arc::new(move |seen: &Seen, _| {
        let url = seen.url.as_str();
        match (seen.method.as_str(), url) {
            ("GET", "/images") => reply(
                200,
                json!({"capacity": {"macos_limit": 2, "macos_taken": taken, "foreign_macos_guests": 0, "macos_running": taken}}),
            ),
            ("GET", "/machines") => reply(200, json!({"machines": leases.clone()})),
            ("POST", "/acquire") => {
                if let Some(refusal) = refusal {
                    return reply(400, json!({"error": refusal}));
                }
                let body = seen.json();
                reply(
                    200,
                    json!({"vm": "pilot-runner-1", "lease_id": "L1", "purpose": body["purpose"], "env": pack}),
                )
            }
            ("PUT", url) if url.starts_with("/machines/pilot-runner-1/files?") => {
                if seen.headers.get("x-content-sha256") != Some(&sha256(&seen.body)) {
                    return reply(400, json!({"error": "digest"}));
                }
                reply(200, json!({"ok": true}))
            }
            ("POST", "/machines/pilot-runner-1/exec") => reply(
                200,
                json!({"rc": 0, "output": "lab-runner: starting runner"}),
            ),
            ("POST", url) if url.ends_with("/release") => reply(200, json!({"released": true})),
            _ => reply(404, json!({"error": "not found"})),
        }
    });
    Fake::start(handler)
}

/// A test environment of run `run` that answers as itself.
fn environment(run: u64, name: &str, kinds: Value) -> Fake {
    let name = name.to_string();
    let handler: Arc<Handler> = Arc::new(move |seen: &Seen, _| match seen.url.as_str() {
        "/health" => reply(
            200,
            json!({"ok": true, "instance": name, "kinds": kinds, "second_instance": {"id": format!("r{run}")}}),
        ),
        "/images" => reply(
            200,
            json!({"capacity": {"macos_limit": 2, "macos_running": 0}}),
        ),
        _ => reply(404, json!({"error": "not found"})),
    });
    Fake::start(handler)
}

struct Lab {
    folder: tempfile::TempDir,
    root: PathBuf,
}

impl Lab {
    fn new() -> Lab {
        let folder = tempfile::tempdir().unwrap();
        let root = folder.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("egress")).unwrap();
        Lab { folder, root }
    }

    fn launches(&self) -> Vec<String> {
        std::fs::read_to_string(self.root.join("launcher.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn build(&self, run: &mut FakeRun) {
        run.zip = build(
            &self.root,
            run.id,
            &self.root.join("launcher.log"),
            run.tamper,
        );
    }

    fn state(&self) -> PathBuf {
        self.root.join("state")
    }
}

struct Outcome {
    stderr: String,
    status: std::process::ExitStatus,
}

struct Options<'a> {
    role: &'a str,
    token: bool,
    owner: Option<&'a str>,
    environment_port: u16,
    peers: Vec<String>,
    rounds: u64,
}

impl Default for Options<'_> {
    fn default() -> Self {
        Options {
            role: "vm-host",
            token: true,
            owner: None,
            environment_port: 1,
            peers: Vec::new(),
            rounds: 3,
        }
    }
}

fn run_lab_host(lab: &Lab, github: &Fake, production: Option<&Fake>, options: Options) -> Outcome {
    let root = &lab.root;
    let archive = root.join("runner.tar.gz");
    std::fs::write(&archive, b"a runner archive").unwrap();
    let sha = sha256(b"a runner archive");
    let mut seams = json!({
        "github": github.url,
        "role": options.role,
        "state": lab.state(),
        "egress": root.join("egress"),
        "address": "127.0.0.1",
        "environment_port": options.environment_port,
        "peers": options.peers,
        "dns": {"github.com": ["140.82.112.3"], "results-receiver.actions.githubusercontent.com": ["20.42.73.1"]},
        "runners": {"osx-arm64": {"path": archive, "sha256": sha}, "linux-arm64": {"path": archive, "sha256": sha}},
        "rounds": options.rounds,
        "interval_s": 0
    });
    if let Some(production) = production {
        seams["production"] = json!(production.url);
    }
    if options.token {
        std::fs::write(root.join("token"), format!("{TOKEN}\n")).unwrap();
        seams["token"] = json!(root.join("token"));
    }
    if let Some(owner) = options.owner {
        std::fs::write(
            root.join("controller.json"),
            json!({"owner": owner}).to_string(),
        )
        .unwrap();
        seams["config"] = json!(root.join("controller.json"));
    }
    std::fs::write(root.join("seams.json"), seams.to_string()).unwrap();
    let output = service_layout::test_build::harness::command(LAB_HOST)
        .arg("run")
        .env("HOME", root)
        .env("LAB_HOST_TEST_SEAMS", root.join("seams.json"))
        .output()
        .expect("run lab-host");
    let _ = &lab.folder;
    Outcome {
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        status: output.status,
    }
}

fn put_paths(production: &Fake) -> Vec<String> {
    production
        .seen()
        .iter()
        .filter(|seen| seen.method == "PUT")
        .filter_map(|seen| seen.url.split("path=").nth(1).map(str::to_string))
        .collect()
}

fn acquisitions(production: &Fake) -> Vec<Value> {
    production
        .seen()
        .iter()
        .filter(|seen| seen.url == "/acquire")
        .map(Seen::json)
        .collect()
}

#[test]
fn a_job_gets_a_ci_runner_vm_from_production_and_its_runs_test_environment() {
    let lab = Lab::new();
    let mut run = FakeRun::new(100, "push", "someone", &["self-hosted", "lab-ubuntu"]);
    lab.build(&mut run);
    let github = github(vec![run]);
    let production = production(0, json!({}), Some("ci"), None);
    let environment = environment(100, "mac-a", json!(["virtual"]));
    let outcome = run_lab_host(
        &lab,
        &github,
        Some(&production),
        Options {
            environment_port: environment.port,
            ..Options::default()
        },
    );
    assert!(outcome.status.success(), "{}", outcome.stderr);

    // The run's build, fetched with the token, its storage without it.
    assert_eq!(
        github.count("GET", "/actions/runs/100/artifacts"),
        1,
        "{}",
        outcome.stderr
    );
    assert_eq!(github.count("GET", "/actions/artifacts/100/zip"), 1);
    assert_eq!(github.count("GET", "/blob/100"), 1, "{}", outcome.stderr);

    // Its test environment, started with the run's own launcher on the
    // host's address and the lab ports, then stopped once the run ended.
    let state = lab.state();
    let launches = lab.launches();
    let root = state.join("runs/100/env");
    assert_eq!(launches.len(), 2, "{launches:?}\n{}", outcome.stderr);
    assert!(
        launches[0].starts_with(&format!(
            "start --root {} --id r100 --port {} --session-port {} --bind 127.0.0.1 ",
            root.display(),
            environment.port,
            environment.port + 1
        )),
        "{launches:?}"
    );
    assert!(launches[0].contains(&format!(
        "--executable {}",
        state.join("runs/100/bundle/machine-service").display()
    )));
    assert!(!launches[0].contains("--durable"));
    assert_eq!(launches[1], format!("stop --root {}", root.display()));
    assert!(
        !state.join("runs/100").exists(),
        "the run's folder was kept"
    );

    // One egress acquisition from production, for the job, with the ci pack.
    let acquired = acquisitions(&production);
    assert_eq!(acquired.len(), 1, "{}", outcome.stderr);
    assert_eq!(acquired[0]["purpose"], "lab-100");
    assert_eq!(acquired[0]["image"], "ubuntu2404");
    assert_eq!(acquired[0]["network"], "internet");
    assert_eq!(acquired[0]["egress"], "allowlist");
    assert_eq!(acquired[0]["env"], "ci");

    // The runner, its plan, the test environments' names and the JIT config.
    let paths = put_paths(&production);
    for name in ["runner.tar.gz", "lab-plan.json", "hosts", "jit"] {
        assert!(
            paths.iter().any(|path| path.ends_with(name)),
            "{name} not placed: {paths:?}"
        );
    }
    let plan = production
        .seen()
        .into_iter()
        .find(|seen| seen.url.contains("lab-plan.json"))
        .map(|seen| seen.json())
        .unwrap();
    assert_eq!(plan["run"], 100);
    assert_eq!(plan["job"], 100);
    assert_eq!(plan["vm_target"]["name"], "mac-a");
    assert_eq!(
        plan["vm_target"]["url"],
        format!("http://mac-a:{}", environment.port)
    );
    // Where the test environment is reaches the run only inside its VM.
    assert!(!outcome.stderr.contains(&environment.port.to_string()));

    // Exactly the job's labels, and the config never in an exec or a log.
    let jit = github
        .seen()
        .into_iter()
        .find(|seen| seen.url.contains("generate-jitconfig"))
        .unwrap();
    assert_eq!(jit.json()["labels"], json!(["self-hosted", "lab-ubuntu"]));
    assert_eq!(
        jit.json()["name"],
        format!("lab-{}-100", &sha256(b"test")[..8])
    );
    let exec = production
        .seen()
        .into_iter()
        .find(|seen| seen.url.ends_with("/exec"))
        .unwrap();
    let exec_body = String::from_utf8_lossy(&exec.body).into_owned();
    assert!(!exec_body.contains("ZW5jb2RlZC1qaXQ="));
    assert!(exec_body.contains("ACTIONS_RUNNER_INPUT_JITCONFIG"));
    assert!(!outcome.stderr.contains("ZW5jb2RlZC1qaXQ="));
    assert!(!outcome.stderr.contains(TOKEN), "the token reached the log");

    // Released, and its registration removed; nothing left in its job files.
    assert_eq!(
        production.count("POST", "/machines/pilot-runner-1/release"),
        1
    );
    assert_eq!(github.count("DELETE", "/actions/runners/55"), 1);
    assert_eq!(
        std::fs::read_dir(state.join("jobs"))
            .map(|d| d.count())
            .unwrap_or(0),
        0
    );

    // The egress allowlist and the lab table.
    let allow = std::fs::read_to_string(lab.root.join("egress/egress-allow")).unwrap();
    assert!(allow.lines().any(|line| line == "# check: api.github.com"));
    for entry in ["140.82.112.0/20", "140.82.112.3", "20.42.73.1", "1.1.1.1"] {
        assert!(allow.lines().any(|line| line == entry), "{entry}: {allow}");
    }
    let table = std::fs::read_to_string(lab.root.join("egress/egress-lab")).unwrap();
    assert!(table.lines().any(|line| line == "127.0.0.1"), "{table}");
}

#[test]
fn a_runner_vm_without_the_ci_pack_gets_no_runner() {
    for (pack, refusal) in [
        (Some("default"), None),
        (None, None),
        (None, Some("credential pack not found: ci")),
    ] {
        let lab = Lab::new();
        let mut run = FakeRun::new(101, "push", "someone", &["self-hosted", "lab-ubuntu"]);
        lab.build(&mut run);
        let github = github(vec![run]);
        let production = production(0, json!({}), pack, refusal);
        let environment = environment(101, "mac-a", json!(["virtual"]));
        let outcome = run_lab_host(
            &lab,
            &github,
            Some(&production),
            Options {
                environment_port: environment.port,
                rounds: 1,
                ..Options::default()
            },
        );
        assert!(outcome.status.success(), "{}", outcome.stderr);
        assert_eq!(acquisitions(&production)[0]["env"], "ci");
        // No runner, no plan, no JIT config.
        assert_eq!(github.count("POST", "generate-jitconfig"), 0, "{pack:?}");
        assert!(put_paths(&production).is_empty(), "{pack:?}");
        match refusal {
            Some(_) => {
                assert!(
                    outcome.stderr.contains("production lent no runner VM"),
                    "{}",
                    outcome.stderr
                );
                assert_eq!(production.count("POST", "/release"), 0);
            }
            None => {
                assert!(
                    outcome.stderr.contains("gets no runner"),
                    "{}",
                    outcome.stderr
                );
                // A lease with another pack is given back at once.
                assert_eq!(
                    production.count("POST", "/machines/pilot-runner-1/release"),
                    1
                );
            }
        }
    }
}

#[test]
fn a_build_that_is_not_the_runs_own_starts_nothing() {
    for tamper in [Tamper::Archive, Tamper::OtherRun, Tamper::Digest] {
        let lab = Lab::new();
        let mut run = FakeRun::new(102, "push", "someone", &["self-hosted", "lab-ubuntu"]);
        run.tamper = tamper;
        lab.build(&mut run);
        let github = github(vec![run]);
        let production = production(0, json!({}), Some("ci"), None);
        let environment = environment(102, "mac-a", json!(["virtual"]));
        let outcome = run_lab_host(
            &lab,
            &github,
            Some(&production),
            Options {
                environment_port: environment.port,
                rounds: 1,
                ..Options::default()
            },
        );
        assert!(outcome.status.success(), "{}", outcome.stderr);
        assert!(outcome.stderr.contains("refused"), "{}", outcome.stderr);
        assert!(lab.launches().is_empty(), "{:?}", lab.launches());
        assert_eq!(production.count("POST", "/acquire"), 0);
        assert!(!lab.state().join("runs/102").exists());
    }
}

#[test]
fn a_run_that_is_not_a_push_or_dispatch_of_the_repository_is_never_served() {
    let lab = Lab::new();
    let mut run = FakeRun::new(
        103,
        "pull_request",
        "someone",
        &["self-hosted", "lab-ubuntu"],
    );
    lab.build(&mut run);
    let github = github(vec![run]);
    let production = production(0, json!({}), Some("ci"), None);
    let outcome = run_lab_host(&lab, &github, Some(&production), Options::default());
    assert!(outcome.status.success(), "{}", outcome.stderr);
    assert_eq!(github.count("GET", "/artifacts"), 0);
    assert_eq!(github.count("GET", "/actions/runs/103/jobs"), 0);
    assert_eq!(production.count("POST", "/acquire"), 0);
    assert!(lab.launches().is_empty());
}

#[test]
fn a_job_without_room_stays_queued_and_fetches_nothing() {
    let lab = Lab::new();
    let mut run = FakeRun::new(104, "push", "someone", &["self-hosted", "lab-macos"]);
    lab.build(&mut run);
    let github = github(vec![run]);
    let production = production(2, json!({}), Some("ci"), None);
    let outcome = run_lab_host(&lab, &github, Some(&production), Options::default());
    assert!(outcome.status.success(), "{}", outcome.stderr);
    assert_eq!(
        production.count("POST", "/acquire"),
        0,
        "{}",
        outcome.stderr
    );
    assert_eq!(github.count("POST", "generate-jitconfig"), 0);
    assert_eq!(github.count("GET", "/artifacts"), 0);
    assert!(lab.launches().is_empty());
}

#[test]
fn without_the_token_nothing_is_served_and_the_log_says_how_to_store_it() {
    let lab = Lab::new();
    let github = github(vec![FakeRun::new(
        105,
        "push",
        "someone",
        &["self-hosted", "lab-ubuntu"],
    )]);
    let production = production(0, json!({}), Some("ci"), None);
    let outcome = run_lab_host(
        &lab,
        &github,
        Some(&production),
        Options {
            token: false,
            ..Options::default()
        },
    );
    assert!(outcome.status.success(), "{}", outcome.stderr);
    assert_eq!(production.count("POST", "/acquire"), 0);
    assert!(
        outcome.stderr.contains("security add-generic-password -U -a WeZZard/vm-service -s com.wezzard.vm-service.lab.github-token -w"),
        "{}",
        outcome.stderr
    );
    // Said once, not every round.
    assert_eq!(
        outcome.stderr.matches("serving no job").count(),
        1,
        "{}",
        outcome.stderr
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
fn what_a_crashed_controller_left_is_released_and_stopped_at_start() {
    let lab = Lab::new();
    // A test environment of run 99 with its manifest and its own launcher,
    // as a crash left it.
    let mut old = FakeRun::new(99, "push", "someone", &["self-hosted", "lab-ubuntu"]);
    lab.build(&mut old);
    let folder = lab.state().join("runs/99");
    std::fs::create_dir_all(folder.join("env")).unwrap();
    std::fs::write(folder.join("env/second-instance.json"), "{}").unwrap();
    std::fs::create_dir_all(folder.join("bundle")).unwrap();
    std::fs::copy(
        lab.root.join("bundle-99/second-instance"),
        folder.join("bundle/second-instance"),
    )
    .unwrap();
    std::fs::set_permissions(
        folder.join("bundle/second-instance"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    // A folder whose run never got as far as its launcher.
    std::fs::create_dir_all(lab.state().join("runs/98/artifact")).unwrap();

    let github = github(Vec::new());
    let leases = json!({
        "pilot-runner-old": {"vm": "pilot-runner-old", "lease_id": "x", "purpose": "lab-99", "env": "ci"},
        "pilot-mine": {"vm": "pilot-mine", "lease_id": "y", "purpose": "lab-98", "env": "default"},
        "pilot-other": {"vm": "pilot-other", "lease_id": "z", "purpose": "work", "env": "ci"}
    });
    let production = production(0, leases, Some("ci"), None);
    let outcome = run_lab_host(&lab, &github, Some(&production), Options::default());
    assert!(outcome.status.success(), "{}", outcome.stderr);
    assert_eq!(
        production.count("POST", "/machines/pilot-runner-old/release"),
        1,
        "{}",
        outcome.stderr
    );
    // Production's other leases are the owner's, never the controller's.
    assert_eq!(production.count("POST", "/machines/pilot-mine/release"), 0);
    assert_eq!(production.count("POST", "/machines/pilot-other/release"), 0);
    assert_eq!(
        lab.launches(),
        vec![format!("stop --root {}", folder.join("env").display())]
    );
    assert!(!folder.exists());
    assert!(!lab.state().join("runs/98").exists());
}

#[test]
fn a_durable_cell_waits_for_and_then_reaches_its_durable_macs_test_environment() {
    let labels = ["self-hosted", "lab-macos", "lab-durable-target"];
    // No durable Mac answers for the run: the job stays queued.
    let lab = Lab::new();
    let github_alone = github(vec![FakeRun::new(106, "workflow_dispatch", OWNER, &labels)]);
    let production_alone = production(0, json!({}), Some("ci"), None);
    let elsewhere = environment(999, "mac-d", json!(["durable"]));
    let outcome = run_lab_host(
        &lab,
        &github_alone,
        Some(&production_alone),
        Options {
            peers: vec![format!("127.0.0.1:{}", elsewhere.port)],
            ..Options::default()
        },
    );
    assert!(outcome.status.success(), "{}", outcome.stderr);
    assert_eq!(
        production_alone.count("POST", "/acquire"),
        0,
        "{}",
        outcome.stderr
    );
    assert!(
        outcome.stderr.contains("no durable Mac"),
        "{}",
        outcome.stderr
    );

    // Its durable Mac's test environment answers: the job is served, and
    // this host starts no test environment of its own for it.
    let lab = Lab::new();
    let github = github(vec![FakeRun::new(106, "workflow_dispatch", OWNER, &labels)]);
    let production = production(0, json!({}), Some("ci"), None);
    let durable = environment(106, "mac-d", json!(["durable"]));
    let outcome = run_lab_host(
        &lab,
        &github,
        Some(&production),
        Options {
            peers: vec![format!("127.0.0.1:{}", durable.port)],
            ..Options::default()
        },
    );
    assert!(outcome.status.success(), "{}", outcome.stderr);
    assert_eq!(acquisitions(&production).len(), 1, "{}", outcome.stderr);
    assert_eq!(github.count("GET", "/artifacts"), 0);
    assert!(lab.launches().is_empty());
    let plan = production
        .seen()
        .into_iter()
        .find(|seen| seen.url.contains("lab-plan.json"))
        .map(|seen| seen.json())
        .unwrap();
    assert_eq!(plan["vm_target"], Value::Null);
    assert_eq!(plan["durable_targets"][0]["name"], "mac-d");
    assert_eq!(
        plan["durable_targets"][0]["url"],
        format!("http://mac-d:{}", durable.port)
    );
}

#[test]
fn a_durable_mac_starts_its_test_environment_only_for_its_owners_dispatch() {
    let durable_job = ["self-hosted", "lab-macos", "lab-durable-target"];
    let lab = Lab::new();
    let mut runs = vec![
        FakeRun::new(200, "workflow_dispatch", OWNER, &durable_job),
        FakeRun::new(201, "push", OWNER, &durable_job),
        FakeRun::new(202, "workflow_dispatch", "someone-else", &durable_job),
        FakeRun::new(
            203,
            "workflow_dispatch",
            OWNER,
            &["self-hosted", "lab-macos"],
        ),
    ];
    for run in &mut runs {
        lab.build(run);
        // Listed for the first round only, then ended.
        run.listed = 1;
    }
    let github = github(runs);
    let environment = environment(200, "mac-d", json!(["durable"]));
    let outcome = run_lab_host(
        &lab,
        &github,
        None,
        Options {
            role: "durable",
            owner: Some(OWNER),
            environment_port: environment.port,
            rounds: 2,
            ..Options::default()
        },
    );
    assert!(outcome.status.success(), "{}", outcome.stderr);
    // Only run 200's build was fetched; no other run's jobs were even read.
    assert_eq!(github.count("GET", "/artifacts?"), 1, "{}", outcome.stderr);
    assert_eq!(github.count("GET", "/actions/runs/200/artifacts"), 1);
    for other in [201, 202] {
        assert_eq!(github.count("GET", &format!("/actions/runs/{other}/")), 0);
    }
    // Started with durable exposure for the durable cells, on the one
    // address and the lab ports; stopped once the run ended.
    let launches = lab.launches();
    let root = lab.state().join("runs/200/env");
    assert_eq!(launches.len(), 2, "{launches:?}\n{}", outcome.stderr);
    assert!(launches[0].starts_with(&format!(
        "start --root {} --id r200 --port {} --session-port {} --bind 127.0.0.1 ",
        root.display(),
        environment.port,
        environment.port + 1
    )));
    assert!(launches[0]
        .ends_with("--durable --durable-cell macos-client.durable-mac,ubuntu-client.durable-mac"));
    assert_eq!(launches[1], format!("stop --root {}", root.display()));
    assert!(!lab.state().join("runs/200").exists());
    // A durable Mac leases nothing and registers no runner.
    assert_eq!(github.count("POST", "generate-jitconfig"), 0);
}

#[test]
fn a_durable_mac_without_an_owner_serves_nothing() {
    let lab = Lab::new();
    let mut run = FakeRun::new(
        210,
        "workflow_dispatch",
        OWNER,
        &["self-hosted", "lab-macos", "lab-durable-target"],
    );
    lab.build(&mut run);
    let github = github(vec![run]);
    let outcome = run_lab_host(
        &lab,
        &github,
        None,
        Options {
            role: "durable",
            ..Options::default()
        },
    );
    assert!(outcome.status.success(), "{}", outcome.stderr);
    assert!(
        outcome.stderr.contains("no owner login"),
        "{}",
        outcome.stderr
    );
    assert_eq!(
        outcome.stderr.matches("no owner login").count(),
        1,
        "said once"
    );
    assert!(lab.launches().is_empty());
    assert_eq!(github.seen().len(), 0);
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
