//! A durable lease's exec, push and pull through the Mac's own relay agent
//! (`docs/arch/durable-machine.md` DM-4, `docs/arch/relay-agent.md` RA-2,
//! RA-8; TL-2), with the service's in-session exec as the fallback.
//!
//! Each test starts the real `machine-service` binary as a second instance
//! with its own state, configuration and lock folders, durable exposure on
//! and the relay link, and the real `relay-agent` binary as the Mac's relay
//! agent, with its own home, configured as the installer configures it: the
//! instance's per-machine credential and its broker on the loopback name.
//! The release the instance holds is made from that same binary, so the
//! agent runs the installed release's build (DC-140).

mod common;
#[path = "../../relay-broker/tests/common/mod.rs"]
mod relay_common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::{
    canonical_dir, free_port, http_timeout, relay_agent_bin, try_http, write_relay_release,
};
use relay_agent::protocol::{ExecutionState, Op, OpKind, Outcome};
use relay_common::{client_config_v2, Collected, TestClient};
use serde_json::{json, Value};

const CLAUDE_KEY: &str = "sk-fake-relay-claude-key-0123456789";
const OWNER_TOKEN: &str = "owner-relay-token-fake-777";
const LONG: Duration = Duration::from_secs(180);

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

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the PID exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn wait_dead(pid: i32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        let state = Command::new("/bin/ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .unwrap_or_default();
        if state.is_empty() || state.starts_with('Z') {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// A PID the lease started that the test kills itself if it survives.
struct Leftover(i32);

impl Drop for Leftover {
    fn drop(&mut self) {
        // SAFETY: only PIDs this test's lease started are passed.
        unsafe { libc::kill(self.0, libc::SIGKILL) };
    }
}

struct Lab {
    /// One lifeline for the lab's daemons and agent: what a stopped daemon
    /// started, a restarted one sweeps (DC-256).
    lifeline: common::Lifeline,
    _dir: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    agent_home: PathBuf,
    state: PathBuf,
    release: PathBuf,
    session_port: u16,
    port: u16,
    daemon: Option<common::Owned>,
    /// Daemons a restart stopped, kept until the lab ends: what they
    /// started, such as the fake launchd's agents, outlives them as it
    /// would outlive a real restart, and ends with the lab.
    retired: Vec<common::Owned>,
    agent: Option<common::Owned>,
    /// The folder of the test's `launchctl`: the fake launchd, which runs
    /// the LaunchAgents it loads, or one that refuses to load any. Never
    /// the real one, which would load the instance's relay agent into this
    /// Mac's desktop session.
    launchd: PathBuf,
    /// How the daemon is given the test's `launchctl`.
    seam: Seam,
}

/// How a test daemon is given its test's `launchctl` (DC-242).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Seam {
    /// Named by the test build's variable: the daemon runs it.
    Named,
    /// Only first on the daemon's `PATH`, with nothing named: the test
    /// build runs no `launchctl` at all, neither it nor `/bin/launchctl`.
    PathOnly,
}

/// A `launchctl` that loads nothing: the daemon finds no desktop session
/// and leaves its relay agent unprovisioned, with nothing loaded into this
/// Mac's own session.
const NO_LAUNCHCTL: &str = "#!/bin/sh\necho 'this test loads no LaunchAgent' >&2\nexit 1\n";

/// A `launchctl` that runs a LaunchAgent's program as launchd would, with
/// its `HOME`, and keeps its process ID by label, so a test sees the daemon
/// bring its own relay agent up without loading anything into this Mac's
/// desktop session.
const FAKE_LAUNCHCTL: &str = r#"#!/bin/sh
dir="$FAKE_LAUNCHD"
printf '%s\n' "$*" >> "$dir/calls.log"
alive() { [ -f "$dir/$1.pid" ] && kill -0 "$(cat "$dir/$1.pid")" 2>/dev/null; }
case "$1" in
  print)
    case "$2" in
      gui/*/*) label="${2##*/}"
        if alive "$label"; then echo "pid = $(cat "$dir/$label.pid")"; exit 0; fi
        echo "Could not find service" >&2; exit 113 ;;
      gui/*) echo "domain = gui"; exit 0 ;;
    esac ;;
  bootstrap)
    plist="$3"; label="$(basename "$plist" .plist)"
    value() { /usr/bin/plutil -extract "$1" raw -o - "$plist" 2>/dev/null; }
    HOME="$(value EnvironmentVariables.HOME)" \
      RELAY_AGENT_ROOT="$(value EnvironmentVariables.RELAY_AGENT_ROOT)" SHELL=/bin/sh \
      nohup "$(value ProgramArguments.0)" run >>"$(value StandardErrorPath)" 2>&1 </dev/null &
    echo $! > "$dir/$label.pid"
    exit 0 ;;
  bootout)
    label="${2##*/}"
    if alive "$label"; then kill "$(cat "$dir/$label.pid")"; fi
    rm -f "$dir/$label.pid"
    exit 0 ;;
esac
exit 0
"#;

/// A copy of `executable` signed ad hoc with the release identifier, as
/// `build-relay-agent.sh` signs a release build (DC-135).
fn signed_copy(executable: &Path, folder: &Path) -> PathBuf {
    std::fs::create_dir_all(folder).unwrap();
    let copy = folder.join("relay-agent");
    std::fs::copy(executable, &copy).unwrap();
    let status = Command::new("/usr/bin/codesign")
        .args([
            "--force",
            "--sign",
            "-",
            "--identifier",
            "com.wezzard.relay-agent",
        ])
        .arg(&copy)
        .status()
        .unwrap();
    assert!(status.success());
    copy
}

impl Lab {
    /// A second instance and its relay agent; `release_of` is the
    /// executable the instance's release holds.
    fn new(release_of: &Path) -> Lab {
        Lab::with_launchd(release_of, false, Seam::Named)
    }

    /// A second instance whose `launchctl` is the fake launchd, so it
    /// brings its own relay agent up (DC-213).
    fn provisioning(release_of: &Path) -> Lab {
        Lab::with_launchd(release_of, true, Seam::Named)
    }

    /// A second instance whose fake launchd is only first on its `PATH`,
    /// as every test daemon's was before DC-242.
    fn unnamed(release_of: &Path) -> Lab {
        Lab::with_launchd(release_of, true, Seam::PathOnly)
    }

    fn with_launchd(release_of: &Path, launchd: bool, seam: Seam) -> Lab {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_dir(dir.path());
        let home = root.join("home");
        let agent_home = root.join("agent-home");
        let config = root.join("config");
        let agents = config.join("agents");
        std::fs::create_dir_all(home.join("Library/Application Support")).unwrap();
        std::fs::create_dir_all(&agent_home).unwrap();
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::set_permissions(&agents, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(
            home.join(".zprofile"),
            format!(
                "export PATH=\"/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin\"\nexport ANTHROPIC_AUTH_TOKEN={OWNER_TOKEN}\n"
            ),
        )
        .unwrap();
        for (name, text) in [
            (
                "claude-code.env",
                format!("ANTHROPIC_API_KEY={CLAUDE_KEY}\n"),
            ),
            (
                "endpoints.json",
                r#"{"claude-code": {"base_url": "http://<gateway-host>"}}"#.to_string(),
            ),
        ] {
            let path = agents.join(name);
            std::fs::write(&path, text).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let release = write_relay_release(&root.join("release"), release_of);
        let mut lab = Lab {
            lifeline: common::Lifeline::new(),
            state: root.join("state"),
            _dir: dir,
            root,
            home,
            agent_home,
            release,
            session_port: 0,
            port: 0,
            daemon: None,
            retired: Vec::new(),
            agent: None,
            launchd: PathBuf::new(),
            seam,
        };
        let folder = lab.root.join("launchd");
        std::fs::create_dir_all(&folder).unwrap();
        let fake = folder.join("launchctl");
        std::fs::write(
            &fake,
            if launchd {
                FAKE_LAUNCHCTL
            } else {
                NO_LAUNCHCTL
            },
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        lab.launchd = folder;
        lab.start_daemon(&config);
        lab
    }

    /// The fake launchd's calls.
    fn launchctl_calls(&self) -> String {
        std::fs::read_to_string(self.launchd.join("calls.log")).unwrap_or_default()
    }

    fn start_daemon(&mut self, config: &Path) {
        std::fs::create_dir_all(&self.state).unwrap();
        // A restart takes the instance's ports again, as an installed
        // instance always has the same ones; a first start takes free ones.
        let again = (self.port != 0).then_some((self.port, self.session_port));
        for attempt in 0..16 {
            let (port, session_port) = match again {
                Some(ports) if attempt < 8 => ports,
                _ => (free_port(), free_port()),
            };
            let err = self.root.join(format!("daemon-{port}.err"));
            let launchd = self.launchd.clone();
            // The fakes' shell utilities only; the daemon finds no tool
            // through `PATH` (DC-254).
            let path = match self.seam {
                Seam::Named => common::BASE_PATH.to_string(),
                Seam::PathOnly => format!("{}:{}", launchd.display(), common::BASE_PATH),
            };
            // The test build's seam, never `PATH`, gives it its launchctl.
            let named = (self.seam == Seam::Named).then(|| {
                (
                    machine_service_core::launchd::TEST_VARIABLE,
                    launchd.join("launchctl"),
                )
            });
            let mut command = common::daemon_command_on(&self.root, &self.lifeline);
            command
                .envs(named)
                .env("HOME", &self.home)
                .env("USER", user())
                .env("LOGNAME", user())
                .env("SHELL", "/bin/zsh")
                .env("PATH", path)
                .env("FAKE_LAUNCHD", &launchd)
                .env("MACHINE_SERVICE_STATE", &self.state)
                .env("MACHINE_SERVICE_PORT", port.to_string())
                .env("MACHINE_SERVICE_HOST", "127.0.0.1")
                .env("MACHINE_SERVICE_CONFIG_DIR", config)
                .env("MACHINE_SERVICE_DURABLE_LOCK_DIR", self.root.join("lock"))
                .env("MACHINE_SERVICE_EXPOSE_DURABLE", "on")
                .env("MACHINE_SERVICE_GUEST_LINK", "relay")
                .env("MACHINE_SERVICE_SESSION_PORT", session_port.to_string())
                .env(
                    "MACHINE_SERVICE_RELAY_URL",
                    format!("wss://localhost:{session_port}/relay"),
                )
                .env("MACHINE_SERVICE_RELAY_AGENT_WAIT", "5")
                .env("MACHINE_SERVICE_RELAY_AGENT_RELEASE", &self.release)
                .env("PILOT_REPO", self.root.join("pilot"))
                .env("TART_HOME", self.root.join("tart"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::from(std::fs::File::create(&err).unwrap()));
            let mut child = command.spawn().unwrap();
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                if let Ok(Some(_)) = child.try_wait() {
                    break;
                }
                if try_http(port, "GET", "/health", None, None).is_ok_and(|r| r.status == 200) {
                    self.port = port;
                    self.session_port = session_port;
                    self.daemon = Some(child);
                    return;
                }
                if Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        panic!("the daemon did not start");
    }

    /// Configure and start the release's relay agent as the installer does
    /// (IN-3): the release's macOS executable, which the daemon trusts. An
    /// agent this lab started before is stopped first, so none outlives
    /// the test and no running executable is written over.
    fn start_agent(&mut self) -> i32 {
        self.stop_agent();
        let release = machine_service_core::service_layout::release::Release::open(&self.release)
            .unwrap()
            .executable(&machine_service_core::service_layout::release::platform_for("macos"))
            .unwrap();
        let executable = self.root.join("release-agent");
        std::fs::write(&executable, release).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        self.start_agent_from(&executable)
    }

    /// [`Lab::start_agent`], running `executable` as the agent.
    fn start_agent_from(&mut self, executable: &Path) -> i32 {
        self.stop_agent();
        let credential =
            std::fs::read_to_string(self.state.join("relay/durable-agent.credential")).unwrap();
        let ca = std::fs::read_to_string(self.state.join("relay/tls/ca.pem")).unwrap();
        let folder = self.agent_home.join(".config/relay-agent");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = folder.join("config.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&json!({
                "broker_url": format!("wss://localhost:{}/relay", self.session_port),
                "agent_credential": credential.trim(),
                "broker_ca_pem": ca,
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let mut command = common::harness::command_on(executable, &self.lifeline);
        command
            .arg("run")
            .env("HOME", &self.agent_home)
            .env("SHELL", "/bin/sh")
            .stdout(Stdio::null())
            .stderr(Stdio::from(
                std::fs::File::create(self.root.join("agent.log")).unwrap(),
            ));
        let child = command.spawn().unwrap();
        let pid = child.id() as i32;
        self.agent = Some(child);
        pid
    }

    fn stop_agent(&mut self) {
        // Dropping the handle ends the agent and what it started.
        self.agent.take();
    }

    fn request(&self, method: &str, path: &str, body: Option<&Value>) -> common::Response {
        http_timeout(self.port, method, path, body, None, LONG)
    }

    fn name(&self) -> String {
        self.request("GET", "/health", None).body["instance"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn acquire(&self, env: &str) -> (String, String, Value) {
        let name = self.name();
        let answer = self.request(
            "POST",
            "/acquire",
            Some(&json!({"machine": name, "purpose": "relay-durable", "env": env})),
        );
        assert_eq!(answer.status, 200, "{}", answer.body);
        let lease = answer.body["lease_id"].as_str().unwrap().to_string();
        (name, lease, answer.body)
    }

    fn exec(&self, name: &str, body: Value) -> common::Response {
        self.request("POST", &format!("/machines/{name}/exec"), Some(&body))
    }

    /// Wait until a call goes through the relay agent.
    fn wait_for_relay(&self, name: &str, lease: &str) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let answer = self.exec(name, json!({"lease_id": lease, "argv": ["true"]}));
            if answer.body["link"] == "relay" {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the relay agent never served: {}\n{}",
                answer.body,
                std::fs::read_to_string(self.root.join("agent.log")).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    /// Stop the instance and start it again on the same state (DM-8).
    fn restart_daemon(&mut self) {
        if let Some(mut daemon) = self.daemon.take() {
            daemon.signal(libc::SIGTERM, Duration::from_secs(30));
            self.retired.push(daemon);
        }
        let config = self.root.join("config");
        self.start_daemon(&config);
    }

    /// The service's log.
    fn log(&self) -> String {
        std::fs::read_to_string(self.state.join("service.log")).unwrap_or_default()
    }

    /// The leases whose purge is pending, as the machine record and
    /// `/health` both show them (DC-177).
    fn pending_purges(&self, name: &str) -> Vec<String> {
        let ids = |list: &Value| -> Vec<String> {
            list.as_array()
                .unwrap_or_else(|| panic!("no pending list: {list}"))
                .iter()
                .map(|purge| purge["lease_id"].as_str().unwrap().to_string())
                .collect()
        };
        let record = self.request("GET", &format!("/machines/{name}"), None);
        let health = self.request("GET", "/health", None);
        let from_record = ids(&record.body["relay_purges_pending"]);
        assert_eq!(from_record, ids(&health.body["relay_purges_pending"]));
        from_record
    }

    /// Wait until the broker logged `count` registrations of an agent.
    fn wait_for_registrations(&self, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while self.log().matches("agent registered").count() < count {
            assert!(Instant::now() < deadline, "the agent did not register");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn release_lease(&self, name: &str, lease: &str) -> common::Response {
        self.request(
            "POST",
            &format!("/machines/{name}/release"),
            Some(&json!({"lease_id": lease, "reason": "test"})),
        )
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        if let (Some(_), Ok(text)) = (
            self.daemon.as_ref(),
            std::fs::read_to_string(self.state.join("durable.json")),
        ) {
            if let Some(lease) = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["current"].as_str().map(str::to_string))
            {
                let name = self.name();
                let _ = self.release_lease(&name, &lease);
            }
        }
        self.stop_agent();
        if let Some(mut daemon) = self.daemon.take() {
            daemon.terminate(Duration::from_secs(30));
        }
        self.retired.clear();
        // The agents the fake launchd started, which their daemon's handle
        // has ended with it; kept as a second line.
        {
            let dir = &self.launchd;
            for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                if entry.path().extension().is_some_and(|e| e == "pid") {
                    if let Some(pid) = std::fs::read_to_string(entry.path())
                        .ok()
                        .and_then(|text| text.trim().parse::<i32>().ok())
                    {
                        // SAFETY: only PIDs the fake launchd started are named.
                        unsafe { libc::kill(pid, libc::SIGKILL) };
                    }
                }
            }
        }
    }
}

/// DC-213: a second instance with durable exposure on and the relay
/// link brings its own relay agent up, without an installer: the release's
/// build, the machine's credential, its own broker, under a label and a home
/// its state folder names. Nothing of it lies in the operator's home, where
/// the installed instance's agent lives, and a restart of the instance
/// leaves an agent that runs as wanted alone.
#[test]
fn an_exposed_instance_brings_up_its_own_relay_agent() {
    let scratch = tempfile::tempdir().unwrap();
    let signed = signed_copy(relay_agent_bin(), scratch.path());
    let mut lab = Lab::provisioning(&signed);
    let (name, lease, acquired) = lab.acquire("none");
    lab.wait_for_relay(&name, &lease);
    let record = lab.request("GET", &format!("/machines/{name}"), None).body;
    assert_eq!(record["link"], "relay", "{record}");
    let agent = &record["relay_agent"];
    let label = agent["label"].as_str().unwrap().to_string();
    assert!(
        label.starts_with("com.wezzard.relay-agent.") && label.len() == 36,
        "{label}"
    );
    let home = PathBuf::from(agent["root"].as_str().unwrap());
    assert_eq!(home, lab.state.canonicalize().unwrap().join("relay/agent"));
    // Its commands keep the operator's HOME (DM-1); its files are its own.
    let seen = lab.exec(
        &name,
        json!({"lease_id": lease, "argv": ["/bin/sh", "-c", "echo $HOME"]}),
    );
    assert_eq!(seen.body["link"], "relay", "{}", seen.body);
    assert_eq!(
        seen.body["output"].as_str().unwrap().trim(),
        lab.home.to_str().unwrap()
    );
    assert!(home.join(".local/state/relay-agent").is_dir());
    assert!(acquired.get("link").is_some(), "{acquired}");
    // Its files are its own: under its state folder, none in the operator's
    // home, and its LaunchAgent is outside ~/Library/LaunchAgents.
    let config: Value = serde_json::from_slice(
        &std::fs::read(home.join(".config/relay-agent/config.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        config["broker_url"],
        format!("wss://localhost:{}/relay", lab.session_port)
    );
    assert_eq!(
        config["agent_credential"].as_str().unwrap(),
        std::fs::read_to_string(lab.state.join("relay/durable-agent.credential"))
            .unwrap()
            .trim()
    );
    assert!(home
        .join(format!("Library/LaunchAgents/{label}.plist"))
        .is_file());
    assert!(!lab.home.join(".config/relay-agent").exists());
    assert!(!lab.home.join(".local/libexec/relay-agent").exists());
    assert!(!lab.home.join("Library/LaunchAgents").exists());
    let calls = lab.launchctl_calls();
    assert_eq!(calls.matches("bootstrap gui/").count(), 1, "{calls}");
    assert!(lab
        .log()
        .contains(&format!("relay agent {label} was started")));
    assert_eq!(lab.release_lease(&name, &lease).status, 200);

    // A restart finds the agent running as wanted and leaves it alone.
    lab.restart_daemon();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !lab
        .log()
        .contains(&format!("relay agent {label} runs as wanted"))
    {
        assert!(Instant::now() < deadline, "{}", lab.log());
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(lab.launchctl_calls().matches("bootstrap gui/").count(), 1);
    let (name, lease, _) = lab.acquire("none");
    lab.wait_for_relay(&name, &lease);
    assert_eq!(lab.release_lease(&name, &lease).status, 200);

    // DC-231: an ad hoc pre-release names no trusted team; a recorded one
    // shows in `/health` as a fingerprint, never as the team.
    let health = lab.request("GET", "/health", None).body;
    assert_eq!(health["relay_agent_team"], Value::Null, "{health}");
    // DC-236: the daemon checked the release it holds itself.
    assert_eq!(health["relay_agent_release"]["trusted"], true, "{health}");
    let record = lab
        .state
        .join(machine_service_core::service_layout::signing::TRUST_RECORD);
    std::fs::write(&record, r#"{"team": "EXAMPLE123"}"#).unwrap();
    let health = lab.request("GET", "/health", None).body;
    assert_eq!(
        health["relay_agent_team"],
        json!({
            "fingerprint": machine_service_core::service_layout::signing::fingerprint("EXAMPLE123"),
            "origin": "recorded",
        })
    );
    assert!(!health.to_string().contains("EXAMPLE123"), "{health}");
}

/// DC-242, keeping DC-241's guarantee: a test daemon runs only the
/// `launchctl` its test names through the test build's seam. One named
/// none, with a recording `launchctl` first on its `PATH`, refuses before
/// it runs any: neither the one `PATH` finds nor `/bin/launchctl`. Its
/// release is untrusted, so a daemon built without the seam would stop at
/// the trust check and still load nothing, and this test fails for want of
/// the refusal instead of reaching this Mac's launchd.
#[test]
fn a_test_daemon_never_runs_a_launchctl_it_is_not_given() {
    assert!(
        machine_service_core::launchd::is_test_build(),
        "the test daemon is built without the launchctl seam"
    );
    let lab = Lab::unnamed(relay_agent_bin());
    let refusal = format!(
        "is not provisioned: this is a test build ({}), which runs only the launchctl a test names in {}",
        machine_service_core::launchd::TEST_FEATURE,
        machine_service_core::launchd::TEST_VARIABLE
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    while !lab.log().contains(&refusal) {
        assert!(Instant::now() < deadline, "{}", lab.log());
        std::thread::sleep(Duration::from_millis(100));
    }
    let calls = lab.launchctl_calls();
    assert!(calls.is_empty(), "a launchctl on PATH ran: {calls}");
    assert!(!lab.state.join("relay/agent/Library/LaunchAgents").exists());
}

/// Everything under `root`, as text.
fn tree_text(root: &Path) -> String {
    let mut out = String::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        if path.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&path) {
                stack.extend(entries.flatten().map(|entry| entry.path()));
            }
        } else if let Ok(bytes) = std::fs::read(&path) {
            out.push_str(&String::from_utf8_lossy(&bytes));
        }
    }
    out
}

/// RA-2, DM-4: through the relay agent a lease's command is the agent's
/// child, with the lease's session environment and working directory, its
/// loaded keys redacted from the answer and never left on disk by the
/// launcher; a file push and pull go through it too, and a folder by the
/// service.
#[test]
fn m4_exec_push_and_pull_run_through_the_relay_agent() {
    let mut lab = Lab::new(relay_agent_bin());
    let agent = lab.start_agent();
    let (name, lease, acquired) = lab.acquire("default");
    lab.wait_for_relay(&name, &lease);
    assert_eq!(acquired["kind"], "durable");

    let answer = lab.exec(
        &name,
        json!({"lease_id": lease, "script": "ps -o ppid= -p $PPID | tr -d ' '\necho $MACHINE_SERVICE_LEASE_ID\npwd\necho \"owner=${ANTHROPIC_AUTH_TOKEN:-none}\"\necho \"key=$ANTHROPIC_API_KEY\"\n"}),
    );
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert_eq!(answer.body["link"], "relay");
    assert!(answer.body["execution_id"].as_str().is_some());
    let output = answer.body["output"].as_str().unwrap();
    let lines: Vec<&str> = output.lines().collect();
    assert_eq!(
        lines[0],
        agent.to_string(),
        "with keys loaded the command is the child of the launcher, the agent's child (DC-221)"
    );
    assert_eq!(lines[1], lease);
    assert!(
        lines[2].ends_with(&format!("machine-service/leases/{lease}/work")),
        "{output}"
    );
    assert_eq!(
        lines[3], "owner=none",
        "no owner credential reaches the session"
    );
    assert_eq!(lines[4], "key=[redacted]");
    assert!(!output.contains(CLAUDE_KEY));
    // The launcher took its request: no key is left in the lease's state.
    let relay_dir = lab.state.join("leases").join(&lease).join("relay");
    assert!(!tree_text(&relay_dir).contains(CLAUDE_KEY));

    let line = lab.exec(
        &name,
        json!({"lease_id": lease, "argv": ["/bin/sh", "-c", "echo hi; exit 3"]}),
    );
    assert_eq!(line.status, 200, "{}", line.body);
    assert_eq!(line.body["rc"], 3);
    assert_eq!(line.body["output"], "hi\n");
    assert_eq!(line.body["link"], "relay");

    let timed = lab.exec(
        &name,
        json!({"lease_id": lease, "argv": ["sleep", "30"], "timeout": 1}),
    );
    assert_eq!(timed.status, 409, "{}", timed.body);
    assert!(timed.body["error"].as_str().unwrap().contains("timed out"));

    // DM-5 through the relay agent.
    let local = lab.root.join("input.txt");
    std::fs::write(&local, b"hello").unwrap();
    let pushed = lab.request(
        "POST",
        &format!("/machines/{name}/push"),
        Some(&json!({"lease_id": lease, "local_path": local, "remote_path": "in.txt"})),
    );
    assert_eq!(pushed.status, 200, "{}", pushed.body);
    assert_eq!(pushed.body["link"], "relay");
    assert_eq!(
        pushed.body["sha256"],
        "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
    );
    let back = lab.root.join("back.txt");
    let pulled = lab.request(
        "POST",
        &format!("/machines/{name}/pull"),
        Some(&json!({"lease_id": lease, "remote_path": "in.txt", "local_path": back})),
    );
    assert_eq!(pulled.status, 200, "{}", pulled.body);
    assert_eq!(pulled.body["link"], "relay");
    assert_eq!(std::fs::read(&back).unwrap(), b"hello");
    let folder = lab.root.join("folder");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("a"), b"a").unwrap();
    let tree = lab.request(
        "POST",
        &format!("/machines/{name}/push"),
        Some(&json!({"lease_id": lease, "local_path": folder, "remote_path": "folder"})),
    );
    assert_eq!(tree.status, 200, "{}", tree.body);
    assert_eq!(tree.body["link"], "in-session");

    assert_eq!(lab.release_lease(&name, &lease).status, 200);
    assert!(alive(agent), "release never ends the relay agent");
}

/// DM-7, HY-3: a release preempts a running relay command, stops its
/// process group, sweeps what it left in the background, and leaves the
/// relay agent and the agent's other work alone.
#[test]
fn m4_release_stops_and_sweeps_relay_commands_and_keeps_the_agent() {
    let mut lab = Lab::new(relay_agent_bin());
    let agent = lab.start_agent();
    let (name, lease, _) = lab.acquire("none");
    lab.wait_for_relay(&name, &lease);

    let background: i32 = lab
        .exec(
            &name,
            json!({"lease_id": lease, "argv": ["/bin/sh", "-c", "nohup sleep 3600 >/dev/null 2>&1 & echo $!"]}),
        )
        .body["output"]
        .as_str()
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let _guard = Leftover(background);
    assert!(alive(background));

    let port = lab.port;
    let path = format!("/machines/{name}/exec");
    let body = json!({"lease_id": lease, "argv": ["sleep", "120"]});
    let running =
        std::thread::spawn(move || http_timeout(port, "POST", &path, Some(&body), None, LONG));
    std::thread::sleep(Duration::from_millis(1500));
    let begun = Instant::now();
    let released = lab.release_lease(&name, &lease);
    assert_eq!(released.status, 200, "{}", released.body);
    let answer = running.join().unwrap();
    assert_eq!(answer.status, 409, "{}", answer.body);
    assert_eq!(
        answer.body["error"],
        format!("command cancelled by release of {name}")
    );
    assert!(begun.elapsed() < Duration::from_secs(60));
    assert!(wait_dead(background), "the background process survived");
    let hygiene = &released.body["hygiene"];
    let ended: Vec<&Value> = hygiene["stopped"]
        .as_array()
        .unwrap()
        .iter()
        .chain(hygiene["swept"].as_array().unwrap())
        .collect();
    assert!(
        ended.iter().any(|entry| entry["pid"] == background),
        "{hygiene}"
    );
    assert!(
        !ended.iter().any(|entry| entry["pid"] == agent),
        "{hygiene}"
    );
    assert!(alive(agent), "the relay agent survives the release");
}

/// RA-8: without a relay agent session, and with an agent whose build is
/// not the installed release's, a durable exec runs in the service's own
/// session and says why.
#[test]
fn m4_without_the_release_agent_exec_falls_back_to_the_session() {
    // No agent at all.
    let lab = Lab::new(relay_agent_bin());
    let (name, lease, acquired) = lab.acquire("none");
    assert_eq!(acquired["link"], "in-session", "{acquired}");
    let answer = lab.exec(&name, json!({"lease_id": lease, "argv": ["echo", "hi"]}));
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert_eq!(answer.body["link"], "in-session");
    assert_eq!(answer.body["output"], "hi\n");
    assert!(answer.body["link_reason"]
        .as_str()
        .unwrap()
        .contains("no session"));
    let released = lab.release_lease(&name, &lease);
    assert_eq!(released.status, 200);
    // DC-173: no call reached a relay agent, so there is nothing to purge.
    let records = &released.body["hygiene"]["relay_records"];
    assert_eq!(records["purged"], false, "{records}");
    assert!(records["skipped"].as_str().unwrap().contains("no call"));
    drop(lab);

    // An agent that is not the release's build (DC-140).
    let mut lab = Lab::new(Path::new("/usr/bin/true"));
    lab.start_agent_from(relay_agent_bin());
    let (name, lease, _) = lab.acquire("none");
    let deadline = Instant::now() + Duration::from_secs(60);
    let reason = loop {
        let answer = lab.exec(&name, json!({"lease_id": lease, "argv": ["true"]}));
        assert_eq!(answer.status, 200, "{}", answer.body);
        assert_eq!(answer.body["link"], "in-session");
        let reason = answer.body["link_reason"].as_str().unwrap().to_string();
        if reason.contains("not the installed release") || Instant::now() >= deadline {
            break reason;
        }
        std::thread::sleep(Duration::from_millis(250));
    };
    assert!(reason.contains("not the installed release"), "{reason}");
    assert_eq!(lab.release_lease(&name, &lease).status, 200);
}

/// The relay agent's record of every execution of `lease`.
fn agent_records(lab: &Lab) -> String {
    tree_text(&lab.agent_home.join(".local/state/relay-agent/executions"))
}

fn relay_records_failure(hygiene: &Value) -> Option<String> {
    hygiene["failures"]
        .as_array()
        .unwrap()
        .iter()
        .find(|failure| failure["step"] == "relay-records")
        .map(|failure| failure["reason"].as_str().unwrap().to_string())
}

/// DC-173, AK-5: the relay agent's records keep a command's output as the
/// agent saw it, which the launcher redacted (DC-221); release has
/// the agent purge the lease's records, and completes only after the agent
/// confirms.
#[test]
fn m4_release_purges_the_relay_agents_records_of_the_lease() {
    let mut lab = Lab::new(relay_agent_bin());
    let agent = lab.start_agent();
    let (name, lease, _) = lab.acquire("default");
    lab.wait_for_relay(&name, &lease);
    let answer = lab.exec(
        &name,
        json!({"lease_id": lease, "script": "echo \"key=$ANTHROPIC_API_KEY of $MACHINE_SERVICE_LEASE_ID\"\n"}),
    );
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert_eq!(answer.body["link"], "relay");
    assert_eq!(
        answer.body["output"],
        format!("key=[redacted] of {lease}\n")
    );
    let records = agent_records(&lab);
    assert!(
        records.contains(&format!("key=[redacted] of {lease}")),
        "the agent's record keeps the output as the agent saw it"
    );
    assert!(
        !records.contains(CLAUDE_KEY),
        "a key never reaches a record"
    );

    let released = lab.release_lease(&name, &lease);
    assert_eq!(released.status, 200, "{}", released.body);
    let hygiene = &released.body["hygiene"];
    let records = &hygiene["relay_records"];
    assert_eq!(records["purged"], true, "{hygiene}");
    assert!(records["records"].as_u64().unwrap() >= 2, "{records}");
    assert!(records["bytes"].as_u64().unwrap() > 0, "{records}");
    assert_eq!(relay_records_failure(hygiene), None, "{hygiene}");
    assert!(!agent_records(&lab).contains(&lease));
    assert!(alive(agent), "a purge never ends the relay agent");

    // The next lease's calls are its own; its release purges them too.
    let (_, next, _) = lab.acquire("none");
    lab.wait_for_relay(&name, &next);
    let released = lab.release_lease(&name, &next);
    assert_eq!(released.body["hygiene"]["relay_records"]["purged"], true);
}

/// DC-173: a release whose relay agent cannot confirm the purge still frees
/// the Mac (HY-1), and its hygiene record fails the `relay-records` step
/// with the reason; the purge stays pending until the agent confirms it
/// (DC-177).
#[test]
fn m4_release_records_a_purge_the_agent_cannot_confirm() {
    let mut lab = Lab::new(relay_agent_bin());
    lab.start_agent();
    let (name, lease, _) = lab.acquire("none");
    lab.wait_for_relay(&name, &lease);
    lab.stop_agent();

    let begun = Instant::now();
    let released = lab.release_lease(&name, &lease);
    assert_eq!(released.status, 200, "{}", released.body);
    assert!(begun.elapsed() < Duration::from_secs(60));
    let hygiene = &released.body["hygiene"];
    assert_eq!(hygiene["relay_records"]["purged"], false, "{hygiene}");
    let reason = relay_records_failure(hygiene).expect("the step failed");
    assert!(reason.contains("relay agent"), "{reason}");
    assert!(
        !agent_records(&lab).is_empty(),
        "the records stay until the agent registers again"
    );
    // The Mac is free again.
    let (_, again, _) = lab.acquire("none");
    assert_eq!(lab.release_lease(&name, &again).status, 200);
}

/// How often the broker logged a confirmed purge of `lease`.
fn confirmations(lab: &Lab, lease: &str) -> usize {
    lab.log()
        .lines()
        .filter(|line| line.contains("relay: ") && line.ends_with(&format!(" of lease {lease}")))
        .filter(|line| line.contains(" purged "))
        .count()
}

/// DC-177: a purge the release could not confirm stays pending in the
/// instance's state, across a restart of the instance, and is sent again
/// when the relay agent registers; once confirmed it leaves the list, the
/// lease's hygiene record says so, and it is never sent again.
#[test]
fn m4_an_unconfirmed_purge_stays_pending_until_the_agent_confirms_it() {
    let mut lab = Lab::new(relay_agent_bin());
    lab.start_agent();
    let (name, lease, _) = lab.acquire("default");
    lab.wait_for_relay(&name, &lease);
    let answer = lab.exec(
        &name,
        json!({"lease_id": lease, "script": "echo \"key=$ANTHROPIC_API_KEY of $MACHINE_SERVICE_LEASE_ID\"\n"}),
    );
    assert_eq!(answer.body["link"], "relay", "{}", answer.body);
    assert!(agent_records(&lab).contains(&format!("of {lease}")));
    assert!(!agent_records(&lab).contains(CLAUDE_KEY));

    // The agent is down at release: the Mac is freed and the purge waits.
    lab.stop_agent();
    let released = lab.release_lease(&name, &lease);
    assert_eq!(released.status, 200, "{}", released.body);
    let hygiene = &released.body["hygiene"];
    assert_eq!(hygiene["relay_records"]["purged"], false, "{hygiene}");
    assert_eq!(hygiene["relay_records"]["pending"], true, "{hygiene}");
    assert!(relay_records_failure(hygiene).is_some(), "{hygiene}");
    assert_eq!(lab.pending_purges(&name), std::slice::from_ref(&lease));

    // A restart of the instance keeps it pending.
    lab.restart_daemon();
    assert_eq!(lab.pending_purges(&name), std::slice::from_ref(&lease));
    assert!(agent_records(&lab).contains(&format!("of {lease}")));
    assert_eq!(confirmations(&lab, &lease), 0);

    // The agent registers again: the purge is sent and confirmed.
    let registered = lab.log().matches("agent registered").count();
    lab.start_agent();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !lab.pending_purges(&name).is_empty() {
        assert!(
            Instant::now() < deadline,
            "the pending purge was not confirmed:\n{}",
            lab.log()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(!agent_records(&lab).contains(&lease));
    assert_eq!(confirmations(&lab, &lease), 1);
    let record = lab.request(
        "GET",
        &format!("/machines/{name}/leases/{lease}/hygiene"),
        None,
    );
    assert_eq!(record.status, 200, "{}", record.body);
    let records = &record.body["relay_records"];
    assert_eq!(records["purged"], true, "{records}");
    assert!(records["purged_at"].as_f64().is_some(), "{records}");
    assert!(records["pending_since"].as_f64().is_some(), "{records}");

    // A confirmed purge is never sent again: not when the agent registers
    // again, nor after a restart of the instance.
    lab.stop_agent();
    lab.start_agent();
    lab.wait_for_registrations(registered + 2);
    lab.restart_daemon();
    let registered = lab.log().matches("agent registered").count();
    lab.start_agent();
    lab.wait_for_registrations(registered + 1);
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(confirmations(&lab, &lease), 1, "{}", lab.log());
    assert!(lab.pending_purges(&name).is_empty());
}

/// DC-177: a purge confirmed at release is never pending and is not sent
/// again when the agent registers later.
#[test]
fn m4_a_purge_confirmed_at_release_is_never_sent_again() {
    let mut lab = Lab::new(relay_agent_bin());
    lab.start_agent();
    let (name, lease, _) = lab.acquire("none");
    lab.wait_for_relay(&name, &lease);
    let released = lab.release_lease(&name, &lease);
    assert_eq!(released.body["hygiene"]["relay_records"]["purged"], true);
    assert!(lab.pending_purges(&name).is_empty());
    assert_eq!(confirmations(&lab, &lease), 1);

    let registered = lab.log().matches("agent registered").count();
    lab.stop_agent();
    lab.start_agent();
    lab.wait_for_registrations(registered + 1);
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(confirmations(&lab, &lease), 1, "{}", lab.log());
}

/// The SHA-256 of `bytes`, as `shasum -a 256` prints it.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A relay client of the lease, as mcp-relay connects one from the
/// acquisition's answer (RP-1, RP-11).
fn lease_client(acquired: &Value) -> TestClient {
    let relay = &acquired["relay"];
    let pem = relay["ca_pem"].as_str().expect("the answer carries relay");
    let mut config = client_config_v2(
        relay["url"].as_str().unwrap(),
        relay["credential"].as_str().unwrap(),
    );
    config.tls = Some(remote_session::tls::client_config(&[pem.as_bytes()]).unwrap());
    TestClient::connect_with(config).unwrap()
}

fn exit_rc(answer: &Collected) -> Option<i32> {
    match &answer.result.as_ref().expect("a result").outcome {
        Outcome::Exit { rc, .. } => *rc,
        other => panic!("not an exit: {other:?}"),
    }
}

/// What a lease's command sees: its lease, its working directory and
/// temporary folder, its key as a command sees it and as its SHA-256, the
/// owner's credential, and its whole environment.
const PROBE: &str = r#"echo "lease=$MACHINE_SERVICE_LEASE_ID"
echo "pwd=$(pwd)"
echo "tmp=$TMPDIR"
echo "key=$ANTHROPIC_API_KEY"
printf %s "$ANTHROPIC_API_KEY" | shasum -a 256 | cut -c1-64
echo "owner=${ANTHROPIC_AUTH_TOKEN:-none}"
env | LC_ALL=C sort | grep -v '^_='
"#;

/// DC-219, DM-4, AK-5: a relay client's exec and script on a
/// durable lease start through the lease's launcher, as machine-service's own
/// exec does: they see the lease's working directory, variables and agent
/// keys exactly as an HTTP exec sees them, standard input included, and a
/// query names the client's own operation. No key appears in any relay
/// message the client receives, in the relay agent's records, spools or log,
/// in the lease's state or in the service's log. Release stops the client's
/// commands and what they left, and purges every record of them.
#[test]
fn m4_a_relay_clients_commands_run_as_the_leases_http_exec() {
    let mut lab = Lab::new(relay_agent_bin());
    let agent = lab.start_agent();
    let (name, lease, acquired) = lab.acquire("default");
    lab.wait_for_relay(&name, &lease);
    let client = lease_client(&acquired);
    const WAIT: Duration = Duration::from_secs(60);

    // The same script over HTTP and over the broker.
    let http = lab.exec(&name, json!({"lease_id": lease, "script": PROBE}));
    assert_eq!(http.status, 200, "{}", http.body);
    assert_eq!(http.body["link"], "relay");
    client.call(
        1,
        "probe-script",
        Op::Script { timeout_s: 60.0 },
        PROBE.as_bytes(),
    );
    let relayed = client.wait(1, WAIT);
    assert_eq!(exit_rc(&relayed), Some(0), "{relayed:?}");
    assert!(relayed.end_matches());
    let seen = String::from_utf8(relayed.bytes.clone()).unwrap();
    assert_eq!(seen, http.body["output"].as_str().unwrap());
    let work = lab
        .home
        .join(format!("machine-service/leases/{lease}/work"));
    for line in [
        format!("lease={lease}"),
        format!("pwd={}", work.display()),
        "key=[redacted]".to_string(),
        sha256_hex(CLAUDE_KEY.as_bytes()),
        "owner=none".to_string(),
        "ANTHROPIC_API_KEY=[redacted]".to_string(),
        "ANTHROPIC_BASE_URL=http://<gateway-host>".to_string(),
        format!("MACHINE_SERVICE_LEASE_ID={lease}"),
    ] {
        assert!(
            seen.lines().any(|seen| seen == line),
            "no {line:?} in:\n{seen}"
        );
    }
    assert!(
        seen.contains(&format!(
            "tmp={}",
            lab.home
                .join(format!("machine-service/leases/{lease}/tmp"))
                .display()
        )),
        "{seen}"
    );

    // An exec, with and without standard input, as an argv over HTTP.
    let line = r#"echo "$MACHINE_SERVICE_LEASE_ID $(pwd)"; printf %s "$ANTHROPIC_API_KEY" | shasum -a 256 | cut -c1-64; exit 7"#;
    let http = lab.exec(
        &name,
        json!({"lease_id": lease, "argv": ["/bin/zsh", "-c", line]}),
    );
    client.call(2, "probe-exec", relay_common::exec(line), b"");
    let relayed = client.wait(2, WAIT);
    assert_eq!(exit_rc(&relayed), Some(7));
    assert_eq!(http.body["rc"], 7, "{}", http.body);
    assert_eq!(
        String::from_utf8_lossy(&relayed.bytes),
        http.body["output"].as_str().unwrap()
    );
    let input = vec![b'x'; 300_000];
    client.call(
        3,
        "probe-stdin",
        Op::Exec {
            command: "shasum -a 256 | cut -c1-64".into(),
            stdin: true,
            timeout_s: 60.0,
        },
        &input,
    );
    let relayed = client.wait(3, WAIT);
    assert_eq!(
        String::from_utf8_lossy(&relayed.bytes).trim(),
        sha256_hex(&input)
    );
    client.query(4, "probe-script");
    match client.wait(4, WAIT).query.unwrap() {
        ExecutionState::Completed { op, .. } => assert_eq!(op, OpKind::Script),
        other => panic!("{other:?}"),
    }

    // A command that runs on and one that leaves a process behind.
    client.call(
        5,
        "leaves",
        relay_common::exec("nohup sleep 3600 >/dev/null 2>&1 & echo $!"),
        b"",
    );
    let left = client.wait(5, WAIT);
    let background: i32 = String::from_utf8_lossy(&left.bytes).trim().parse().unwrap();
    let _guard = Leftover(background);
    client.call(
        6,
        "runs-on",
        relay_common::exec("echo started; sleep 300"),
        b"",
    );
    let deadline = Instant::now() + WAIT;
    while client.seen(6).bytes.is_empty() {
        assert!(Instant::now() < deadline, "the long command never started");
    }

    // Nothing the client received, and nothing the agent or the service
    // kept, holds a key; the launcher's requests are gone.
    let mut received = format!("{relayed:?}{left:?}{:?}", client.seen(6));
    received.push_str(&seen);
    assert!(!received.contains(CLAUDE_KEY));
    assert!(
        !tree_text(&lab.agent_home).contains(CLAUDE_KEY),
        "the agent's files hold a key"
    );
    assert!(!std::fs::read_to_string(lab.root.join("agent.log"))
        .unwrap()
        .contains(CLAUDE_KEY));
    assert!(!lab.log().contains(CLAUDE_KEY));
    let relay_dir = lab.state.join("leases").join(&lease).join("relay");
    assert!(
        !tree_text(&relay_dir).contains(CLAUDE_KEY),
        "a request was left behind"
    );

    // Release stops both, and purges every record of the client's calls.
    let released = lab.release_lease(&name, &lease);
    assert_eq!(released.status, 200, "{}", released.body);
    assert!(wait_dead(background), "the background process survived");
    let running = client.wait(6, WAIT);
    assert!(running.result.is_some());
    let hygiene = &released.body["hygiene"];
    let ended: Vec<&Value> = hygiene["stopped"]
        .as_array()
        .unwrap()
        .iter()
        .chain(hygiene["swept"].as_array().unwrap())
        .collect();
    assert!(
        ended.iter().any(|entry| entry["pid"] == background),
        "{hygiene}"
    );
    let records = &hygiene["relay_records"];
    assert_eq!(records["purged"], true, "{hygiene}");
    assert!(records["records"].as_u64().unwrap() >= 7, "{records}");
    assert!(
        !agent_records(&lab).contains(&lease),
        "a record of the lease survived the purge"
    );
    assert!(!tree_text(&relay_dir).contains(CLAUDE_KEY));
    assert!(alive(agent), "the relay agent survives the release");
}

/// A relay client of the lease that may be refused.
fn try_lease_client(acquired: &Value) -> Result<TestClient, remote_session::Error> {
    let relay = &acquired["relay"];
    let pem = relay["ca_pem"].as_str().expect("the answer carries relay");
    let mut config = client_config_v2(
        relay["url"].as_str().unwrap(),
        relay["credential"].as_str().unwrap(),
    );
    config.tls = Some(remote_session::tls::client_config(&[pem.as_bytes()]).unwrap());
    TestClient::connect_with(config)
}

/// DM-8, RP-3, AC-64: a durable lease lives on through a restart of its
/// instance, and so does its relay client credential: the record keeps its
/// hash, never the credential, and the restarted broker accepts it again
/// until the lease is released.
#[test]
fn m4_a_durable_leases_client_credential_outlives_a_restart() {
    let mut lab = Lab::new(relay_agent_bin());
    lab.start_agent();
    let (name, lease, acquired) = lab.acquire("default");
    lab.wait_for_relay(&name, &lease);
    let credential = acquired["relay"]["credential"].as_str().unwrap();
    let state = tree_text(&lab.state);
    assert!(!state.contains(credential), "the credential is on disk");
    assert!(
        state.contains(&sha256_hex(credential.as_bytes())),
        "no record keeps the credential's hash"
    );
    drop(lease_client(&acquired));

    let registered = lab.log().matches("agent registered").count();
    lab.restart_daemon();
    lab.wait_for_registrations(registered + 1);
    let client = try_lease_client(&acquired).expect("the restarted broker refuses the lease");
    client.call(1, "after-restart", relay_common::exec("echo back"), b"");
    let answer = client.wait(1, Duration::from_secs(60));
    assert_eq!(exit_rc(&answer), Some(0), "{answer:?}");
    assert_eq!(String::from_utf8_lossy(&answer.bytes), "back\n");
    drop(client);

    let released = lab.release_lease(&name, &lease);
    assert_eq!(released.status, 200, "{}", released.body);
    assert!(
        try_lease_client(&acquired).is_err(),
        "a released lease's credential is accepted"
    );
}
