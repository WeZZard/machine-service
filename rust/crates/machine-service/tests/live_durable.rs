//! Live durable-machine leases on this Mac (`docs/arch/test-layers.md`,
//! TL-6; AC-18, AC-20, AC-21, AC-23 to AC-26, AC-28, AC-30 to AC-33).
//!
//! Each test starts second instances of this worktree's `machine-service`
//! on their own ports, with their own state and configuration folders under
//! a temporary directory and durable exposure on, beside the installed
//! instance on 6240, which it never touches (AC-18). The instances use the
//! real host-wide lease lock, so they can never lease the Mac while another
//! instance holds it.
//!
//! The configuration folder holds fake agent keys and a placeholder
//! endpoint; no real key is ever written. Commands are harmless: `id`,
//! `env` names, `sleep` stand-ins the test starts itself, one uniquely
//! named folder in `~/Library/Application Support` that the test removes,
//! a small folder pushed into and pulled out of the lease's workspace, and
//! `open -a TextEdit`, which is quit at release only when the lease
//! launched it. `machctl` runs with Consul at a closed port and its own
//! state folder. Every lease is released.
//!
//! The daemon is this worktree's test build, started with the live
//! layer's opt-in (`MACHINE_SERVICE_TEST_LIVE=TL-6`), which only these
//! ignored tests set: with it the build sweeps the real session and runs the
//! system's tools as a release does, and still refuses the installed ports
//! and folders (DC-1xx-ISO-3).
//!
//! ```sh
//! cargo test -p machine-service --test live_durable -- --ignored --test-threads=1
//! ```
//!
//! Each test writes its answers to a new directory under
//! `test-evidence/live-durable/`.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::{canonical_dir, free_port, http_timeout, try_http, DAEMON};
use serde_json::{json, Value};

const INSTALLED_PORT: u16 = 6240;
const CLAUDE_KEY: &str = "sk-fake-live-claude-key-0000000001";
const CODEX_KEY: &str = "sk-fake-live-codex-key-0000000002";
const LONG: Duration = Duration::from_secs(300);
const LOCK_FILE: &str = "/var/tmp/com.wezzard.machine-service/durable-machine.lock";

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").expect("HOME"))
}

fn user() -> String {
    String::from_utf8(
        Command::new("/usr/bin/id")
            .arg("-un")
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string()
}

/// The installed instance's answer, which must not change (AC-18).
fn installed_health() -> Option<Value> {
    try_http(INSTALLED_PORT, "GET", "/health", None, None)
        .ok()
        .map(|response| response.body)
}

fn evidence(test: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../test-evidence/live-durable")
        .join(format!("{stamp}-{test}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn record(dir: &Path, name: &str, value: &Value) {
    std::fs::write(
        dir.join(format!("{name}.json")),
        serde_json::to_vec_pretty(value).unwrap(),
    )
    .unwrap();
}

struct Live {
    _dir: tempfile::TempDir,
    root: PathBuf,
    config: PathBuf,
    workspace_root_existed: bool,
    installed: Option<Value>,
}

struct Instance {
    child: Option<Child>,
    port: u16,
    state: PathBuf,
}

impl Live {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_dir(dir.path());
        let config = root.join("config");
        let agents = config.join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::set_permissions(&agents, std::fs::Permissions::from_mode(0o700)).unwrap();
        for (name, text) in [
            ("claude-code.env", format!("ANTHROPIC_API_KEY={CLAUDE_KEY}\n")),
            ("codex.env", format!("LITELLM_CODEX_KEY={CODEX_KEY}\n")),
            (
                "endpoints.json",
                r#"{"claude-code": {"base_url": "http://<gateway-host>"}, "codex": {"base_url": "http://<gateway-host>/v1"}}"#.to_string(),
            ),
        ] {
            let path = agents.join(name);
            std::fs::write(&path, text).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        Self {
            _dir: dir,
            root,
            config,
            workspace_root_existed: home().join("machine-service").exists(),
            installed: installed_health(),
        }
    }

    fn start(&self, state: &str) -> Instance {
        let state_dir = self.root.join(state);
        std::fs::create_dir_all(&state_dir).unwrap();
        for _ in 0..16 {
            let port = free_port();
            let err = std::fs::File::create(state_dir.join(format!("daemon-{port}.err"))).unwrap();
            // Started in its own session, like the LaunchAgent, so the test's
            // own process group is never the lease's.
            let mut child = Command::new("/usr/bin/perl")
                .args(["-MPOSIX", "-e", "POSIX::setsid(); exec @ARGV or die", DAEMON])
                .env_clear()
                .env("HOME", home())
                .env("USER", user())
                .env("LOGNAME", user())
                .env("SHELL", "/bin/zsh")
                .env(
                    "PATH",
                    "/opt/homebrew/bin:/opt/homebrew/sbin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
                )
                .env("MACHINE_SERVICE_STATE", &state_dir)
                .env("MACHINE_SERVICE_PORT", port.to_string())
                .env("MACHINE_SERVICE_HOST", "127.0.0.1")
                .env("MACHINE_SERVICE_CONFIG_DIR", &self.config)
                .env("MACHINE_SERVICE_EXPOSE_DURABLE", "on")
                // The opt-in live layer (DC-1xx-ISO-3): the test build runs
                // the system's tools, reads the Mac's name, takes the
                // host-wide lock and sweeps the real session, as a release
                // does; the installed ports and folders stay refused.
                .env(
                    service_layout::test_build::LIVE_VARIABLE,
                    service_layout::test_build::LIVE_VALUE,
                )
                .env("MACHINE_SERVICE_CONSOLE_CONFIG", self.root.join("no-console.json"))
                .env("PILOT_REPO", self.root.join("no-pilot"))
                .env("TART_HOME", self.root.join("no-tart"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::from(err))
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(30);
            while Instant::now() < deadline {
                if let Ok(Some(_)) = child.try_wait() {
                    break;
                }
                if try_http(port, "GET", "/health", None, None).is_ok_and(|r| r.status == 200) {
                    return Instance {
                        child: Some(child),
                        port,
                        state: state_dir,
                    };
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        panic!("second instance did not start");
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        assert_eq!(
            installed_health(),
            self.installed,
            "AC-18: the installed instance's answer changed"
        );
        // Leave no empty workspace root behind that the test created.
        if !self.workspace_root_existed {
            let root = home().join("machine-service");
            let _ = std::fs::remove_dir(root.join("leases"));
            let _ = std::fs::remove_dir(&root);
        }
    }
}

impl Instance {
    fn request(&self, method: &str, path: &str, body: Option<&Value>) -> common::Response {
        http_timeout(self.port, method, path, body, None, LONG)
    }

    fn name(&self) -> String {
        self.request("GET", "/health", None).body["instance"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn acquire(&self, purpose: &str, env: &str) -> (String, String, Value) {
        let name = self.name();
        let response = self.request(
            "POST",
            "/acquire",
            Some(&json!({"machine": name, "purpose": purpose, "env": env, "ttl_hours": 1})),
        );
        assert_eq!(response.status, 200, "{}", response.body);
        let lease = response.body["lease_id"].as_str().unwrap().to_string();
        (name, lease, response.body)
    }

    fn exec(&self, name: &str, lease: &str, script: &str) -> Value {
        let response = self.request(
            "POST",
            &format!("/machines/{name}/exec"),
            Some(&json!({"lease_id": lease, "script": script})),
        );
        assert_eq!(response.status, 200, "{}", response.body);
        response.body
    }

    fn release(&self, name: &str, lease: &str) -> Value {
        let response = self.request(
            "POST",
            &format!("/machines/{name}/release"),
            Some(&json!({"lease_id": lease, "reason": "live-test"})),
        );
        assert_eq!(response.status, 200, "{}", response.body);
        response.body
    }

    fn current(&self) -> Option<String> {
        let text = std::fs::read_to_string(self.state.join("durable.json")).ok()?;
        serde_json::from_str::<Value>(&text).ok()?["current"]
            .as_str()
            .map(str::to_string)
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // SAFETY: the PID is this handle's own child.
            unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
            let _ = child.wait();
        }
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        if self.child.is_some() {
            if let Some(lease) = self.current() {
                let name = self.name();
                let _ = self.request(
                    "POST",
                    &format!("/machines/{name}/release"),
                    Some(&json!({"lease_id": lease, "reason": "live-test-cleanup"})),
                );
            }
        }
        self.stop();
    }
}

fn alive(pid: i32) -> bool {
    let state = Command::new("/bin/ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_default();
    !state.is_empty() && !state.starts_with('Z')
}

fn wait_dead(pid: i32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// A stand-in the test started itself; killed when the test ends.
struct StandIn(Child);

impl StandIn {
    fn spawn(program: &Path, arguments: &[&str]) -> Self {
        Self(
            Command::new(program)
                .args(arguments)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        )
    }
    fn pid(&self) -> i32 {
        self.0.id() as i32
    }
}

impl Drop for StandIn {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A PID the lease started that the test kills itself if still alive.
struct Leftover(i32);

impl Drop for Leftover {
    fn drop(&mut self) {
        if alive(self.0) {
            // SAFETY: only a PID this test's lease started.
            unsafe { libc::kill(self.0, libc::SIGKILL) };
        }
    }
}

/// AC-20, AC-21, AC-18: a lease through one instance holds the host-wide
/// lock; another instance with its own state refuses with 409 naming the
/// purpose; after release the lock names nobody.
#[test]
#[ignore = "live TL-6: leases this Mac through two second instances with exposure on; see the module documentation"]
fn live_ac21_one_lease_across_instances() {
    let live = Live::new();
    let evidence = evidence("ac21");
    let first = live.start("state-a");
    let second = live.start("state-b");
    let (name, lease, acquired) = first.acquire("live-check", "none");
    record(&evidence, "acquired", &acquired);
    let lock: Value = serde_json::from_str(&std::fs::read_to_string(LOCK_FILE).unwrap()).unwrap();
    assert_eq!(lock["holder"]["lease_id"], lease.as_str());
    assert_eq!(
        lock["holder"]["state_dir"],
        first.state.to_string_lossy().as_ref()
    );
    let refused = second.request(
        "POST",
        "/acquire",
        Some(&json!({"machine": name, "purpose": "other"})),
    );
    record(&evidence, "refused", &refused.body);
    assert_eq!(refused.status, 409);
    assert!(refused.body["error"]
        .as_str()
        .unwrap()
        .starts_with(&format!(
            "durable machine leased: {name} (purpose live-check, until "
        )));
    let workspace = PathBuf::from(acquired["workspace"].as_str().unwrap());
    assert!(workspace.join("work").is_dir());
    let released = first.release(&name, &lease);
    record(&evidence, "released", &released);
    assert!(!workspace.exists());
    let lock: Value = serde_json::from_str(&std::fs::read_to_string(LOCK_FILE).unwrap()).unwrap();
    assert_eq!(lock["holder"], Value::Null);
}

/// AC-23 to AC-26: in the GUI session, a command runs as this user with the
/// login PATH and the lease's variables, without the owner's credentials;
/// the fake keys reach it with the placeholder endpoint, come back
/// redacted, and appear in no record or log.
#[test]
#[ignore = "live TL-6: runs harmless commands on this Mac through a second instance with exposure on; see the module documentation"]
fn live_ac23_to_ac26_session_and_agent_keys() {
    let live = Live::new();
    let evidence = evidence("session");
    let owner_codex = std::fs::read(home().join(".codex/config.toml")).ok();
    let instance = live.start("state");
    let (name, lease, acquired) = instance.acquire("live-session", "default");
    record(&evidence, "capabilities", &acquired["capabilities"]);
    let output = instance.exec(
        &name,
        &lease,
        "id -un; echo \"$PATH\"; env | cut -d= -f1 | sort; pwd -P",
    );
    record(&evidence, "session", &output);
    let text = output["output"].as_str().unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], user());
    for variable in [
        "MACHINE_SERVICE_LEASE_ID",
        "MACHINE_SERVICE_WORKSPACE",
        "TMPDIR",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_BASE_URL",
        "LITELLM_CODEX_KEY",
        "CODEX_HOME",
    ] {
        assert!(lines.contains(&variable), "{variable} missing");
    }
    for variable in [
        "ANTHROPIC_AUTH_TOKEN",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "OPENAI_API_KEY",
    ] {
        assert!(!lines.contains(&variable), "{variable} leaked");
    }
    let workspace = PathBuf::from(acquired["workspace"].as_str().unwrap());
    assert_eq!(
        *lines.last().unwrap(),
        workspace
            .join("work")
            .canonicalize()
            .unwrap()
            .to_string_lossy()
    );
    let keys = instance.exec(
        &name,
        &lease,
        "echo $ANTHROPIC_API_KEY $LITELLM_CODEX_KEY; echo $ANTHROPIC_BASE_URL",
    );
    assert_eq!(
        keys["output"],
        "[redacted] [redacted]\nhttp://<gateway-host>\n"
    );
    let config = std::fs::read_to_string(workspace.join("codex/config.toml")).unwrap();
    assert!(config.contains("base_url = \"http://<gateway-host>/v1\""));
    let released = instance.release(&name, &lease);
    record(&evidence, "released", &released);
    let mut seen = released.to_string();
    for entry in walk(&instance.state) {
        seen.push_str(&String::from_utf8_lossy(
            &std::fs::read(entry).unwrap_or_default(),
        ));
    }
    assert!(!seen.contains(CLAUDE_KEY) && !seen.contains(CODEX_KEY));
    assert_eq!(
        std::fs::read(home().join(".codex/config.toml")).ok(),
        owner_codex,
        "the owner's Codex configuration changed"
    );
}

fn walk(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => {
                if let Ok(entries) = std::fs::read_dir(&path) {
                    stack.extend(entries.flatten().map(|entry| entry.path()));
                }
            }
            Ok(meta) if meta.is_file() => out.push(path),
            _ => {}
        }
    }
    out
}

/// AC-30, AC-31: release sweeps the lease's background process and its
/// setsid child, spares a stand-in CI process the lease started and every
/// process the test started itself, and records an Application Support
/// entry the lease made.
#[test]
#[ignore = "live TL-6: sweeps stand-in processes on this Mac through a second instance with exposure on; see the module documentation"]
fn live_ac30_ac31_sweep_and_inventory() {
    let live = Live::new();
    let evidence = evidence("sweep");
    // A stand-in CI runner: a tiny program named like the runner's worker.
    // A copied platform binary is refused at launch, so it is compiled.
    let source = live.root.join("worker.c");
    std::fs::write(
        &source,
        "#include <unistd.h>\n#include <stdlib.h>\nint main(int c, char **v) { sleep(c > 1 ? atoi(v[1]) : 60); return 0; }\n",
    )
    .unwrap();
    let runner = live.root.join("Runner.Worker");
    // The system's own toolchain: a `DEVELOPER_DIR` chosen for the Rust
    // build may name an Xcode older than the running macOS.
    let built = Command::new("/usr/bin/cc")
        .env_remove("DEVELOPER_DIR")
        .env_remove("SDKROOT")
        .arg("-o")
        .arg(&runner)
        .arg(&source)
        .status()
        .unwrap();
    assert!(built.success(), "cannot build the CI stand-in");
    let owner = StandIn::spawn(Path::new("/bin/sleep"), &["3600"]);
    let instance = live.start("state");
    let (name, lease, _) = instance.acquire("live-sweep", "none");
    let owner_during = StandIn::spawn(Path::new("/bin/sleep"), &["3600"]);
    let support = home()
        .join("Library/Application Support")
        .join(format!("machine-service-live-test-{lease}"));
    let script = format!(
        "nohup sleep 3600 >/dev/null 2>&1 & echo $!\n\
         /bin/sh -c '/usr/bin/perl -MPOSIX -e \"POSIX::setsid(); exec q(/bin/sleep), q(3600)\" & wait' >/dev/null 2>&1 & echo $!\n\
         {} 3600 >/dev/null 2>&1 & echo $!\n\
         mkdir -p '{}'\n",
        runner.display(),
        support.display()
    );
    struct Folder(PathBuf);
    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir(&self.0);
        }
    }
    let _support = Folder(support.clone());
    let started = instance.exec(&name, &lease, &script);
    let pids: Vec<i32> = started["output"]
        .as_str()
        .unwrap()
        .lines()
        .map(|line| line.trim().parse().unwrap())
        .collect();
    let guards: Vec<Leftover> = pids.iter().map(|pid| Leftover(*pid)).collect();
    let (background, parent, ci) = (pids[0], pids[1], pids[2]);
    std::thread::sleep(Duration::from_millis(500));
    let escaped: i32 = String::from_utf8_lossy(
        &Command::new("/usr/bin/pgrep")
            .args(["-P", &parent.to_string()])
            .output()
            .unwrap()
            .stdout,
    )
    .trim()
    .parse()
    .unwrap();
    let _escaped = Leftover(escaped);
    let busy = instance
        .request("GET", &format!("/machines/{name}"), None)
        .body;
    assert_eq!(busy["capabilities"]["ci_runner"]["busy"], true);

    let released = instance.release(&name, &lease);
    record(&evidence, "released", &released);
    for pid in [background, parent, escaped] {
        assert!(wait_dead(pid), "{pid} survived the release");
    }
    assert!(alive(ci), "the CI stand-in was ended");
    assert!(
        alive(owner.pid()) && alive(owner_during.pid()),
        "an owner stand-in was ended"
    );
    let hygiene = &released["hygiene"];
    assert!(hygiene["spared"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["pid"] == ci && entry["spared"] == "ci-runner"));
    let added = &hygiene["changes"]["parts"]["folders"]["added"];
    assert!(
        added.get(support.to_string_lossy().as_ref()).is_some(),
        "{added}"
    );
    // Formulae are listed even where an untrusted tap fails the casks.
    assert!(hygiene["changes"]["parts"]
        .get("homebrew_formulae")
        .is_some());
    // HY-5, AC-32: the stand-in job ran at release, so an interval is
    // recorded, closed at the release.
    let intervals = hygiene["ci_busy_intervals"].as_array().unwrap();
    assert!(!intervals.is_empty(), "{hygiene}");
    assert!(intervals.iter().all(|interval| interval["to"].is_number()));
    drop(guards);
}

/// The PIDs of processes named exactly `name` of this user.
fn pids_named(name: &str) -> Vec<i32> {
    let output = Command::new("/usr/bin/pgrep")
        .args(["-x", "-u", &user(), name])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .filter_map(|pid| pid.parse().ok())
        .collect()
}

/// AC-30, HY-3 (DC-73): `open -a TextEdit` from a lease is quit at release
/// under the `application` rule. When TextEdit was already running before
/// the lease, `open` only activates the owner's TextEdit, which must
/// survive: the sweep never ends an application it did not launch.
#[test]
#[ignore = "live TL-6: opens TextEdit through a second instance with exposure on and quits it at release unless it was already running; see the module documentation"]
fn live_ac30_open_a_textedit() {
    let live = Live::new();
    let evidence = evidence("textedit");
    let before = pids_named("TextEdit");
    let instance = live.start("state");
    let (name, lease, _) = instance.acquire("live-textedit", "none");
    let opened = instance.exec(&name, &lease, "/usr/bin/open -g -a TextEdit && echo opened");
    assert_eq!(opened["output"].as_str().unwrap().trim(), "opened");
    let deadline = Instant::now() + Duration::from_secs(30);
    let launched = loop {
        let now: Vec<i32> = pids_named("TextEdit")
            .into_iter()
            .filter(|pid| !before.contains(pid))
            .collect();
        if !now.is_empty() || !before.is_empty() || Instant::now() >= deadline {
            break now;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let _guards: Vec<Leftover> = launched.iter().map(|pid| Leftover(*pid)).collect();
    let released = instance.release(&name, &lease);
    record(&evidence, "released", &released);
    let swept = released["hygiene"]["swept"].as_array().unwrap().clone();
    if before.is_empty() {
        assert_eq!(launched.len(), 1, "TextEdit did not start: {launched:?}");
        let pid = launched[0];
        assert!(wait_dead(pid), "TextEdit survived the release");
        let entry = swept
            .iter()
            .find(|entry| entry["pid"] == pid)
            .unwrap_or_else(|| panic!("TextEdit is not in the hygiene record: {swept:?}"));
        assert_eq!(entry["rule"], "application");
        assert_eq!(entry["ended"], true);
    } else {
        eprintln!("TextEdit was running before the lease; checking that it survives");
        for pid in &before {
            assert!(alive(*pid), "the owner's TextEdit was ended");
            assert!(swept.iter().all(|entry| entry["pid"] != *pid));
        }
    }
}

/// CL-2, CL-3, FD-4 (AC-28): the real `machctl` against a second instance
/// acquires by capability and by name, remembers the lease, moves bytes
/// both ways with `--bytes`, and releases by the remembered lease. Consul
/// is a closed port, so no other Mac is ever asked or leased.
#[test]
#[ignore = "live TL-6: acquires this Mac through machctl and a second instance with exposure on; see the module documentation"]
fn live_cl2_fd4_machctl_against_a_second_instance() {
    let live = Live::new();
    let evidence = evidence("machctl");
    let instance = live.start("state");
    let name = instance.name();
    let cli_state = live.root.join("cli-state");
    let cli_home = live.root.join("cli-home");
    std::fs::create_dir_all(&cli_home).unwrap();
    let env = vec![
        (
            "MACHINE_SERVICE_PORT".to_string(),
            instance.port.to_string(),
        ),
        (
            "MACHINE_SERVICE_STATE".to_string(),
            cli_state.display().to_string(),
        ),
        ("HOME".to_string(), cli_home.display().to_string()),
    ];
    let machctl = |arguments: &[&str]| {
        let output = common::run_machctl(arguments, &env);
        let text = format!(
            "machctl {arguments:?}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        (output, text)
    };

    let (output, text) = machctl(&[
        "acquire",
        "--purpose",
        "live-cli",
        "--require",
        "durable",
        "--env",
        "none",
        "--ttl-hours",
        "1",
    ]);
    std::fs::write(evidence.join("acquire-require.txt"), &text).unwrap();
    assert!(output.status.success(), "{text}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(&format!("chose {name} (durable")),
        "{text}"
    );
    let lease: Value = serde_json::from_slice(&output.stdout).unwrap();
    let lease_id = lease["lease_id"].as_str().unwrap().to_string();
    let remembered = std::fs::read_to_string(cli_state.join("cli-leases.json")).unwrap();
    assert!(remembered.contains(&lease_id), "{remembered}");

    let (output, text) = machctl(&["exec", &name, "--", "/bin/echo", "remembered"]);
    assert!(output.status.success(), "{text}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "remembered\n");

    let source = live.root.join("payload");
    std::fs::create_dir_all(source.join("inner")).unwrap();
    std::fs::write(source.join("inner/data.txt"), "live bytes").unwrap();
    let (output, text) = machctl(&["push", "--bytes", &name, source.to_str().unwrap(), "."]);
    assert!(output.status.success(), "{text}");
    let back = live.root.join("back");
    std::fs::create_dir_all(&back).unwrap();
    let (output, text) = machctl(&["pull", "--bytes", &name, "payload", back.to_str().unwrap()]);
    assert!(output.status.success(), "{text}");
    assert_eq!(
        std::fs::read_to_string(back.join("payload/inner/data.txt")).unwrap(),
        "live bytes"
    );

    let (output, text) = machctl(&["release", &name]);
    std::fs::write(evidence.join("release.txt"), &text).unwrap();
    assert!(output.status.success(), "{text}");
    assert!(instance.current().is_none());

    let (output, text) = machctl(&[
        "acquire",
        "--purpose",
        "live-cli-named",
        "--machine",
        &name,
        "--env",
        "none",
        "--ttl-hours",
        "1",
    ]);
    assert!(output.status.success(), "{text}");
    let (output, text) = machctl(&["release", &name]);
    assert!(output.status.success(), "{text}");
    let remembered = std::fs::read_to_string(cli_state.join("cli-leases.json")).unwrap();
    assert!(!remembered.contains(&lease_id), "{remembered}");
    assert!(
        remembered.contains("\"leases\": []"),
        "every lease was forgotten: {remembered}"
    );
}

/// AC-33 and DM-7: a restart keeps the lease and its process; a release
/// during a running command preempts it, and ends the kept process.
#[test]
#[ignore = "live TL-6: restarts a second instance on this Mac during a durable lease; see the module documentation"]
fn live_ac33_restart_and_release_during_a_command() {
    let live = Live::new();
    let evidence = evidence("restart");
    let mut instance = live.start("state");
    let (name, lease, _) = instance.acquire("live-restart", "none");
    let background: i32 = instance.exec(&name, &lease, "sleep 3600 >/dev/null 2>&1 & echo $!")
        ["output"]
        .as_str()
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let _guard = Leftover(background);
    instance.stop();
    assert!(alive(background));
    drop(instance);
    let instance = live.start("state");
    let machine = instance
        .request("GET", &format!("/machines/{name}"), None)
        .body;
    assert_eq!(machine["state"], "leased");
    assert_eq!(machine["lease_id"], lease.as_str());

    let port = instance.port;
    let path = format!("/machines/{name}/exec");
    let body = json!({"lease_id": lease, "argv": ["sleep", "120"]});
    let running =
        std::thread::spawn(move || http_timeout(port, "POST", &path, Some(&body), None, LONG));
    std::thread::sleep(Duration::from_secs(1));
    let released = instance.release(&name, &lease);
    record(&evidence, "released", &released);
    let answer = running.join().unwrap();
    assert_eq!(answer.status, 409, "{}", answer.body);
    assert!(wait_dead(background));
    assert_eq!(
        instance
            .request("GET", &format!("/machines/{name}"), None)
            .body["state"],
        "available"
    );
}

// ------------------------------------------------- applications, from TL-2

/// Whether this process runs in a logged-in GUI (Aqua) session, where
/// Launch Services can open an application.
fn in_gui_session() -> bool {
    Command::new("/bin/launchctl")
        .arg("managername")
        .output()
        .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).trim() == "Aqua")
}

/// Build a minimal regular application, signed ad hoc, that quits when
/// asked as any application does. Answers its bundle and executable.
fn stand_in_app(root: &Path, name: &str, bundle_id: &str) -> (PathBuf, PathBuf) {
    let app = root.join(format!("{name}.app"));
    let macos = app.join("Contents/MacOS");
    std::fs::create_dir_all(&macos).unwrap();
    let source = root.join(format!("{name}.m"));
    std::fs::write(
        &source,
        "#import <Cocoa/Cocoa.h>\nint main(void) { @autoreleasepool { [NSApplication sharedApplication]; [NSApp setActivationPolicy:NSApplicationActivationPolicyRegular]; [NSApp run]; } return 0; }\n",
    )
    .unwrap();
    let executable = macos.join(name);
    // The system's own toolchain and SDK: a `DEVELOPER_DIR` chosen for the
    // Rust build may name an Xcode older than the running macOS.
    let built = Command::new("/usr/bin/clang")
        .env_remove("DEVELOPER_DIR")
        .env_remove("SDKROOT")
        .args(["-fobjc-arc", "-framework", "Cocoa", "-o"])
        .arg(&executable)
        .arg(&source)
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    std::fs::write(
        app.join("Contents/Info.plist"),
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\
<key>CFBundleIdentifier</key><string>{bundle_id}</string>\
<key>CFBundleExecutable</key><string>{name}</string>\
<key>CFBundleName</key><string>{name}</string>\
<key>CFBundlePackageType</key><string>APPL</string>\
<key>CFBundleVersion</key><string>1</string>\
</dict></plist>\n"
        ),
    )
    .unwrap();
    let signed = Command::new("/usr/bin/codesign")
        .args(["--force", "--sign", "-"])
        .arg(&app)
        .output()
        .unwrap();
    assert!(
        signed.status.success(),
        "{}",
        String::from_utf8_lossy(&signed.stderr)
    );
    (app, executable)
}

/// The PIDs running `executable`.
fn pids_of(executable: &Path) -> Vec<i32> {
    let output = Command::new("/usr/bin/pgrep")
        .arg("-f")
        .arg(executable)
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .filter_map(|pid| pid.parse().ok())
        .collect()
}

fn wait_for_pid(executable: &Path) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(pid) = pids_of(executable).first() {
            return *pid;
        }
        assert!(
            Instant::now() < deadline,
            "{} never started",
            executable.display()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Removes a stand-in application's Launch Services registration.
struct Registered(PathBuf);

impl Drop for Registered {
    fn drop(&mut self) {
        let _ = Command::new(
            "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister",
        )
        .arg("-u")
        .arg(&self.0)
        .output();
    }
}

/// AC-30, HY-3 (DC-73), live: an application the lease opened with `open`
/// is asked to quit at release and recorded with the `application` rule;
/// an application the owner opened during the lease, which no rule may
/// touch, keeps running. It builds, registers and opens two stand-in
/// applications in this Mac's desktop session, which is why it is not in
/// the default suite (DC-1xx-ISO-3).
#[test]
#[ignore = "live TL-6: builds, registers and opens stand-in applications in this Mac's desktop session through a second instance with exposure on; see the module documentation"]
fn live_ac30_release_quits_the_applications_the_lease_opened_and_only_those() {
    if !in_gui_session() {
        eprintln!("skipped: no logged-in GUI session to open applications in");
        return;
    }
    let live = Live::new();
    let tag = &uuid_like();
    let (lease_app, lease_exe) = stand_in_app(
        &live.root,
        "LeaseStandIn",
        &format!("dev.machine-service.test.lease.{tag}"),
    );
    let (owner_app, owner_exe) = stand_in_app(
        &live.root,
        "OwnerStandIn",
        &format!("dev.machine-service.test.owner.{tag}"),
    );
    let _registrations = (Registered(lease_app.clone()), Registered(owner_app.clone()));
    let instance = live.start("state");
    let (name, lease, _) = instance.acquire("live-apps", "none");

    // The owner opens an application during the lease, outside it.
    let opened = Command::new("/usr/bin/open")
        .args(["-g", "-n"])
        .arg(&owner_app)
        .env_remove("MACHINE_SERVICE_LEASE_ID")
        .status()
        .unwrap();
    assert!(opened.success());
    let owner_pid = wait_for_pid(&owner_exe);
    let _owner_guard = Leftover(owner_pid);
    // The lease opens one through Launch Services.
    let output = instance.exec(
        &name,
        &lease,
        &format!(
            "/usr/bin/open -g -n '{}' && echo opened",
            lease_app.display()
        ),
    );
    assert_eq!(output["output"].as_str().unwrap().trim(), "opened");
    let lease_pid = wait_for_pid(&lease_exe);
    let _lease_guard = Leftover(lease_pid);

    let released = instance.release(&name, &lease);
    let swept = released["hygiene"]["swept"].as_array().unwrap().clone();
    let entry = swept
        .iter()
        .find(|entry| entry["pid"] == lease_pid)
        .unwrap_or_else(|| panic!("the lease's application was not swept: {swept:?}"));
    assert_eq!(entry["rule"], "application");
    assert_eq!(entry["ended"], true);
    assert_eq!(entry["signal"], "QUIT", "asked to quit first: {entry}");
    assert!(wait_dead(lease_pid));
    assert!(alive(owner_pid), "the owner's application was ended");
    assert!(swept.iter().all(|entry| entry["pid"] != owner_pid));
}

/// A short random tag for bundle identifiers.
fn uuid_like() -> String {
    format!(
        "{:x}{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    )
}
