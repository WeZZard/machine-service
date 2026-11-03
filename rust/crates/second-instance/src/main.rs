//! `second-instance`: start, restart, stop and report a second
//! machine-service instance under a root it owns (`docs/arch/test-layers.md`,
//! TL-4, DC-1xx-LIVE-1).
//!
//! ```text
//! second-instance start --root DIR --port N --session-port N --tart-home DIR
//!     [--bind ADDR] [--guest-link ssh|ssh-session|relay] [--executable PATH]
//!     [--durable [--durable-cell CELL]] [--env NAME=VALUE]...
//! second-instance restart --root DIR     # the daemon only; leases live on
//! second-instance stop --root DIR        # releases, deletes and removes everything
//! second-instance status --root DIR
//! second-instance exposure --root DIR    # what it exposes, for a run's evidence
//! ```
//!
//! `start` writes the root's manifest before it makes anything, and `stop`
//! works from the manifest alone, so it finishes what a crashed run left:
//! it releases every lease through the daemon (starting it again if it
//! died), stops the daemon, deletes every VM in the Tart store whose name
//! carries the instance's prefix, unloads every relay agent under its state
//! folder, removes the durable workspaces of its leases and removes the
//! root. It removes the manifest last, and only when nothing is left, so it
//! can be run again until it is.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::net::{IpAddr, TcpListener};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use second_instance::{
    is_loopback, is_wildcard, overlaps, owned_agent_label, Manifest, INSTALLED_CONFIG,
    INSTALLED_PORT, INSTALLED_SESSION_PORT, INSTALLED_STATE, MARKER_VARIABLE,
    SESSION_HOST_VARIABLE,
};
use serde_json::{json, Value};

/// The `PATH` the daemon gets: the system's and Homebrew's, never the
/// caller's.
const PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";
/// The caller's variables the daemon keeps.
const INHERITED: [&str; 5] = ["HOME", "USER", "LOGNAME", "TMPDIR", "LANG"];
/// The variables the launcher sets itself, which `--env` may not.
const OWNED: [&str; 12] = [
    MARKER_VARIABLE,
    SESSION_HOST_VARIABLE,
    "MACHINE_SERVICE_PORT",
    "MACHINE_SERVICE_HOST",
    "MACHINE_SERVICE_STATE",
    "MACHINE_SERVICE_CONFIG_DIR",
    "MACHINE_SERVICE_SESSION_PORT",
    "MACHINE_SERVICE_GUEST_LINK",
    "MACHINE_SERVICE_EXPOSE_DURABLE",
    "MACHINE_SERVICE_DURABLE_LOCK_DIR",
    "MACHINE_SERVICE_ENVIRONMENT",
    "MACHINE_SERVICE_ENVIRONMENT_PROFILE",
];
/// Variables outside `MACHINE_SERVICE_*` that `--env` may set.
const EXTRA: [&str; 2] = ["PILOT_REPO", "PILOT_IMAGES_STATE_DIR"];
const LAUNCHCTL: &str = "/bin/launchctl";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("start") => start(&args[1..]),
        Some("restart") => root_of(&args[1..]).and_then(|root| restart(&root)),
        Some("stop") => root_of(&args[1..]).and_then(|root| stop(&root)),
        Some("status") => root_of(&args[1..]).and_then(|root| status(&root)),
        Some("exposure") => root_of(&args[1..]).and_then(|root| exposure(&root)),
        _ => Err(usage()),
    };
    if let Err(error) = result {
        eprintln!("second-instance: {error}");
        std::process::exit(1);
    }
}

fn usage() -> String {
    "usage: second-instance start --root DIR --port N --session-port N --tart-home DIR \
[--bind ADDR] [--guest-link LINK] [--executable PATH] [--durable [--durable-cell CELL]] \
[--env NAME=VALUE]... | restart|stop|status|exposure --root DIR"
        .into()
}

/// The parsed flags: each `--name value`, `--durable` alone, and every
/// `--env`.
struct Flags {
    values: BTreeMap<String, String>,
    env: Vec<String>,
    durable: bool,
}

fn flags(args: &[String]) -> Result<Flags, String> {
    let mut parsed = Flags {
        values: BTreeMap::new(),
        env: Vec::new(),
        durable: false,
    };
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let name = flag
            .strip_prefix("--")
            .ok_or_else(|| format!("unexpected argument {flag:?}; {}", usage()))?;
        if name == "durable" {
            parsed.durable = true;
            continue;
        }
        let value = rest
            .next()
            .ok_or_else(|| format!("--{name} needs a value"))?
            .clone();
        if name == "env" {
            parsed.env.push(value);
        } else if parsed.values.insert(name.to_string(), value).is_some() {
            return Err(format!("--{name} is given twice"));
        }
    }
    Ok(parsed)
}

fn absolute(path: &str, what: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(format!(
            "{what} must be an absolute path: {}",
            path.display()
        ))
    }
}

fn root_of(args: &[String]) -> Result<PathBuf, String> {
    let flags = flags(args)?;
    let root = absolute(
        flags.values.get("root").ok_or("--root is required")?,
        "--root",
    )?;
    Ok(
        match (root.parent().map(Path::canonicalize), root.file_name()) {
            (Some(Ok(parent)), Some(name)) => parent.join(name),
            _ => root,
        },
    )
}

/// The root with its parent's links resolved, in a folder that no one else
/// can write: the daemon trusts only configuration that its user alone can
/// change, so a root in `/tmp` would be refused later and less clearly.
fn owned_root(root: &Path) -> Result<PathBuf, String> {
    use std::os::unix::fs::MetadataExt as _;
    let (parent, name) = match (root.parent(), root.file_name()) {
        (Some(parent), Some(name)) => (parent, name),
        _ => return Err(format!("--root {} has no parent folder", root.display())),
    };
    let parent = parent
        .canonicalize()
        .map_err(|e| format!("--root's parent {}: {e}", parent.display()))?;
    // SAFETY: `getuid` has no preconditions.
    let uid = unsafe { libc::getuid() };
    for folder in parent.ancestors() {
        let info = std::fs::metadata(folder).map_err(|e| format!("{}: {e}", folder.display()))?;
        if info.mode() & 0o022 != 0 || !(info.uid() == 0 || info.uid() == uid) {
            return Err(format!(
                "--root {} lies in {}, which others can write; use a folder of your own",
                root.display(),
                folder.display()
            ));
        }
    }
    Ok(parent.join(name))
}

fn home() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set".into())
}

fn port(flags: &Flags, name: &str) -> Result<u16, String> {
    let value = flags
        .values
        .get(name)
        .ok_or_else(|| format!("--{name} is required: a second instance names its own ports"))?;
    value
        .parse::<u16>()
        .ok()
        .filter(|p| *p > 0)
        .ok_or_else(|| format!("--{name} {value:?} is not a port"))
}

/// The manifest the flags ask for, with `id` and `created_root`: every
/// check that needs nothing but the flags and the caller's environment.
fn plan(args: &[String], id: &str, created_root: bool) -> Result<Manifest, String> {
    let flags = flags(args)?;
    let root = owned_root(&absolute(
        flags.values.get("root").ok_or("--root is required")?,
        "--root",
    )?)?;
    let home = home()?;
    for (installed, what) in [
        (home.join(INSTALLED_STATE), "state"),
        (home.join(INSTALLED_CONFIG), "configuration"),
        (home.join(".tart"), "Tart store"),
    ] {
        if overlaps(&root, &installed) {
            return Err(format!(
                "--root {} overlaps the installed instance's {what} folder",
                root.display()
            ));
        }
    }
    if home.starts_with(&root) {
        return Err(format!("--root {} holds the home folder", root.display()));
    }
    let http = port(&flags, "port")?;
    let session = port(&flags, "session-port")?;
    if [http, session]
        .iter()
        .any(|p| [INSTALLED_PORT, INSTALLED_SESSION_PORT].contains(p))
    {
        return Err(format!(
            "ports {INSTALLED_PORT} and {INSTALLED_SESSION_PORT} are the installed instance's"
        ));
    }
    if http == session {
        return Err("--port and --session-port must differ".into());
    }
    let bind = flags
        .values
        .get("bind")
        .cloned()
        .unwrap_or_else(|| "127.0.0.1".into());
    if is_wildcard(&bind) || bind.parse::<IpAddr>().is_err() {
        return Err(format!(
            "--bind {bind:?} must be one address of this host, never every interface"
        ));
    }
    let durable_cell = flags.values.get("durable-cell").cloned();
    if durable_cell.is_some() && !flags.durable {
        return Err("--durable-cell needs --durable".into());
    }
    if flags.durable && !is_loopback(&bind) && durable_cell.is_none() {
        return Err(format!(
            "durable exposure on {bind}, off loopback, is for a durable acceptance cell only: \
name it with --durable-cell"
        ));
    }
    let tart_home = absolute(
        flags
            .values
            .get("tart-home")
            .ok_or("--tart-home is required: a second instance names its Tart store")?,
        "--tart-home",
    )?;
    if !tart_home.is_dir() {
        return Err(format!(
            "--tart-home {} is not a folder",
            tart_home.display()
        ));
    }
    let executable = match flags.values.get("executable") {
        Some(path) => absolute(path, "--executable")?,
        None => std::env::current_exe()
            .map_err(|e| e.to_string())?
            .with_file_name("machine-service"),
    };
    if !executable.is_file() {
        return Err(format!("no machine-service at {}", executable.display()));
    }
    let guest_link = flags
        .values
        .get("guest-link")
        .cloned()
        .unwrap_or_else(|| "ssh".into());
    for key in flags.values.keys() {
        if ![
            "root",
            "port",
            "session-port",
            "bind",
            "durable-cell",
            "tart-home",
            "executable",
            "guest-link",
        ]
        .contains(&key.as_str())
        {
            return Err(format!("unknown flag --{key}; {}", usage()));
        }
    }
    let mut manifest = Manifest {
        id: id.to_string(),
        root,
        created_root,
        port: http,
        session_port: session,
        bind,
        tart_home,
        guest_link,
        durable: flags.durable,
        durable_cell,
        executable,
        env: BTreeMap::new(),
    };
    manifest.env = environment(&manifest, &flags.env)?;
    Ok(manifest)
}

/// The daemon's whole environment: a few of the caller's variables, the
/// launcher's own, and the `--env` settings it allows.
fn environment(m: &Manifest, extra: &[String]) -> Result<BTreeMap<String, String>, String> {
    let mut env: BTreeMap<String, String> = INHERITED
        .iter()
        .filter_map(|name| Some((name.to_string(), std::env::var(name).ok()?)))
        .collect();
    let state = m.state().to_string_lossy().into_owned();
    let config = m.config().to_string_lossy().into_owned();
    let console = m
        .config()
        .join("console.json")
        .to_string_lossy()
        .into_owned();
    let relay_url = format!("wss://localhost:{}/relay", m.session_port);
    let owned: [(&str, String); 13] = [
        ("PATH", PATH.into()),
        ("TART_HOME", m.tart_home.to_string_lossy().into_owned()),
        // A clone in a store prunes its cache unless told not to; the
        // operator's cached images are not the instance's to prune.
        ("TART_NO_AUTO_PRUNE", "1".into()),
        (MARKER_VARIABLE, m.id.clone()),
        ("MACHINE_SERVICE_PORT", m.port.to_string()),
        ("MACHINE_SERVICE_HOST", m.bind.clone()),
        ("MACHINE_SERVICE_SESSION_PORT", m.session_port.to_string()),
        (SESSION_HOST_VARIABLE, m.bind.clone()),
        ("MACHINE_SERVICE_STATE", state),
        ("MACHINE_SERVICE_CONFIG_DIR", config),
        ("MACHINE_SERVICE_GUEST_LINK", m.guest_link.clone()),
        ("MACHINE_SERVICE_CONSOLE_CONFIG", console),
        ("MACHINE_SERVICE_DISCOVERY_SCOPE", "local-only".into()),
    ];
    env.extend(owned.into_iter().map(|(k, v)| (k.to_string(), v)));
    if m.durable {
        env.insert("MACHINE_SERVICE_EXPOSE_DURABLE".into(), "on".into());
    }
    if is_loopback(&m.bind) {
        env.insert("MACHINE_SERVICE_RELAY_URL".into(), relay_url);
    }
    for setting in extra {
        let (name, value) = setting
            .split_once('=')
            .ok_or_else(|| format!("--env {setting:?} is not NAME=VALUE"))?;
        let allowed = (name.starts_with("MACHINE_SERVICE_") || EXTRA.contains(&name))
            && !OWNED.contains(&name);
        if !allowed {
            return Err(format!(
                "--env {name} is not allowed: only MACHINE_SERVICE_* settings the launcher does \
not set itself, and {}",
                EXTRA.join(", ")
            ));
        }
        env.insert(name.into(), value.into());
    }
    if !env.contains_key("HOME") {
        return Err("HOME is not set".into());
    }
    Ok(env)
}

/// Whether two manifests ask for the same instance.
fn same(a: &Manifest, b: &Manifest) -> bool {
    let key = |m: &Manifest| {
        let mut env = m.env.clone();
        env.remove(MARKER_VARIABLE);
        (
            m.port,
            m.session_port,
            m.bind.clone(),
            m.tart_home.clone(),
            m.guest_link.clone(),
            m.durable,
            m.durable_cell.clone(),
            m.executable.clone(),
            env.into_iter()
                .filter(|(k, _)| !INHERITED.contains(&k.as_str()))
                .collect::<Vec<_>>(),
        )
    };
    key(a) == key(b)
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let temporary = path.with_extension("tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&temporary)
        .map_err(|e| format!("{}: {e}", temporary.display()))?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .and_then(|_| file.write_all(bytes))
        .and_then(|_| file.sync_all())
        .map_err(|e| format!("{}: {e}", temporary.display()))?;
    std::fs::rename(&temporary, path).map_err(|e| format!("{}: {e}", path.display()))
}

fn mkdir(path: &Path) -> Result<(), String> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn start(args: &[String]) -> Result<(), String> {
    let root = root_of(args)?;
    if let Some(existing) = Manifest::read(&root)? {
        let wanted = plan(args, &existing.id, existing.created_root)?;
        if !same(&existing, &wanted) {
            return Err(format!(
                "{} holds a second instance with other settings; stop it first",
                root.display()
            ));
        }
        if let Some(pid) = owned_pid(&existing) {
            println!(
                "{}",
                json!({"already_running": pid, "exposure": existing.exposure()})
            );
            return Ok(());
        }
        return spawn(&existing);
    }
    let created_root = match std::fs::read_dir(&root).map(|entries| entries.count()) {
        Ok(0) => false,
        Ok(_) => return Err(format!("--root {} is not empty", root.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => return Err(format!("{}: {e}", root.display())),
    };
    let id = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let manifest = plan(args, &id, created_root)?;
    for port in [manifest.port, manifest.session_port] {
        TcpListener::bind((manifest.bind.as_str(), port)).map_err(|e| {
            format!(
                "cannot listen on {}:{port} ({e}): the address must be this host's and the port free",
                manifest.bind
            )
        })?;
    }
    mkdir(&root)?;
    write_private(
        &Manifest::file(&root),
        serde_json::to_string_pretty(&manifest)
            .map_err(|e| e.to_string())?
            .as_bytes(),
    )?;
    for folder in [manifest.state(), manifest.config(), manifest.packs()] {
        mkdir(&folder)?;
    }
    write_private(
        &manifest.config().join("console.json"),
        br#"{"schemaVersion": 1, "enabled": false}"#,
    )?;
    spawn(&manifest)
}

fn pid_file(m: &Manifest) -> PathBuf {
    m.root.join("daemon.pid")
}

/// The daemon's PID, while that process is this manifest's daemon.
fn owned_pid(m: &Manifest) -> Option<i32> {
    let pid: i32 = std::fs::read_to_string(pid_file(m))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let output = Command::new("/bin/ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let command = String::from_utf8_lossy(&output.stdout);
    command
        .trim()
        .starts_with(&*m.executable.to_string_lossy())
        .then_some(pid)
}

fn base(m: &Manifest) -> String {
    match m.bind.parse::<IpAddr>() {
        Ok(IpAddr::V6(ip)) => format!("http://[{ip}]:{}", m.port),
        _ => format!("http://{}:{}", m.bind, m.port),
    }
}

fn http() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(900)))
        .http_status_as_error(false)
        .proxy(None)
        .build()
        .into()
}

fn get(m: &Manifest, path: &str) -> Result<Value, String> {
    let mut response = http()
        .get(&format!("{}{path}", base(m)))
        .config()
        .timeout_global(Some(Duration::from_secs(5)))
        .build()
        .call()
        .map_err(|e| format!("GET {path}: {e}"))?;
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("GET {path}: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("GET {path}: {e}"))
}

fn post(m: &Manifest, path: &str, body: &Value) -> Result<(u16, String), String> {
    let mut response = http()
        .post(&format!("{}{path}", base(m)))
        .header("Content-Type", "application/json")
        .send(body.to_string())
        .map_err(|e| format!("POST {path}: {e}"))?;
    let text = response.body_mut().read_to_string().unwrap_or_default();
    Ok((response.status().as_u16(), text))
}

fn spawn(m: &Manifest) -> Result<(), String> {
    let log = |name: &str| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(m.root.join(name))
            .map_err(|e| format!("{name}: {e}"))
    };
    let mut command = Command::new(&m.executable);
    command
        .env_clear()
        .envs(&m.env)
        .current_dir(&m.root)
        .stdin(Stdio::null())
        .stdout(log("daemon.out")?)
        .stderr(log("daemon.err")?);
    // Its own session, so that a signal to the caller's terminal or task
    // never reaches the daemon or the `tart run` processes of its VMs.
    // SAFETY: `setsid` is async-signal-safe and touches no memory.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("{}: {e}", m.executable.display()))?;
    write_private(&pid_file(m), child.id().to_string().as_bytes())?;
    let started = Instant::now();
    loop {
        if let Ok(health) = get(m, "/health") {
            if health["second_instance"]["id"] == m.id.as_str() {
                println!(
                    "{}",
                    json!({"started": child.id(), "url": base(m), "exposure": m.exposure()})
                );
                return Ok(());
            }
            return Err(format!(
                "{} answers, but not as second instance {}: another daemon holds the port",
                base(m),
                m.id
            ));
        }
        if let Ok(Some(status)) = child.try_wait() {
            let err = std::fs::read_to_string(m.root.join("daemon.err")).unwrap_or_default();
            let tail: Vec<&str> = err.lines().rev().take(5).collect();
            return Err(format!(
                "the daemon exited ({status}): {}",
                tail.into_iter().rev().collect::<Vec<_>>().join(" | ")
            ));
        }
        if started.elapsed() > Duration::from_secs(30) {
            return Err(format!("the daemon did not answer on {} in 30 s", base(m)));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn terminate(m: &Manifest) -> Option<i32> {
    let pid = owned_pid(m)?;
    // SAFETY: plain signals to a PID this manifest's daemon holds.
    unsafe { libc::kill(pid, libc::SIGTERM) };
    let started = Instant::now();
    while owned_pid(m).is_some() {
        if started.elapsed() > Duration::from_secs(60) {
            // `stop` reports a daemon that outlives SIGKILL as left.
            return Some(pid);
        }
        if started.elapsed() > Duration::from_secs(30) {
            // SAFETY: as above.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = std::fs::remove_file(pid_file(m));
    Some(pid)
}

fn read_json(path: &Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(Value::Null)
}

/// The instance's leases, from its state folder: every VM record, and the
/// durable lease, if one is current.
fn leases(m: &Manifest) -> (Vec<String>, Option<String>) {
    let state = read_json(&m.state().join("state.json"));
    let vms = state["vms"]
        .as_object()
        .map(|vms| vms.keys().cloned().collect())
        .unwrap_or_default();
    let durable = read_json(&m.state().join("durable.json"))["current"]
        .as_str()
        .map(str::to_string);
    (vms, durable)
}

fn tart(m: &Manifest, args: &[&str]) -> Result<std::process::Output, String> {
    Command::new("tart")
        .args(args)
        .env_clear()
        .env("PATH", PATH)
        .env("HOME", m.env.get("HOME").cloned().unwrap_or_default())
        .env("TART_HOME", &m.tart_home)
        .env("TART_NO_AUTO_PRUNE", "1")
        .output()
        .map_err(|e| format!("tart {}: {e}", args.join(" ")))
}

/// The VMs of the store whose names carry the instance's prefix, and
/// whether each runs. No Tart on the Mac means no VMs.
fn owned_vms(m: &Manifest) -> Result<Vec<(String, bool)>, String> {
    let output = match tart(m, &["list", "--format", "json"]) {
        Ok(output) => output,
        Err(_) if !tart_installed() => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    if !output.status.success() {
        return Err(format!(
            "tart list: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let list: Value =
        serde_json::from_slice(&output.stdout).map_err(|e| format!("tart list: {e}"))?;
    let prefix = m.vm_prefix();
    Ok(list
        .as_array()
        .into_iter()
        .flatten()
        .filter(|vm| vm["Source"] == "local")
        .filter_map(|vm| {
            let name = vm["Name"].as_str()?;
            name.starts_with(&prefix)
                .then(|| (name.to_string(), vm["Running"].as_bool().unwrap_or(false)))
        })
        .collect())
}

fn tart_installed() -> bool {
    PATH.split(':')
        .any(|dir| Path::new(dir).join("tart").is_file())
}

/// The labels of the relay agents under the state folder (IN-3).
fn owned_agents(m: &Manifest) -> Vec<String> {
    let folder = m.state().join("relay/agent/Library/LaunchAgents");
    std::fs::read_dir(folder)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.strip_suffix(".plist").map(str::to_string)
        })
        .filter(|label| owned_agent_label(label))
        .collect()
}

fn agent_loaded(label: &str) -> bool {
    // SAFETY: `getuid` has no preconditions.
    let uid = unsafe { libc::getuid() };
    Command::new(LAUNCHCTL)
        .args(["print", &format!("gui/{uid}/{label}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn restart(root: &Path) -> Result<(), String> {
    let m =
        Manifest::read(root)?.ok_or_else(|| format!("no second instance in {}", root.display()))?;
    terminate(&m);
    spawn(&m)
}

fn status(root: &Path) -> Result<(), String> {
    let Some(m) = Manifest::read(root)? else {
        println!("{}", json!({"running": false, "manifest": null}));
        return Ok(());
    };
    let (vms, durable) = leases(&m);
    println!(
        "{}",
        json!({
            "running": owned_pid(&m).is_some(),
            "pid": owned_pid(&m),
            "url": base(&m),
            "exposure": m.exposure(),
            "vm_prefix": m.vm_prefix(),
            "leases": {"vms": vms, "durable": durable},
            "agents": owned_agents(&m)
                .into_iter()
                .map(|label| json!({"label": label, "loaded": agent_loaded(&label)}))
                .collect::<Vec<_>>(),
        })
    );
    Ok(())
}

fn exposure(root: &Path) -> Result<(), String> {
    let m =
        Manifest::read(root)?.ok_or_else(|| format!("no second instance in {}", root.display()))?;
    println!("{}", m.exposure());
    Ok(())
}

fn stop(root: &Path) -> Result<(), String> {
    let Some(m) = Manifest::read(root)? else {
        println!("{}", json!({"stopped": root, "nothing_owned": true}));
        return Ok(());
    };
    let mut report = json!({"stopped": m.root, "id": m.id});
    let (vms, durable) = leases(&m);
    let releases = !vms.is_empty() || durable.is_some();
    // A lease is released by its own daemon, which knows how; after a
    // crash, it is started again for that.
    if releases && owned_pid(&m).is_none() {
        report["restarted_to_release"] = json!(spawn(&m).is_ok());
    }
    if releases && owned_pid(&m).is_some() {
        let body = |lease: Option<&str>| {
            let mut body = json!({"reason": "second-instance stop"});
            if let Some(id) = lease {
                body["lease_id"] = json!(id);
            }
            body
        };
        let mut answers: Vec<Value> = vms
            .iter()
            .map(|vm| {
                let answer = post(&m, &format!("/machines/{vm}/release"), &body(None));
                json!({"vm": vm, "answer": format!("{answer:?}")})
            })
            .collect();
        if let Some(id) = &durable {
            let machine = get(&m, "/health")
                .ok()
                .and_then(|h| h["instance"].as_str().map(str::to_string));
            if let Some(machine) = machine {
                let answer = post(&m, &format!("/machines/{machine}/release"), &body(Some(id)));
                answers.push(json!({"durable": id, "answer": format!("{answer:?}")}));
            }
        }
        report["released"] = json!(answers);
    }
    report["daemon"] = json!(terminate(&m));

    let mut left = Vec::new();
    match owned_vms(&m) {
        Ok(owned) => {
            let deleted: Vec<String> = owned
                .into_iter()
                .map(|(vm, running)| {
                    if running {
                        let _ = tart(&m, &["stop", &vm]);
                    }
                    let _ = tart(&m, &["delete", &vm]);
                    vm
                })
                .collect();
            report["vms_deleted"] = json!(deleted);
        }
        Err(e) => left.push(format!("VMs: {e}")),
    }
    // SAFETY: `getuid` has no preconditions.
    let uid = unsafe { libc::getuid() };
    let agents = owned_agents(&m);
    for label in &agents {
        if agent_loaded(label) {
            let _ = Command::new(LAUNCHCTL)
                .args(["bootout", &format!("gui/{uid}/{label}")])
                .status();
        }
    }
    report["agents_unloaded"] = json!(agents);
    // A durable lease's workspace lies in the operator's home (DM-1); the
    // instance's leases are the ones under its state folder.
    let workspaces = m
        .env
        .get("HOME")
        .map(|home| Path::new(home).join("machine-service/leases"));
    let removed: Vec<String> = std::fs::read_dir(m.state().join("leases"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let id = entry.file_name().to_string_lossy().into_owned();
            let workspace = workspaces.as_ref()?.join(&id);
            (!id.is_empty() && !id.contains('/') && workspace.is_dir())
                .then(|| std::fs::remove_dir_all(&workspace).ok().map(|_| id))
                .flatten()
        })
        .collect();
    report["workspaces_removed"] = json!(removed);

    // Nothing may be left before the manifest goes.
    if owned_pid(&m).is_some() {
        left.push("the daemon still runs".into());
    }
    match owned_vms(&m) {
        Ok(vms) if !vms.is_empty() => left.push(format!(
            "VMs: {}",
            vms.into_iter()
                .map(|(vm, _)| vm)
                .collect::<Vec<_>>()
                .join(", ")
        )),
        _ => {}
    }
    left.extend(
        agents
            .iter()
            .filter(|label| agent_loaded(label))
            .map(|label| format!("relay agent {label} is still loaded")),
    );
    if !left.is_empty() {
        report["left"] = json!(left);
        println!("{report}");
        return Err(format!(
            "{} still owns: {}; run stop again",
            m.root.display(),
            left.join("; ")
        ));
    }
    for entry in ["state", "config", "daemon.out", "daemon.err", "daemon.pid"] {
        let path = m.root.join(entry);
        let _ = std::fs::remove_dir_all(&path).or_else(|_| std::fs::remove_file(&path));
    }
    std::fs::remove_file(Manifest::file(&m.root)).map_err(|e| e.to_string())?;
    if m.created_root {
        std::fs::remove_dir(&m.root).map_err(|e| format!("{}: {e}", m.root.display()))?;
    }
    report["root_removed"] = json!(m.created_root);
    println!("{report}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(bind: &str) -> Manifest {
        Manifest {
            id: "ab12".into(),
            root: PathBuf::from("/owned/root"),
            created_root: true,
            port: 6290,
            session_port: 6291,
            bind: bind.into(),
            tart_home: PathBuf::from("/store"),
            guest_link: "relay".into(),
            durable: false,
            durable_cell: None,
            executable: PathBuf::from("/bin/machine-service"),
            env: BTreeMap::new(),
        }
    }

    #[test]
    fn the_daemon_gets_the_launchers_settings_and_only_allowed_extras() {
        let env = environment(&manifest("127.0.0.1"), &["PILOT_REPO=/p".into()]).unwrap();
        assert_eq!(env["PATH"], PATH);
        assert_eq!(env[MARKER_VARIABLE], "ab12");
        assert_eq!(env["MACHINE_SERVICE_HOST"], "127.0.0.1");
        assert_eq!(env[SESSION_HOST_VARIABLE], "127.0.0.1");
        assert_eq!(env["MACHINE_SERVICE_STATE"], "/owned/root/state");
        assert_eq!(env["MACHINE_SERVICE_CONFIG_DIR"], "/owned/root/config");
        assert_eq!(env["TART_HOME"], "/store");
        assert_eq!(env["TART_NO_AUTO_PRUNE"], "1");
        assert_eq!(
            env["MACHINE_SERVICE_RELAY_URL"],
            "wss://localhost:6291/relay"
        );
        assert_eq!(env["PILOT_REPO"], "/p");
        assert!(!env.contains_key("MACHINE_SERVICE_EXPOSE_DURABLE"));
        let names: Vec<&String> = env.keys().collect();
        assert!(
            names.iter().all(|n| n.starts_with("MACHINE_SERVICE_")
                || n.starts_with("TART_")
                || INHERITED.contains(&n.as_str())
                || EXTRA.contains(&n.as_str())
                || n.as_str() == "PATH"),
            "{names:?}"
        );
    }

    #[test]
    fn an_extra_may_not_replace_what_the_launcher_owns() {
        let m = manifest("127.0.0.1");
        for setting in [
            "MACHINE_SERVICE_STATE=/x",
            "MACHINE_SERVICE_HOST=0.0.0.0",
            "MACHINE_SERVICE_SECOND_INSTANCE=zz",
            "PATH=/x",
            "TART_HOME=/x",
            "HOME=/x",
            "NOEQUALS",
        ] {
            assert!(environment(&m, &[setting.into()]).is_err(), "{setting}");
        }
        let lan = environment(&manifest("198.51.100.7"), &[]).unwrap();
        assert!(!lan.contains_key("MACHINE_SERVICE_RELAY_URL"));
    }
}
