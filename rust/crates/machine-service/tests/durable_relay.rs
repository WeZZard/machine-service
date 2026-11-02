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

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::{
    canonical_dir, free_port, http_timeout, relay_agent_bin, try_http, write_relay_release, DAEMON,
};
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
    _dir: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    agent_home: PathBuf,
    state: PathBuf,
    release: PathBuf,
    session_port: u16,
    port: u16,
    daemon: Option<Child>,
    agent: Option<Child>,
}

impl Lab {
    /// A second instance and its relay agent; `release_of` is the
    /// executable the instance's release holds.
    fn new(release_of: &Path) -> Lab {
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
            state: root.join("state"),
            _dir: dir,
            root,
            home,
            agent_home,
            release,
            session_port: 0,
            port: 0,
            daemon: None,
            agent: None,
        };
        lab.start_daemon(&config);
        lab
    }

    fn start_daemon(&mut self, config: &Path) {
        std::fs::create_dir_all(&self.state).unwrap();
        for _ in 0..16 {
            let port = free_port();
            let session_port = free_port();
            let err = self.root.join(format!("daemon-{port}.err"));
            let mut child = Command::new(DAEMON)
                .env_clear()
                .env("HOME", &self.home)
                .env("USER", user())
                .env("LOGNAME", user())
                .env("SHELL", "/bin/zsh")
                .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
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
                .stderr(Stdio::from(std::fs::File::create(&err).unwrap()))
                .spawn()
                .unwrap();
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
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        panic!("the daemon did not start");
    }

    /// Configure and start the relay agent as the installer does (IN-3).
    fn start_agent(&mut self) -> i32 {
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
        let child = Command::new(relay_agent_bin())
            .arg("run")
            .env("HOME", &self.agent_home)
            .env("SHELL", "/bin/sh")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(
                std::fs::File::create(self.root.join("agent.log")).unwrap(),
            ))
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        self.agent = Some(child);
        pid
    }

    fn stop_agent(&mut self) {
        if let Some(mut agent) = self.agent.take() {
            let _ = agent.kill();
            let _ = agent.wait();
        }
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
            // SAFETY: the PID is this test's own child.
            unsafe { libc::kill(daemon.id() as i32, libc::SIGTERM) };
            let _ = daemon.wait();
        }
    }
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
        json!({"lease_id": lease, "script": "echo $PPID\necho $MACHINE_SERVICE_LEASE_ID\npwd\necho \"owner=${ANTHROPIC_AUTH_TOKEN:-none}\"\necho \"key=$ANTHROPIC_API_KEY\"\n"}),
    );
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert_eq!(answer.body["link"], "relay");
    assert!(answer.body["execution_id"].as_str().is_some());
    let output = answer.body["output"].as_str().unwrap();
    let lines: Vec<&str> = output.lines().collect();
    assert_eq!(
        lines[0],
        agent.to_string(),
        "the command is the agent's child"
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
    lab.start_agent();
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
/// agent saw it, keys and all; release has the agent purge the lease's
/// records, and completes only after the agent confirms.
#[test]
fn m4_release_purges_the_relay_agents_records_of_the_lease() {
    let mut lab = Lab::new(relay_agent_bin());
    let agent = lab.start_agent();
    let (name, lease, _) = lab.acquire("default");
    lab.wait_for_relay(&name, &lease);
    let answer = lab.exec(
        &name,
        json!({"lease_id": lease, "script": "echo \"key=$ANTHROPIC_API_KEY\"\n"}),
    );
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert_eq!(answer.body["link"], "relay");
    assert_eq!(answer.body["output"], "key=[redacted]\n");
    assert!(
        agent_records(&lab).contains(CLAUDE_KEY),
        "the agent's record keeps the output unredacted"
    );

    let released = lab.release_lease(&name, &lease);
    assert_eq!(released.status, 200, "{}", released.body);
    let hygiene = &released.body["hygiene"];
    let records = &hygiene["relay_records"];
    assert_eq!(records["purged"], true, "{hygiene}");
    assert!(records["records"].as_u64().unwrap() >= 2, "{records}");
    assert!(records["bytes"].as_u64().unwrap() > 0, "{records}");
    assert_eq!(relay_records_failure(hygiene), None, "{hygiene}");
    assert!(!agent_records(&lab).contains(CLAUDE_KEY));
    assert!(alive(agent), "a purge never ends the relay agent");

    // The next lease's calls are its own; its release purges them too.
    let (_, next, _) = lab.acquire("none");
    lab.wait_for_relay(&name, &next);
    let released = lab.release_lease(&name, &next);
    assert_eq!(released.body["hygiene"]["relay_records"]["purged"], true);
}

/// DC-173: a release whose relay agent cannot confirm the purge still frees
/// the Mac (HY-1), and its hygiene record fails the `relay-records` step
/// with the reason; the agent's safety bound removes the records later.
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
        "the records stay until the safety bound"
    );
    // The Mac is free again.
    let (_, again, _) = lab.acquire("none");
    assert_eq!(lab.release_lease(&name, &again).status, 200);
}
