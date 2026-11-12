//! Durable-machine leases through the real daemon (`docs/arch/test-layers.md`,
//! TL-2; AC-20 to AC-27, AC-30, AC-31, AC-33, AC-43, AC-44).
//!
//! Each test starts the real `machine-service` binary as an isolated test
//! child (`common::daemon_command`): a cleared environment, a fixture
//! `HOME` whose login profile sets `PATH` and fake owner credentials, its
//! own state and configuration folders with fake agent keys and a
//! placeholder endpoint, its own lease-lock folder
//! (`MACHINE_SERVICE_DURABLE_LOCK_DIR`, honoured only by debug builds), so
//! no test shares the host-wide lock with another or with an installed
//! instance, its own name file and its own tool table.
//!
//! Commands really run on this Mac, as this user, in their own process
//! sessions, and end with the test. The release sweep of a test build sees
//! only the processes the daemon started and the rows of the test's process
//! fixture, and signals nothing else (DC-253): stand-in processes the
//! lease did not start survive, and the fixture's rows prove the rules
//! without any real application or process of the owner's session. The
//! applications a real lease opens are proved only in the opt-in live layer
//! (`live_durable.rs`).

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::{canonical_dir, free_port, http_timeout, try_http, write_exec};
use serde_json::{json, Value};

const CLAUDE_KEY: &str = "sk-fake-claude-code-key-0123456789";
const CODEX_KEY: &str = "sk-fake-codex-key-9876543210";
const OWNER_TOKEN: &str = "owner-auth-token-fake-555";
const LONG: Duration = Duration::from_secs(180);

/// One isolated durable-machine fixture.
struct Fixture {
    /// One lifeline for every daemon of the fixture: what a stopped daemon
    /// started, a restarted one sweeps (DC-256).
    lifeline: common::Lifeline,
    _dir: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    config: PathBuf,
    lock: PathBuf,
}

/// A running daemon.
struct Daemon {
    child: Option<common::Owned>,
    port: u16,
    state: PathBuf,
    err: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = canonical_dir(dir.path());
        let home = root.join("home");
        let config = root.join("config");
        let lock = root.join("lock");
        let bin = root.join("bin");
        std::fs::create_dir_all(home.join("Library/Application Support")).unwrap();
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(home.join(".codex/config.toml"), "model = \"owner\"\n").unwrap();
        std::fs::write(
            home.join(".zprofile"),
            format!(
                "export PATH=\"{}:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin\"\n\
                 export ANTHROPIC_AUTH_TOKEN={OWNER_TOKEN}\n\
                 export CLAUDE_CODE_OAUTH_TOKEN={OWNER_TOKEN}\n\
                 export OPENAI_API_KEY={OWNER_TOKEN}\n",
                bin.display()
            ),
        )
        .unwrap();
        write_exec(
            &bin,
            "brew",
            &format!(
                r#"#!/bin/sh
db="{root}/brew-formulae"
case "$1 $2" in
  "list --formula") [ -e "{root}/brew-fails" ] && {{ echo broken >&2; exit 1; }}; cat "$db" 2>/dev/null; exit 0 ;;
  "list --cask") exit 0 ;;
  "install "*) printf '%s 1.0\n' "$2" >> "$db"; exit 0 ;;
esac
exit 0
"#,
                root = root.display()
            ),
        );
        std::fs::write(root.join("brew-formulae"), "jq 1.7\n").unwrap();
        let fixture = Self {
            lifeline: common::Lifeline::new(),
            _dir: dir,
            root,
            home,
            config,
            lock,
        };
        fixture.write_agents(true, true, true);
        fixture
    }

    fn agents(&self) -> PathBuf {
        self.config.join("agents")
    }

    fn write_agents(&self, claude: bool, codex: bool, endpoints: bool) {
        let dir = self.agents();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let write = |name: &str, text: &str, mode: u32| {
            let path = dir.join(name);
            std::fs::write(&path, text).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        if claude {
            write(
                "claude-code.env",
                &format!("ANTHROPIC_API_KEY={CLAUDE_KEY}\n"),
                0o600,
            );
        }
        if codex {
            write(
                "codex.env",
                &format!("LITELLM_CODEX_KEY={CODEX_KEY}\n"),
                0o600,
            );
        }
        if endpoints {
            write(
                "endpoints.json",
                r#"{"claude-code": {"base_url": "http://<gateway-host>"}, "codex": {"base_url": "http://<gateway-host>/v1"}}"#,
                0o600,
            );
        }
    }

    fn state(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn workspace_root(&self) -> PathBuf {
        self.home.join("machine-service/leases")
    }

    fn start(&self, state: &str, exposed: bool) -> Daemon {
        self.try_start_with(state, exposed, &[])
            .unwrap_or_else(|_| panic!("daemon did not start"))
    }

    /// Start a daemon with `extra` environment, or answer the last
    /// attempt's standard-error file when none started.
    fn try_start_with(
        &self,
        state: &str,
        exposed: bool,
        extra: &[(&str, &str)],
    ) -> Result<Daemon, PathBuf> {
        let state_dir = self.state(state);
        std::fs::create_dir_all(&state_dir).unwrap();
        let mut last_err = state_dir.join("daemon.err");
        for _ in 0..16 {
            let port = free_port();
            let err = state_dir.join(format!("daemon-{port}.err"));
            let mut command = common::daemon_command_on(&self.root, &self.lifeline);
            command
                .env("HOME", &self.home)
                .env("USER", user())
                .env("LOGNAME", user())
                .env("SHELL", "/bin/zsh")
                .env("MACHINE_SERVICE_STATE", &state_dir)
                .env("MACHINE_SERVICE_PORT", port.to_string())
                .env("MACHINE_SERVICE_HOST", "127.0.0.1")
                .env("MACHINE_SERVICE_CONFIG_DIR", &self.config)
                .env("MACHINE_SERVICE_DURABLE_LOCK_DIR", &self.lock)
                .env("PILOT_REPO", self.root.join("pilot"))
                .env("TART_HOME", self.root.join("tart"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::from(std::fs::File::create(&err).unwrap()));
            if exposed {
                command.env("MACHINE_SERVICE_EXPOSE_DURABLE", "on");
            }
            command.envs(extra.iter().copied());
            last_err = err.clone();
            let mut child = command.spawn().expect("spawn daemon");
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                if let Ok(Some(_)) = child.try_wait() {
                    break;
                }
                if let Ok(response) = try_http(port, "GET", "/health", None, None) {
                    if response.status == 200 {
                        return Ok(Daemon {
                            child: Some(child),
                            port,
                            state: state_dir,
                            err,
                        });
                    }
                }
                if Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        Err(last_err)
    }
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

impl Daemon {
    fn request(&self, method: &str, path: &str, body: Option<&Value>) -> common::Response {
        http_timeout(self.port, method, path, body, None, LONG)
    }

    fn instance(&self) -> String {
        self.request("GET", "/health", None).body["instance"]
            .as_str()
            .expect("instance")
            .to_string()
    }

    fn acquire(&self, body: Value) -> common::Response {
        self.request("POST", "/acquire", Some(&body))
    }

    fn acquired(&self, purpose: &str, env: &str) -> (String, String) {
        let name = self.instance();
        let response = self.acquire(json!({"machine": name, "purpose": purpose, "env": env}));
        assert_eq!(response.status, 200, "{}\n{}", response.body, self.stderr());
        let lease = response.body["lease_id"].as_str().unwrap().to_string();
        (name, lease)
    }

    fn exec(&self, name: &str, lease: &str, script: &str) -> common::Response {
        self.request(
            "POST",
            &format!("/machines/{name}/exec"),
            Some(&json!({"lease_id": lease, "argv": ["/bin/sh", "-c", script]})),
        )
    }

    fn output(&self, name: &str, lease: &str, script: &str) -> String {
        let response = self.exec(name, lease, script);
        assert_eq!(response.status, 200, "{}", response.body);
        response.body["output"].as_str().unwrap().to_string()
    }

    fn release(&self, name: &str, lease: &str) -> common::Response {
        self.request(
            "POST",
            &format!("/machines/{name}/release"),
            Some(&json!({"lease_id": lease, "reason": "test"})),
        )
    }

    fn machine(&self, name: &str) -> Value {
        self.request("GET", &format!("/machines/{name}"), None).body
    }

    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.err).unwrap_or_default()
    }

    fn current_lease(&self) -> Option<String> {
        let text = std::fs::read_to_string(self.state.join("durable.json")).ok()?;
        serde_json::from_str::<Value>(&text).ok()?["current"]
            .as_str()
            .map(str::to_string)
    }

    /// Stop the daemon with SIGTERM only; the lease's sessions survive
    /// until the answered handle drops, which ends what the daemon started.
    fn stop(&mut self) -> Option<common::Owned> {
        let mut child = self.child.take()?;
        child.signal(libc::SIGTERM, Duration::from_secs(30));
        Some(child)
    }

    /// The process fixture of the daemon's sweep (DC-253).
    fn processes(&self) -> PathBuf {
        self.state.parent().unwrap().join("processes.json")
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // Never leave a lease's processes behind.
        if self.child.is_some() {
            if let Some(lease) = self.current_lease() {
                if let Ok(health) = try_http(self.port, "GET", "/health", None, None) {
                    if let Some(name) = health.body["instance"].as_str() {
                        let _ = common::try_http_timeout(
                            self.port,
                            "POST",
                            &format!("/machines/{name}/release"),
                            Some(&json!({"lease_id": lease, "reason": "test-cleanup"})),
                            None,
                            LONG,
                        );
                    }
                }
            }
        }
        if let Some(mut child) = self.child.take() {
            child.terminate(Duration::from_secs(5));
        }
    }
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the PID exists.
    let exists = unsafe { libc::kill(pid, 0) } == 0;
    if !exists {
        return false;
    }
    let state = Command::new("/bin/ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_default();
    !state.is_empty() && !state.starts_with('Z')
}

fn wait_dead(pid: i32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

fn kill(pid: i32) {
    // SAFETY: only PIDs this test started are passed.
    unsafe { libc::kill(pid, libc::SIGKILL) };
}

/// A stand-in process this test started itself, ended with everything it
/// started when the test ends, even on a failed assertion.
struct StandIn {
    child: common::Owned,
}

impl StandIn {
    fn spawn(program: &str, arguments: &[&str]) -> Self {
        let mut command = common::harness::command(program);
        command
            .args(arguments)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        Self {
            child: command.spawn().expect("spawn stand-in"),
        }
    }

    fn pid(&self) -> i32 {
        self.child.id() as i32
    }
}

/// A PID a lease started that the test must clean up itself.
struct Leftover(i32);

impl Drop for Leftover {
    fn drop(&mut self) {
        let _ = Command::new("/usr/bin/pkill")
            .args(["-KILL", "-P", &self.0.to_string()])
            .status();
        kill(self.0);
    }
}

/// Every file under `root`, as text.
fn tree_text(root: &Path) -> String {
    let mut out = String::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&path) {
                stack.extend(entries.flatten().map(|entry| entry.path()));
            }
        } else if meta.is_file() {
            out.push_str(&String::from_utf8_lossy(
                &std::fs::read(&path).unwrap_or_default(),
            ));
        }
    }
    out
}

// ------------------------------------------------------------------ AC-43

/// AC-43: with exposure off, no durable machine is listed, `/health` has no
/// durable kind, and an acquisition naming the Mac is refused with 404
/// before any lock or workspace is touched. Turned on, the machine is
/// listed as available.
#[test]
fn ac43_exposure_is_off_unless_turned_on() {
    let fixture = Fixture::new();
    let daemon = fixture.start("state", false);
    let name = daemon.instance();
    let health = daemon.request("GET", "/health", None);
    assert!(!health.body["kinds"]
        .as_array()
        .unwrap()
        .contains(&json!("durable")));
    let machines = daemon.request("GET", "/machines", None).body;
    assert!(machines["machines"].get(&name).is_none(), "{machines}");
    for machine in [name.as_str(), "some-other-mac"] {
        let refused = daemon.acquire(json!({"machine": machine, "purpose": "metal-check"}));
        assert_eq!(refused.status, 404, "{}", refused.body);
        assert_eq!(
            refused.body["error"],
            format!("durable machine not exposed on {name}")
        );
    }
    let exec = daemon.request(
        "POST",
        &format!("/machines/{name}/exec"),
        Some(&json!({"lease_id": "x", "argv": ["true"]})),
    );
    assert_eq!(exec.status, 404);
    assert!(!fixture.lock.exists(), "no lock is touched");
    assert!(
        !fixture.workspace_root().exists(),
        "no workspace is touched"
    );
    drop(daemon);

    let daemon = fixture.start("state", true);
    let health = daemon.request("GET", "/health", None);
    assert!(health.body["kinds"]
        .as_array()
        .unwrap()
        .contains(&json!("durable")));
    let record = &daemon.request("GET", "/machines", None).body["machines"][&name];
    assert_eq!(record["state"], "available", "{record}");
    assert_eq!(record["kind"], "durable");
    assert_eq!(record["instance"], name.as_str());
}

// ------------------------------------------------------------------ AC-20

/// AC-20: a durable machine is acquired by name, with a new lease, an empty
/// workspace and the state leased; a request with both machine and image,
/// or with cpu, is refused with 400 and reserves nothing.
#[test]
fn ac20_durable_machine_is_acquired_by_name() {
    let fixture = Fixture::new();
    let daemon = fixture.start("state", true);
    let name = daemon.instance();
    let response = daemon.acquire(json!({"machine": name, "purpose": "metal-check"}));
    assert_eq!(response.status, 200, "{}", response.body);
    let record = &response.body;
    assert_eq!(record["kind"], "durable");
    assert_eq!(record["name"], name.as_str());
    assert_eq!(record["state"], "leased");
    assert_eq!(record["image"], Value::Null);
    let lease = record["lease_id"].as_str().unwrap().to_string();
    assert_eq!(lease.len(), 32);
    let workspace = PathBuf::from(record["workspace"].as_str().unwrap());
    assert_eq!(workspace, fixture.workspace_root().join(&lease));
    let mut folders: Vec<String> = std::fs::read_dir(&workspace)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    folders.sort();
    assert_eq!(folders, vec!["codex", "tmp", "work"]);
    assert_eq!(
        std::fs::read_dir(workspace.join("work")).unwrap().count(),
        0
    );
    assert_eq!(
        std::fs::metadata(&workspace).unwrap().permissions().mode() & 0o777,
        0o700
    );
    for field in [
        "purpose",
        "created_at",
        "ttl_hours",
        "ttl_expires_at",
        "grace_until",
        "warned",
        "env",
        "ip",
        "cpu",
        "memory_mb",
        "disk_gb",
        "configuration",
        "image_kind",
        "capabilities",
    ] {
        assert!(record.get(field).is_some(), "{field}");
    }
    assert_eq!(daemon.machine(&name)["state"], "leased");
    assert_eq!(record["capabilities"]["schema_version"], 1);

    let both = daemon.acquire(json!({"machine": name, "image": "macos26", "purpose": "other"}));
    assert_eq!(both.status, 400, "{}", both.body);
    let cpu = daemon.acquire(json!({"machine": name, "cpu": 4, "purpose": "other"}));
    assert_eq!(cpu.status, 400);
    assert!(cpu.body["error"].as_str().unwrap().contains("cpu"));
    assert_eq!(daemon.machine(&name)["lease_id"], lease.as_str());

    // One lease at a time on one instance, too.
    let again = daemon.acquire(json!({"machine": name, "purpose": "other"}));
    assert_eq!(again.status, 409);
    assert!(again.body["error"].as_str().unwrap().starts_with(&format!(
        "durable machine leased: {name} (purpose metal-check, until "
    )));

    let released = daemon.release(&name, &lease);
    assert_eq!(released.status, 200, "{}", released.body);
    assert!(!workspace.exists());
    assert_eq!(daemon.machine(&name)["state"], "available");
}

// ------------------------------------------------------------------ AC-21

/// AC-21: two instances on one Mac with their own state directories share
/// the lease lock; the second refuses with 409 naming purpose and expiry,
/// and the first lease is unchanged.
#[test]
fn ac21_one_lease_at_a_time_across_instances() {
    let fixture = Fixture::new();
    let first = fixture.start("state-a", true);
    let second = fixture.start("state-b", true);
    let (name, lease) = first.acquired("metal-check", "none");
    let refused = second.acquire(json!({"machine": name, "purpose": "other"}));
    assert_eq!(refused.status, 409, "{}", refused.body);
    let error = refused.body["error"].as_str().unwrap();
    assert!(
        error.starts_with(&format!(
            "durable machine leased: {name} (purpose metal-check, until 20"
        )),
        "{error}"
    );
    assert_eq!(first.machine(&name)["lease_id"], lease.as_str());
    assert_eq!(first.machine(&name)["state"], "leased");
    assert_eq!(second.machine(&name)["state"], "available");

    assert_eq!(first.release(&name, &lease).status, 200);
    let (_, second_lease) = second.acquired("other", "none");
    assert_eq!(second.release(&name, &second_lease).status, 200);
}

// ------------------------------------------------------------------ AC-22

/// AC-22: an exec with a released lease's ID, or without one, is refused
/// with 409 and runs nothing.
#[test]
fn ac22_stale_or_missing_lease_id_is_refused() {
    let fixture = Fixture::new();
    let daemon = fixture.start("state", true);
    let (name, lease_a) = daemon.acquired("first", "none");
    assert_eq!(daemon.release(&name, &lease_a).status, 200);
    let (_, lease_b) = daemon.acquired("second", "none");
    let marker = fixture.root.join("ran");
    let stale = daemon.exec(&name, &lease_a, &format!("touch {}", marker.display()));
    assert_eq!(stale.status, 409, "{}", stale.body);
    let missing = daemon.request(
        "POST",
        &format!("/machines/{name}/exec"),
        Some(&json!({"argv": ["touch", marker.to_string_lossy()]})),
    );
    assert_eq!(missing.status, 409, "{}", missing.body);
    for path in ["heartbeat", "release", "push", "pull"] {
        let refused = daemon.request(
            "POST",
            &format!("/machines/{name}/{path}"),
            Some(&json!({"lease_id": lease_a})),
        );
        assert_eq!(refused.status, 409, "{path}: {}", refused.body);
    }
    assert!(!marker.exists(), "no command ran");
    assert_eq!(daemon.release(&name, &lease_b).status, 200);
}

// ------------------------------------------------------------------ AC-23

/// AC-23: a command runs as this user, with the login shell's PATH, the
/// lease's variables, none of the owner's credentials, in `work/`.
#[test]
fn ac23_command_runs_as_the_user_without_owner_credentials() {
    let fixture = Fixture::new();
    let daemon = fixture.start("state", true);
    let (name, lease) = daemon.acquired("session", "default");
    let output = daemon.output(
        &name,
        &lease,
        "id -un; echo \"$PATH\"; env | cut -d= -f1; pwd -P",
    );
    let lines: Vec<&str> = output.lines().collect();
    assert_eq!(lines[0], user());
    assert!(lines[1].contains("/opt/homebrew/bin"), "{}", lines[1]);
    for variable in [
        "MACHINE_SERVICE_LEASE_ID",
        "MACHINE_SERVICE_WORKSPACE",
        "TMPDIR",
    ] {
        assert!(lines.contains(&variable), "{variable} missing: {output}");
    }
    for variable in [
        "ANTHROPIC_AUTH_TOKEN",
        "OPENAI_API_KEY",
        "CLAUDE_CODE_OAUTH_TOKEN",
    ] {
        assert!(!lines.contains(&variable), "{variable} leaked");
    }
    assert!(!output.contains(OWNER_TOKEN));
    let work = fixture.workspace_root().join(&lease).join("work");
    assert_eq!(
        *lines.last().unwrap(),
        work.canonicalize().unwrap().to_string_lossy()
    );
    let names = daemon.machine(&name);
    assert!(names.get("environment_names").is_none());
    assert_eq!(daemon.release(&name, &lease).status, 200);
}

// ------------------------------------------------------------------ AC-24

/// AC-24: with env default, Claude Code's gateway URL and key reach a
/// command and the owner's tokens do not; with env none, neither does.
#[test]
fn ac24_claude_code_gets_the_machine_key_and_endpoint() {
    let fixture = Fixture::new();
    let daemon = fixture.start("state", true);
    let (name, lease) = daemon.acquired("claude", "default");
    let output = daemon.output(
        &name,
        &lease,
        "printf '%s\\n' \"$ANTHROPIC_BASE_URL\"; [ -n \"$ANTHROPIC_API_KEY\" ] && echo key-set; \
         env | grep -c -e '^ANTHROPIC_AUTH_TOKEN=' -e '^CLAUDE_CODE_OAUTH_TOKEN=' || true",
    );
    assert_eq!(output, "http://<gateway-host>\nkey-set\n0\n");
    assert_eq!(daemon.release(&name, &lease).status, 200);

    let (_, lease) = daemon.acquired("claude", "none");
    let output = daemon.output(
        &name,
        &lease,
        "env | grep -c -e '^ANTHROPIC_BASE_URL=' -e '^ANTHROPIC_API_KEY=' || true",
    );
    assert_eq!(output, "0\n");
    assert_eq!(daemon.release(&name, &lease).status, 200);
}

// ------------------------------------------------------------------ AC-25

/// AC-25: Codex gets a per-session CODEX_HOME whose config.toml selects the
/// gateway as written, names the key's variable, and holds no key; the
/// owner's `~/.codex/config.toml` is unchanged.
#[test]
fn ac25_codex_gets_a_per_session_custom_provider() {
    let fixture = Fixture::new();
    let owner_config = std::fs::read(fixture.home.join(".codex/config.toml")).unwrap();
    let daemon = fixture.start("state", true);
    let (name, lease) = daemon.acquired("codex", "default");
    let workspace = fixture.workspace_root().join(&lease);
    let output = daemon.output(
        &name,
        &lease,
        "printf '%s\\n' \"$CODEX_HOME\"; [ -n \"$LITELLM_CODEX_KEY\" ] && echo key-set",
    );
    assert_eq!(
        output,
        format!("{}\nkey-set\n", workspace.join("codex").display())
    );
    let config = std::fs::read_to_string(workspace.join("codex/config.toml")).unwrap();
    assert!(config.contains("model_provider = \"machine-service-litellm\""));
    assert!(config.contains("base_url = \"http://<gateway-host>/v1\""));
    assert!(config.contains("env_key = \"LITELLM_CODEX_KEY\""));
    let text = tree_text(&workspace);
    assert!(!text.contains(CODEX_KEY) && !text.contains(CLAUDE_KEY));
    assert_eq!(
        std::fs::read(fixture.home.join(".codex/config.toml")).unwrap(),
        owner_config
    );
    assert_eq!(daemon.release(&name, &lease).status, 200);
}

// ------------------------------------------------------------------ AC-26

/// AC-26: a key printed by a command comes back redacted, and neither key
/// appears in the lease record, the log, `/machines`, the release report,
/// an error or any file the service wrote.
#[test]
fn ac26_keys_are_never_printed_logged_or_returned() {
    let fixture = Fixture::new();
    let daemon = fixture.start("state", true);
    let (name, lease) = daemon.acquired("keys", "default");
    let output = daemon.output(&name, &lease, "echo $ANTHROPIC_API_KEY $LITELLM_CODEX_KEY");
    assert_eq!(output, "[redacted] [redacted]\n");
    let mut seen = vec![
        daemon.request("GET", "/machines", None).body.to_string(),
        daemon.machine(&name).to_string(),
    ];
    let timed_out = daemon.request(
        "POST",
        &format!("/machines/{name}/exec"),
        Some(&json!({"lease_id": lease, "timeout": 1, "argv": ["/bin/sh", "-c", "echo $ANTHROPIC_API_KEY; sleep 5"]})),
    );
    assert_eq!(timed_out.status, 409);
    seen.push(timed_out.body.to_string());
    let released = daemon.release(&name, &lease);
    seen.push(released.body.to_string());
    seen.push(tree_text(&daemon.state));
    seen.push(daemon.stderr());
    for text in seen {
        assert!(!text.contains(CLAUDE_KEY), "Claude Code key leaked");
        assert!(!text.contains(CODEX_KEY), "Codex key leaked");
    }
}

// ------------------------------------------------------------------ AC-27

/// AC-27: an unsafe key file and a missing one are reported, not loaded,
/// and never refuse an acquisition.
#[test]
fn ac27_unsafe_or_missing_key_is_reported_not_loaded() {
    let fixture = Fixture::new();
    fixture.write_agents(false, true, true);
    std::fs::set_permissions(
        fixture.agents().join("codex.env"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    let daemon = fixture.start("state", true);
    let name = daemon.instance();
    let agents = &daemon.machine(&name)["capabilities"]["agents"];
    assert_eq!(agents["codex"]["key"], "unsafe-mode");
    assert_eq!(agents["claude_code"]["key"], "absent");
    let (_, lease) = daemon.acquired("unsafe", "default");
    let output = daemon.output(
        &name,
        &lease,
        "env | grep -c -e '^ANTHROPIC_API_KEY=' -e '^LITELLM_CODEX_KEY=' || true",
    );
    assert_eq!(output, "0\n");
    assert_eq!(daemon.release(&name, &lease).status, 200);
}

// ------------------------------------------------------------------ AC-44

/// AC-44: a key without a configured endpoint is reported present with an
/// absent endpoint and is not loaded; the service has no default endpoint.
#[test]
fn ac44_key_without_endpoint_is_not_loaded() {
    let fixture = Fixture::new();
    fixture.write_agents(true, true, false);
    let daemon = fixture.start("state", true);
    let name = daemon.instance();
    let agents = &daemon.machine(&name)["capabilities"]["agents"];
    assert_eq!(
        agents["claude_code"],
        json!({"key": "present", "endpoint": "absent"})
    );
    let (_, lease) = daemon.acquired("no-endpoint", "default");
    let output = daemon.output(
        &name,
        &lease,
        "env | grep -c -e '^ANTHROPIC_API_KEY=' -e '^ANTHROPIC_BASE_URL=' -e '^LITELLM_CODEX_KEY=' || true",
    );
    assert_eq!(output, "0\n");
    assert!(!fixture
        .workspace_root()
        .join(&lease)
        .join("codex/config.toml")
        .exists());
    assert_eq!(daemon.release(&name, &lease).status, 200);
}

// ------------------------------------------------------------------ AC-30

/// AC-30: release ends what the lease left running — a background process
/// in its session and a process that left the session with `setsid` while
/// its parent stayed — and nothing else: processes the lease did not start
/// survive, and a CI-runner process survives even when the lease started it.
/// The hygiene record lists each with its rule, the workspace is removed and
/// the machine is available.
#[test]
fn ac30_release_sweeps_what_the_lease_left_and_nothing_else() {
    let fixture = Fixture::new();
    let runner = fixture.home.join("actions-runner");
    write_exec(&runner, "run.sh", "#!/bin/sh\nsleep 3600 &\nwait\n");
    let run_sh = runner.join("run.sh").to_string_lossy().to_string();

    // Stand-ins for the owner's own app, started before the lease, and a
    // CI job and an owner process started during it, none by the lease.
    let owner_app = StandIn::spawn("/bin/sleep", &["3600"]);
    let daemon = fixture.start("state", true);
    let (name, lease) = daemon.acquired("sweep", "none");
    let ci_job = StandIn::spawn("/bin/sh", &[&run_sh]);
    let owner_during = StandIn::spawn("/bin/sleep", &["3600"]);

    let background: i32 = daemon
        .output(&name, &lease, "nohup sleep 3600 >/dev/null 2>&1 & echo $!")
        .trim()
        .parse()
        .unwrap();
    let _background_guard = Leftover(background);
    // A child that leaves the session with setsid while its parent waits.
    let parent: i32 = daemon
        .output(
            &name,
            &lease,
            "/bin/sh -c '/usr/bin/perl -MPOSIX -e \"POSIX::setsid(); exec q(/bin/sleep), q(3600)\" & wait' >/dev/null 2>&1 & echo $!",
        )
        .trim()
        .parse()
        .unwrap();
    let _parent_guard = Leftover(parent);
    let lease_ci: i32 = daemon
        .output(
            &name,
            &lease,
            &format!("/bin/sh {run_sh} >/dev/null 2>&1 & echo $!"),
        )
        .trim()
        .parse()
        .unwrap();
    let _lease_ci_guard = Leftover(lease_ci);
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
    .expect("the escaped child");
    let _escaped_guard = Leftover(escaped);
    let sid = |pid: i32| {
        // SAFETY: `getsid` only reads kernel state.
        unsafe { libc::getsid(pid) }
    };
    assert_ne!(sid(escaped), sid(parent), "the child left the session");
    for pid in [background, parent, escaped, lease_ci] {
        assert!(alive(pid), "{pid} should run before the release");
    }

    let released = daemon.release(&name, &lease);
    assert_eq!(released.status, 200, "{}", released.body);
    let hygiene = &released.body["hygiene"];
    for (pid, what) in [
        (background, "background"),
        (parent, "parent"),
        (escaped, "escaped"),
    ] {
        assert!(wait_dead(pid), "the {what} process survived");
    }
    for (pid, what) in [
        (owner_app.pid(), "owner app"),
        (owner_during.pid(), "owner process"),
        (ci_job.pid(), "CI job"),
        (lease_ci, "CI process the lease started"),
    ] {
        assert!(alive(pid), "the {what} ({pid}) was ended");
    }
    let ended: Vec<&Value> = hygiene["stopped"]
        .as_array()
        .unwrap()
        .iter()
        .chain(hygiene["swept"].as_array().unwrap())
        .collect();
    let entry = |pid: i32| ended.iter().find(|entry| entry["pid"] == pid).copied();
    let background_entry = entry(background).expect("background listed");
    assert_eq!(background_entry["rule"], "session");
    assert_eq!(background_entry["ended"], true);
    assert!(background_entry["executable"]
        .as_str()
        .unwrap()
        .ends_with("sleep"));
    assert!(background_entry["started_at"].as_f64().unwrap() > 0.0);
    // Ended with its parent's session or by the sweep's descendant rule.
    let escaped_entry = entry(escaped);
    assert!(
        escaped_entry.is_none_or(|entry| entry["rule"] == "descendant"),
        "{escaped_entry:?}"
    );
    for pid in [owner_app.pid(), owner_during.pid(), ci_job.pid()] {
        assert!(entry(pid).is_none());
    }
    let spared = hygiene["spared"].as_array().unwrap();
    assert!(spared
        .iter()
        .any(|entry| entry["pid"] == lease_ci && entry["spared"] == "ci-runner"));
    assert!(!fixture.workspace_root().join(&lease).exists());
    assert_eq!(daemon.machine(&name)["state"], "available");
}

// ------------------------------------------------------------------ AC-31

/// AC-31: the inventory records an installed formula and a new Application
/// Support entry; the release carries `changes.json` and both inventory
/// paths; a failing part is recorded as missing and never blocks the
/// release.
#[test]
fn ac31_inventory_before_and_after_never_refuses() {
    let fixture = Fixture::new();
    let daemon = fixture.start("state", true);
    let (name, lease) = daemon.acquired("inventory", "none");
    daemon.output(
        &name,
        &lease,
        "brew install hello && mkdir -p \"$HOME/Library/Application Support/FixtureApp\"",
    );
    let released = daemon.release(&name, &lease);
    assert_eq!(released.status, 200, "{}", released.body);
    let hygiene = &released.body["hygiene"];
    let changes = &hygiene["changes"]["parts"];
    assert_eq!(
        changes["homebrew_formulae"]["added"],
        json!({"hello": "1.0"})
    );
    let support = fixture
        .home
        .join("Library/Application Support/FixtureApp")
        .to_string_lossy()
        .to_string();
    assert!(
        changes["folders"]["added"].get(&support).is_some(),
        "{changes}"
    );
    for key in ["before", "after", "changes"] {
        let path = hygiene["inventory"][key].as_str().unwrap();
        assert!(Path::new(path).is_file(), "{key}: {path}");
    }
    let record = daemon.request(
        "GET",
        &format!("/machines/{name}/leases/{lease}/hygiene"),
        None,
    );
    assert_eq!(record.status, 200);
    assert_eq!(record.body["lease_id"], lease.as_str());

    // A part that fails at release is missing with its reason.
    let (_, lease) = daemon.acquired("inventory", "none");
    std::fs::write(fixture.root.join("brew-fails"), "").unwrap();
    let released = daemon.release(&name, &lease);
    assert_eq!(released.status, 200, "{}", released.body);
    let missing = &released.body["hygiene"]["changes"]["missing"]["after"]["homebrew_formulae"];
    assert!(
        missing.as_str().unwrap().contains("exited with 1"),
        "{missing}"
    );
    assert_eq!(daemon.machine(&name)["state"], "available");
}

// ------------------------------------------------------------------ AC-33

/// AC-33: a service restart keeps the durable lease and the processes it
/// started; the release after the restart ends them and the hygiene record
/// is complete.
#[test]
fn ac33_service_restart_keeps_the_durable_lease() {
    let fixture = Fixture::new();
    let mut daemon = fixture.start("state", true);
    let (name, lease) = daemon.acquired("restart", "default");
    let background: i32 = daemon
        .output(&name, &lease, "sleep 3600 >/dev/null 2>&1 & echo $!")
        .trim()
        .parse()
        .unwrap();
    let _stopped = daemon.stop();
    assert!(
        alive(background),
        "the lease's process died with the service"
    );
    drop(daemon);
    // `drop` released nothing: the daemon had stopped.
    assert!(alive(background));

    let daemon = fixture.start("state", true);
    let record = daemon.machine(&name);
    assert_eq!(record["state"], "leased");
    assert_eq!(record["lease_id"], lease.as_str());
    // The session environment is rebuilt for the same lease.
    let output = daemon.output(
        &name,
        &lease,
        "echo $MACHINE_SERVICE_LEASE_ID $ANTHROPIC_API_KEY",
    );
    assert_eq!(output, format!("{lease} [redacted]\n"));

    let released = daemon.release(&name, &lease);
    assert_eq!(released.status, 200, "{}", released.body);
    assert!(
        wait_dead(background),
        "the release left the process running"
    );
    let hygiene = &released.body["hygiene"];
    assert!(hygiene["stopped"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["pid"] == background));
    assert!(hygiene["inventory"]["changes"].is_string());
    assert_eq!(hygiene["failures"], json!([]));
}

// ------------------------------------------------------------------ DM-5..DM-7

/// DM-7: a release preempts a running command and kills its session.
#[test]
fn dm7_release_preempts_a_running_command() {
    let fixture = Fixture::new();
    let daemon = fixture.start("state", true);
    let (name, lease) = daemon.acquired("preempt", "none");
    let port = daemon.port;
    let path = format!("/machines/{name}/exec");
    let body = json!({"lease_id": lease, "argv": ["sleep", "120"]});
    let running =
        std::thread::spawn(move || http_timeout(port, "POST", &path, Some(&body), None, LONG));
    std::thread::sleep(Duration::from_millis(800));
    let begun = Instant::now();
    let released = daemon.release(&name, &lease);
    assert_eq!(released.status, 200);
    let answer = running.join().unwrap();
    assert_eq!(answer.status, 409);
    assert_eq!(
        answer.body["error"],
        format!("command cancelled by release of {name}")
    );
    assert!(begun.elapsed() < Duration::from_secs(60));
}

/// DM-5: path-based push and pull through the workspace, with digests; a
/// transfer touching the agent key files is refused.
#[test]
fn dm5_push_and_pull() {
    let fixture = Fixture::new();
    let daemon = fixture.start("state", true);
    let (name, lease) = daemon.acquired("transfer", "none");
    let local = fixture.root.join("input.txt");
    std::fs::write(&local, b"hello").unwrap();
    let pushed = daemon.request(
        "POST",
        &format!("/machines/{name}/push"),
        Some(&json!({"lease_id": lease, "local_path": local, "remote_path": "in.txt"})),
    );
    assert_eq!(pushed.status, 200, "{}", pushed.body);
    assert_eq!(
        pushed.body["sha256"],
        "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
    );
    assert_eq!(daemon.output(&name, &lease, "cat in.txt"), "hello");
    let back = fixture.root.join("back.txt");
    let pulled = daemon.request(
        "POST",
        &format!("/machines/{name}/pull"),
        Some(&json!({"lease_id": lease, "remote_path": "in.txt", "local_path": back})),
    );
    assert_eq!(pulled.status, 200, "{}", pulled.body);
    assert_eq!(std::fs::read(&back).unwrap(), b"hello");
    let key = fixture.agents().join("claude-code.env");
    let refused = daemon.request(
        "POST",
        &format!("/machines/{name}/push"),
        Some(&json!({"lease_id": lease, "local_path": key, "remote_path": "k"})),
    );
    assert_eq!(refused.status, 409, "{}", refused.body);
    assert_eq!(daemon.release(&name, &lease).status, 200);
}

/// DM-6: a heartbeat renews the TTL; past TTL GC warns, past grace GC
/// releases the lease and frees the lock.
#[test]
fn dm6_heartbeat_ttl_and_gc() {
    let fixture = Fixture::new();
    let daemon = fixture.start("state", true);
    let name = daemon.instance();
    let acquired =
        daemon.acquire(json!({"machine": name, "purpose": "ttl", "ttl_hours": 1, "env": "none"}));
    assert_eq!(acquired.status, 200, "{}", acquired.body);
    let lease = acquired.body["lease_id"].as_str().unwrap().to_string();
    let before = acquired.body["ttl_expires_at"].as_f64().unwrap();
    let beat = daemon.request(
        "POST",
        &format!("/machines/{name}/heartbeat"),
        Some(&json!({"lease_id": lease, "ttl_hours": 4})),
    );
    assert_eq!(beat.status, 200, "{}", beat.body);
    assert!(beat.body["ttl_expires_at"].as_f64().unwrap() > before + 2.0 * 3600.0);

    let record_path = daemon.state.join("leases").join(&lease).join("lease.json");
    let rewrite = |change: &dyn Fn(&mut Value)| {
        let mut record: Value =
            serde_json::from_str(&std::fs::read_to_string(&record_path).unwrap()).unwrap();
        change(&mut record);
        std::fs::write(&record_path, serde_json::to_vec(&record).unwrap()).unwrap();
    };
    rewrite(&|record| record["ttl_expires_at"] = json!(1.0));
    assert_eq!(daemon.request("POST", "/gc", Some(&json!({}))).status, 200);
    let warned = daemon.machine(&name);
    assert_eq!(warned["warned"], true);
    assert!(warned["grace_until"].as_f64().is_some());
    rewrite(&|record| record["grace_until"] = json!(2.0));
    assert_eq!(daemon.request("POST", "/gc", Some(&json!({}))).status, 200);
    assert_eq!(daemon.machine(&name)["state"], "available");
    let hygiene = daemon.request(
        "GET",
        &format!("/machines/{name}/leases/{lease}/hygiene"),
        None,
    );
    assert_eq!(hygiene.body["reason"], "ttl-expired");
    // The lock is free again.
    let (_, next) = daemon.acquired("after-gc", "none");
    assert_eq!(daemon.release(&name, &next).status, 200);
}

// ------------------------------------------------------------------ AC-38

/// AC-38: an instance on a Mac without Tart, with exposure on, offers only
/// its durable machine: `/health` reports `kinds: ["durable"]`, `/machines`
/// lists the durable machine and no offer even with a valid image line, and
/// an acquisition with an image is refused with 400 naming Tart, before any
/// state is written.
#[test]
fn ac38_instance_without_tart_offers_only_its_durable_machine() {
    let fixture = Fixture::new();
    // A valid image line, so only the missing Tart keeps virtual machines
    // away.
    let line = fixture.root.join("pilot/images/macos26");
    std::fs::create_dir_all(&line).unwrap();
    std::fs::write(
        line.join("line.conf"),
        "LINE_KIND=macos\nBASE_VM=pilot-macos26-base\nCLONE_PREFIX=pilot-mac-\nGUEST_USER=admin\nGUEST_PASS=admin\n",
    )
    .unwrap();
    let daemon = fixture.start("state", true);
    let name = daemon.instance();
    let health = daemon.request("GET", "/health", None);
    assert_eq!(health.body["kinds"], json!(["durable"]), "{}", health.body);
    let machines = daemon.request("GET", "/machines", None).body;
    assert_eq!(machines["machines"][&name]["kind"], "durable", "{machines}");
    assert_eq!(machines["machines"].as_object().unwrap().len(), 1);
    assert!(
        machines
            .get("offers")
            .is_none_or(|offers| offers.as_array().is_some_and(Vec::is_empty)),
        "{machines}"
    );
    let refused = daemon.acquire(json!({"purpose": "vm-check", "image": "macos26"}));
    assert_eq!(refused.status, 400, "{}", refused.body);
    let error = refused.body["error"].as_str().unwrap();
    assert!(error.contains("Tart is not installed"), "{error}");
    let state = std::fs::read_to_string(daemon.state.join("state.json")).unwrap_or_default();
    assert!(!state.contains("vm-check"), "{state}");
    // The durable machine still leases.
    let (_, lease) = daemon.acquired("durable-check", "none");
    assert_eq!(daemon.release(&name, &lease).status, 200);
}

/// IC-7 (DC-94): without `PILOT_REPO` the instance has no image source. It
/// checks no image files, its catalog is current and empty, and nothing
/// warns.
#[test]
fn ic7_no_pilot_repo_means_no_image_source() {
    let fixture = Fixture::new();
    let daemon = fixture
        .try_start_with("state", true, &[("PILOT_REPO", "")])
        .unwrap_or_else(|err| panic!("{}", std::fs::read_to_string(err).unwrap_or_default()));
    let machines = daemon.request("GET", "/machines", None).body;
    assert_eq!(machines["catalog"]["status"], "current", "{machines}");
    assert!(machines["catalog"].get("error").is_none(), "{machines}");
    let images = daemon.request("GET", "/images", None);
    assert_eq!(images.status, 200, "{}", images.body);
    assert_eq!(images.body["images"], json!({}), "{}", images.body);
    let applications = daemon.request("GET", "/applications", None);
    assert_eq!(applications.status, 200, "{}", applications.body);
    assert_eq!(
        applications.body["images"],
        json!([]),
        "{}",
        applications.body
    );
    let log = std::fs::read_to_string(daemon.state.join("service.log")).unwrap_or_default();
    assert!(!log.contains("ERROR"), "{log}");
    assert!(log.contains("image source: none"), "{log}");
}

/// DC-93: a Mac configured to provide neither its durable machine nor
/// virtual machines runs, and answers with no machine at all.
#[test]
fn dc93_an_instance_configured_to_offer_nothing_serves_no_machines() {
    let fixture = Fixture::new();
    let daemon = fixture
        .try_start_with("state", false, &[("PILOT_REPO", "")])
        .unwrap_or_else(|err| panic!("{}", std::fs::read_to_string(err).unwrap_or_default()));
    let health = daemon.request("GET", "/health", None);
    assert_eq!(health.body["kinds"], json!([]), "{}", health.body);
    let machines = daemon.request("GET", "/machines", None).body;
    assert_eq!(machines["machines"], json!({}), "{machines}");
    assert!(
        machines
            .get("offers")
            .is_none_or(|offers| offers.as_array().is_some_and(Vec::is_empty)),
        "{machines}"
    );
}

fn sha256(bytes: &[u8]) -> String {
    common::sha256_hex(bytes)
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(field, _)| field == name)
        .map(|(_, value)| value.as_str())
}

/// AC-28, FD-4 (DC-70, DC-71): bytes travel over HTTP to and from a
/// durable machine: a file as itself and a folder as a tar stream, each
/// checked against `X-Content-SHA256`. A wrong digest places nothing, a
/// missing lease is refused before any byte is kept, and `machctl push`
/// and `pull --bytes` take the same endpoints with a 64 MiB file and a
/// folder of 100 files.
#[test]
fn ac28_byte_transfers_to_and_from_a_durable_machine() {
    let fixture = Fixture::new();
    let daemon = fixture.start("state", true);
    let (name, lease) = daemon.acquired("bytes", "none");
    let files = |path: &str| format!("/machines/{name}/files?lease_id={lease}&path={path}");

    let hello = b"hello over http";
    let digest = sha256(hello);
    let (status, _, body) = common::raw_http(
        daemon.port,
        "PUT",
        &files("in.txt"),
        &[
            ("Content-Type", "application/octet-stream"),
            ("X-Content-SHA256", &digest),
        ],
        hello,
    );
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let answer: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(answer["sha256"], digest);
    assert_eq!(answer["bytes"], hello.len());
    assert_eq!(
        daemon.output(&name, &lease, "cat in.txt"),
        "hello over http"
    );

    // A digest that does not match the body places nothing.
    let (status, _, body) = common::raw_http(
        daemon.port,
        "PUT",
        &files("bad.txt"),
        &[
            ("Content-Type", "application/octet-stream"),
            ("X-Content-SHA256", &sha256(b"something else")),
        ],
        hello,
    );
    assert_eq!(status, 400);
    assert!(String::from_utf8_lossy(&body).contains("digest mismatch"));
    assert_eq!(
        daemon.output(
            &name,
            &lease,
            "test -e bad.txt && echo there || echo absent"
        ),
        "absent\n"
    );
    // No lease, no transfer.
    let (status, _, body) = common::raw_http(
        daemon.port,
        "PUT",
        &format!("/machines/{name}/files?path=x.txt"),
        &[
            ("Content-Type", "application/octet-stream"),
            ("X-Content-SHA256", &digest),
        ],
        hello,
    );
    assert_eq!(status, 409, "{}", String::from_utf8_lossy(&body));

    // A folder travels as a tar stream of its contents.
    let tree = fixture.root.join("tree");
    std::fs::create_dir_all(tree.join("nested")).unwrap();
    std::fs::write(tree.join("nested/b.txt"), "bee").unwrap();
    let archive = fixture.root.join("tree.tar");
    byte_transfer::pack(&tree, &archive).unwrap();
    let tar = std::fs::read(&archive).unwrap();
    let (status, _, body) = common::raw_http(
        daemon.port,
        "PUT",
        &format!("{}&name=tree", files("tree")),
        &[
            ("Content-Type", "application/x-tar"),
            ("X-Content-SHA256", &sha256(&tar)),
        ],
        &tar,
    );
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert_eq!(daemon.output(&name, &lease, "cat tree/nested/b.txt"), "bee");

    // Back again: a file as bytes, a folder as a tar stream.
    let (status, headers, body) = common::raw_http(daemon.port, "GET", &files("in.txt"), &[], b"");
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert_eq!(body, hello);
    assert_eq!(header(&headers, "x-content-sha256"), Some(digest.as_str()));
    assert_eq!(
        header(&headers, "content-type"),
        Some("application/octet-stream")
    );
    let (status, headers, body) = common::raw_http(daemon.port, "GET", &files("tree"), &[], b"");
    assert_eq!(status, 200);
    assert_eq!(header(&headers, "content-type"), Some("application/x-tar"));
    assert_eq!(header(&headers, "x-content-name"), Some("tree"));
    let fetched = fixture.root.join("fetched.tar");
    std::fs::write(&fetched, &body).unwrap();
    let unpacked = fixture.root.join("unpacked");
    byte_transfer::unpack(&fetched, &unpacked).unwrap();
    assert_eq!(
        std::fs::read_to_string(unpacked.join("nested/b.txt")).unwrap(),
        "bee"
    );

    // The CLI, with `--bytes` on loopback, and the lease it was given.
    let cli_home = fixture.root.join("cli-home");
    std::fs::create_dir_all(&cli_home).unwrap();
    let env = vec![
        ("MACHINE_SERVICE_PORT".to_string(), daemon.port.to_string()),
        ("HOME".to_string(), cli_home.display().to_string()),
    ];
    let source = fixture.root.join("cli.txt");
    std::fs::write(&source, "from the cli").unwrap();
    let pushed = common::run_machctl(
        &[
            "push",
            "--bytes",
            "--lease-id",
            &lease,
            &name,
            source.to_str().unwrap(),
            "cli.txt",
        ],
        &env,
    );
    assert!(
        pushed.status.success(),
        "{}",
        String::from_utf8_lossy(&pushed.stderr)
    );
    let back = fixture.root.join("cli-back");
    std::fs::create_dir_all(&back).unwrap();
    let pulled = common::run_machctl(
        &[
            "pull",
            "--bytes",
            "--lease-id",
            &lease,
            &name,
            "tree",
            back.to_str().unwrap(),
        ],
        &env,
    );
    assert!(
        pulled.status.success(),
        "{}",
        String::from_utf8_lossy(&pulled.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(back.join("tree/nested/b.txt")).unwrap(),
        "bee"
    );
    assert_eq!(daemon.output(&name, &lease, "cat cli.txt"), "from the cli");

    // AC-28's sizes: a 64 MiB file and a folder of 100 files, both ways.
    let big = fixture.root.join("big.bin");
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let bytes: Vec<u8> = (0..64 * 1024 * 1024 / 8)
        .flat_map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_le_bytes()
        })
        .collect();
    std::fs::write(&big, &bytes).unwrap();
    let many = fixture.root.join("many");
    std::fs::create_dir_all(&many).unwrap();
    for index in 0..100 {
        std::fs::write(
            many.join(format!("f{index:03}.txt")),
            format!("file {index}\n"),
        )
        .unwrap();
    }
    for source in [&big, &many] {
        let pushed = common::run_machctl(
            &[
                "push",
                "--bytes",
                "--lease-id",
                &lease,
                &name,
                source.to_str().unwrap(),
                ".",
            ],
            &env,
        );
        assert!(
            pushed.status.success(),
            "{}",
            String::from_utf8_lossy(&pushed.stderr)
        );
    }
    let round = fixture.root.join("round");
    std::fs::create_dir_all(&round).unwrap();
    for item in ["big.bin", "many"] {
        let pulled = common::run_machctl(
            &[
                "pull",
                "--bytes",
                "--lease-id",
                &lease,
                &name,
                item,
                round.to_str().unwrap(),
            ],
            &env,
        );
        assert!(
            pulled.status.success(),
            "{}",
            String::from_utf8_lossy(&pulled.stderr)
        );
    }
    assert_eq!(
        sha256(&std::fs::read(round.join("big.bin")).unwrap()),
        sha256(&bytes)
    );
    assert_eq!(std::fs::read_dir(round.join("many")).unwrap().count(), 100);
    assert_eq!(
        std::fs::read_to_string(round.join("many/f042.txt")).unwrap(),
        "file 42\n"
    );

    // Nothing of a transfer is left in the state folder.
    let staged = daemon.state.join("transfers");
    let left: Vec<_> = std::fs::read_dir(&staged)
        .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
        .unwrap_or_default();
    assert!(left.is_empty(), "{left:?}");
    assert_eq!(daemon.release(&name, &lease).status, 200);
}

/// Seconds since the epoch, as the process fixture's start times.
fn now_s() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// One row of the sweep's process fixture (DC-253), run by this
/// account, started at `start`.
fn row(pid: i32, start: u64, path: &str, env: &[String], job_lease: Option<&str>) -> Value {
    // SAFETY: `geteuid` only reads process state.
    let uid = unsafe { libc::geteuid() };
    json!({
        "pid": pid, "ppid": 1, "pgid": pid, "sid": pid, "uid": uid,
        "start": [start, 0],
        "comm": path.rsplit('/').next().unwrap(),
        "path": path,
        "argv": [path],
        "env": env,
        "job_lease": job_lease,
    })
}

/// A regular application of the fixture, as `lsappinfo` lists it.
fn application(pid: i32, name: &str) -> Value {
    json!({"name": name, "bundle_id": format!("dev.machine-service.test.{name}"),
           "path": format!("/Applications/{name}.app"), "pid": pid})
}

/// The signals the sweep sent to the fixture's rows, `(pid, signal)`.
fn signals(daemon: &Daemon) -> Vec<(i64, String)> {
    std::fs::read_to_string(daemon.processes().with_extension("signals"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(pid, signal)| (pid.parse().unwrap(), signal.to_string()))
        .collect()
}

/// AC-30, HY-3 (DC-73): an application the lease opened is asked to quit
/// at release and recorded with the `application` rule; an application the
/// owner opened, before or during the lease, and one already running before
/// it, keep running; a process that carries the lease's ID is ended by the
/// environment rule and an untagged one is not. Proved through the test
/// build's process seam (DC-253): the applications and processes are
/// rows of the test's fixture, so no application of this Mac is opened,
/// asked to quit or signalled; the real applications are proved in the
/// opt-in live layer (`live_durable.rs`).
#[test]
fn ac30_release_quits_the_applications_the_lease_opened_and_only_those() {
    let fixture = Fixture::new();
    let daemon = fixture.start("state", true);
    let before = now_s();
    // Running before the lease: the owner's application, and one that the
    // lease will only activate, whose job carries no lease.
    let early = row(
        990_002,
        before - 100,
        "/Applications/Owner.app/Contents/MacOS/Owner",
        &[],
        None,
    );
    let running = row(
        990_004,
        before + 2,
        "/Applications/Running.app/Contents/MacOS/Running",
        &[],
        None,
    );
    std::fs::write(
        daemon.processes(),
        json!({"processes": [early, running],
               "applications": [application(990_002, "Owner"), application(990_004, "Running")]})
        .to_string(),
    )
    .unwrap();
    let (name, lease) = daemon.acquired("apps", "none");
    let during = now_s() + 1;
    let tag = format!("MACHINE_SERVICE_LEASE_ID={lease}");
    let opened = row(
        990_001,
        during,
        "/Applications/Lease.app/Contents/MacOS/Lease",
        &[],
        Some(&lease),
    );
    let owner = row(
        990_003,
        during,
        "/Applications/OwnerDuring.app/Contents/MacOS/OwnerDuring",
        &[],
        None,
    );
    let running_now = row(
        990_004,
        before + 2,
        "/Applications/Running.app/Contents/MacOS/Running",
        &[],
        Some(&lease),
    );
    let tagged = row(990_005, during, "/usr/bin/tagged-helper", &[tag], None);
    let untagged = row(990_006, during, "/usr/bin/owner-helper", &[], None);
    std::fs::write(
        daemon.processes(),
        json!({"processes": [early, running_now, opened, owner, tagged, untagged],
               "applications": [application(990_001, "Lease"), application(990_002, "Owner"),
                                application(990_003, "OwnerDuring"), application(990_004, "Running")]})
        .to_string(),
    )
    .unwrap();

    let released = daemon.release(&name, &lease);
    assert_eq!(released.status, 200, "{}", released.body);
    let swept = released.body["hygiene"]["swept"]
        .as_array()
        .unwrap()
        .clone();
    let entry = |pid: i64| swept.iter().find(|entry| entry["pid"] == pid);
    let app = entry(990_001)
        .unwrap_or_else(|| panic!("the lease's application was not swept: {swept:?}"));
    assert_eq!(app["rule"], "application");
    assert_eq!(app["ended"], true);
    assert_eq!(app["signal"], "QUIT", "asked to quit first: {app}");
    let helper =
        entry(990_005).unwrap_or_else(|| panic!("the tagged process was not swept: {swept:?}"));
    assert_eq!(helper["rule"], "environment");
    assert_eq!(helper["signal"], "TERM");
    for pid in [990_002, 990_003, 990_004, 990_006] {
        assert!(entry(pid).is_none(), "{pid} was swept: {swept:?}");
    }
    assert_eq!(
        signals(&daemon),
        vec![(990_001, "QUIT".to_string()), (990_005, "TERM".to_string())],
        "the sweep signalled only the lease's own rows, and refused nothing"
    );
}

/// HY-6 (DC-76): below `MACHINE_SERVICE_DISK_WARN_GB` the machine record
/// warns `low free disk` and stays available; `0` turns the warning off;
/// a value that is not a number stops the daemon, naming the variable.
#[test]
fn hy6_low_free_disk_warning() {
    let fixture = Fixture::new();
    let daemon = fixture
        .try_start_with(
            "warned",
            true,
            &[("MACHINE_SERVICE_DISK_WARN_GB", "1000000")],
        )
        .unwrap_or_else(|err| panic!("{}", std::fs::read_to_string(err).unwrap_or_default()));
    let name = daemon.instance();
    let record = daemon.machine(&name);
    assert_eq!(record["warnings"][0]["code"], "low-free-disk", "{record}");
    assert!(record["warnings"][0]["message"]
        .as_str()
        .unwrap()
        .starts_with("low free disk: "));
    assert_eq!(record["state"], "available");
    drop(daemon);
    let quiet = fixture
        .try_start_with("quiet", true, &[("MACHINE_SERVICE_DISK_WARN_GB", "0")])
        .unwrap_or_else(|err| panic!("{}", std::fs::read_to_string(err).unwrap_or_default()));
    assert_eq!(quiet.machine(&name)["warnings"], json!([]));
    drop(quiet);
    let err =
        match fixture.try_start_with("typo", true, &[("MACHINE_SERVICE_DISK_WARN_GB", "thirty")]) {
            Ok(_) => panic!("a daemon started with a malformed warning level"),
            Err(err) => std::fs::read_to_string(err).unwrap_or_default(),
        };
    assert!(err.contains("MACHINE_SERVICE_DISK_WARN_GB"), "{err}");
}

/// HY-5, AC-32 (DC-75): the CI runner is sampled at acquisition, on every
/// GC pass and at release; the hygiene record carries the intervals during
/// which a job ran. The job is a row of the sweep's process fixture
/// (DC-253), which the release never signals.
#[test]
fn hy5_ci_busy_intervals_are_recorded() {
    let fixture = Fixture::new();
    let daemon = fixture.start("state", true);
    let (name, lease) = daemon.acquired("ci-coexist", "none");
    let worker = fixture.home.join("actions-runner/bin/Runner.Worker");
    let job = row(990_100, now_s(), &worker.to_string_lossy(), &[], None);
    std::fs::write(daemon.processes(), json!({"processes": [job]}).to_string()).unwrap();
    let gc = daemon.request("POST", "/gc", Some(&json!({})));
    assert_eq!(gc.status, 200, "{}", gc.body);
    assert_eq!(
        daemon.machine(&name)["capabilities"]["ci_runner"]["busy"],
        true
    );
    std::fs::write(daemon.processes(), json!({"processes": []}).to_string()).unwrap();
    let gc = daemon.request("POST", "/gc", Some(&json!({})));
    assert_eq!(gc.status, 200, "{}", gc.body);
    let released = daemon.release(&name, &lease);
    assert_eq!(released.status, 200, "{}", released.body);
    let hygiene = &released.body["hygiene"];
    let intervals = hygiene["ci_busy_intervals"].as_array().unwrap();
    assert_eq!(intervals.len(), 1, "{hygiene}");
    let (from, to) = (
        intervals[0]["from"].as_f64().unwrap(),
        intervals[0]["to"].as_f64().unwrap(),
    );
    assert!(from <= to, "{hygiene}");
    assert!(hygiene["ci_sampling"]["samples"].as_u64().unwrap() >= 4);
    assert_eq!(hygiene["ci_sampling"]["interval_s"], 60);
    assert!(signals(&daemon).is_empty(), "{:?}", signals(&daemon));
}
