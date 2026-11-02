//! The durable Mac's own relay agent (`docs/arch/relay-agent.md` RA-2, RA-4;
//! `docs/arch/installation.md` IN-3; DC-140, DC-1xx-DUR2-1).
//!
//! Every instance with durable exposure on has one relay agent of its own,
//! and the instance names it: the agent's label and files derive from the
//! instance's state folder, so the installed instance and any second
//! instance on the same Mac never share a label, a configuration, an
//! executable or execution records.
//!
//! - The instance on the default state folder, `~/.local/state/machine-service`,
//!   is the installed one. Its agent is [`DEFAULT_LABEL`] under the
//!   operator's home, with its LaunchAgent in `~/Library/LaunchAgents`, so
//!   launchd starts it at every login.
//! - Any other instance's agent is `com.wezzard.relay-agent.<the first 12 hex
//!   digits of the SHA-256 of the state folder>`, whose files lie under its
//!   root, `<state>/relay/agent/`, which it is told as `RELAY_AGENT_ROOT`. Its
//!   LaunchAgent lies there too, outside `~/Library/LaunchAgents`, so launchd
//!   runs it only while its instance has loaded it and never at a later
//!   login, and the state folder holds all of it.
//! - Every agent's `HOME` is the operator's home, because the commands it
//!   runs are the operator's session's (DM-1).
//!
//! [`ensure`] brings an agent to the state its instance wants. The daemon
//! calls it at startup when it runs the broker, so a second instance gets a
//! registered agent exactly as the installed one does, and the installer
//! calls it at installation.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde_json::json;
use service_layout::release::{self, Release};
use sha2::{Digest, Sha256};

/// The LaunchAgent label of the installed instance's relay agent
/// (pilot-images RA-I2).
pub const DEFAULT_LABEL: &str = crate::relay::LAUNCH_AGENT_LABEL;
/// The code-signing identifier every release build carries (DC-135).
pub const IDENTIFIER: &str = "com.wezzard.relay-agent";
/// Where the executable lives under the agent's home, as in every guest
/// (pilot-images RA-I1).
pub const EXECUTABLE: &str = crate::relay::GUEST_EXECUTABLE;
/// The relay agent's execution records under its home (DC-121).
pub const RECORDS: &str = ".local/state/relay-agent";
/// The installed instance's state folder under the operator's home.
pub const DEFAULT_STATE: &str = ".local/state/machine-service";
/// The login shell whose `PATH` the agent's commands get (DM-4).
pub const LOGIN_SHELL: &str = "/bin/zsh";

/// The root of a second instance's relay agent, under its state folder.
pub const INSTANCE_ROOT: &str = "relay/agent";

/// Which relay agent an instance has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// The LaunchAgent label.
    pub label: String,
    /// The agent's `HOME`: the operator's home.
    pub home: PathBuf,
    /// The folder every file of the agent lies under: the home for the
    /// installed instance's agent, `<state>/relay/agent` for any other.
    pub root: PathBuf,
    /// The `PATH` its commands get: the operator's login shell's, as a
    /// terminal of the desktop session has it (DC-1xx-DUR2-6); `None`
    /// leaves launchd's.
    pub path: Option<String>,
}

impl Identity {
    /// Whether this is the installed instance's agent, which launchd starts
    /// at every login.
    pub fn installed(&self) -> bool {
        self.label == DEFAULT_LABEL
    }
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize()
        .unwrap_or_else(|_| std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()))
}

/// The relay agent of the instance whose state folder is `state_dir`, on a
/// Mac whose operator's home is `home`.
pub fn identity(state_dir: &Path, home: &Path) -> Identity {
    let state = canonical(state_dir);
    if state == canonical(&home.join(DEFAULT_STATE)) {
        return Identity {
            label: DEFAULT_LABEL.to_string(),
            home: home.to_path_buf(),
            root: home.to_path_buf(),
            path: None,
        };
    }
    let digest = Sha256::digest(state.to_string_lossy().as_bytes());
    let suffix: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
    Identity {
        label: format!("{DEFAULT_LABEL}.{suffix}"),
        home: home.to_path_buf(),
        root: state.join(INSTANCE_ROOT),
        path: None,
    }
}

/// The `PATH` of the operator's login shell (`zsh -l`), read as DM-4 reads
/// the session environment; `None` when it cannot be read.
pub fn login_path(home: &Path) -> Option<String> {
    let base: Vec<(String, String)> = std::env::vars().collect();
    super::session_env::capture_login_environment(
        Path::new(LOGIN_SHELL),
        &base,
        home,
        Duration::from_secs(30),
    )
    .ok()?
    .into_iter()
    .find(|(name, _)| name == "PATH")
    .map(|(_, value)| value)
    .filter(|value| !value.is_empty())
}

impl Identity {
    /// This identity, whose commands get the operator's login `PATH`.
    pub fn with_login_path(mut self) -> Identity {
        self.path = login_path(&self.home);
        self
    }
}

/// The files of one relay agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// The LaunchAgent.
    pub plist: PathBuf,
    /// The executable.
    pub executable: PathBuf,
    /// The configuration, mode 600.
    pub config: PathBuf,
    /// The agent's log.
    pub log: PathBuf,
    /// The agent's execution records.
    pub records: PathBuf,
}

/// The files of the relay agent `identity`.
pub fn paths(identity: &Identity) -> Paths {
    let root = &identity.root;
    let label = &identity.label;
    Paths {
        plist: root.join(format!("Library/LaunchAgents/{label}.plist")),
        executable: root.join(EXECUTABLE),
        config: root.join(crate::relay::AGENT_CONFIG),
        log: root.join(format!("Library/Logs/{label}.log")),
        records: root.join(RECORDS),
    }
}

/// The LaunchAgent of the relay agent: the desktop session only, started at
/// load and kept alive (RA-2), with the operator's `HOME` and, for an agent
/// whose files lie elsewhere, `RELAY_AGENT_ROOT`.
pub fn plist(identity: &Identity, paths: &Paths) -> Vec<u8> {
    let mut environment = json!({"HOME": identity.home.to_string_lossy()});
    if let Some(path) = &identity.path {
        environment["PATH"] = json!(path);
    }
    if identity.root != identity.home {
        environment[relay_agent::config::ROOT_VARIABLE] = json!(identity.root.to_string_lossy());
    }
    crate::plist::dumps(&json!({
        "Label": identity.label,
        "ProgramArguments": [paths.executable.to_string_lossy(), "run"],
        "EnvironmentVariables": environment,
        "RunAtLoad": true,
        "KeepAlive": true,
        "LimitLoadToSessionType": "Aqua",
        "ProcessType": "Interactive",
        "StandardOutPath": paths.log.to_string_lossy(),
        "StandardErrorPath": paths.log.to_string_lossy(),
    }))
}

/// The macOS executable of the release in `release_dir`, checked against
/// its `SHA256SUMS`, holding only `relay-agent`, and signed with
/// [`IDENTIFIER`] (DC-135, DC-140).
pub fn release_executable(release_dir: &Path) -> Result<Vec<u8>, String> {
    let platform = release::platform_for("macos");
    let release = Release::open(release_dir)?;
    release.check_members(&platform)?;
    let bytes = release.executable(&platform)?;
    verify_signature(&bytes)?;
    Ok(bytes)
}

/// Refuse an executable that is not signed with [`IDENTIFIER`]: the
/// privacy grants name that identifier, and an unsigned or differently
/// signed build would lose them.
fn verify_signature(bytes: &[u8]) -> Result<(), String> {
    let directory = tempfile::tempdir().map_err(|e| e.to_string())?;
    let path = directory.path().join(release::MEMBER);
    std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| e.to_string())?;
    let mut command = Command::new("/usr/bin/codesign");
    command
        .args(["--verify", "--strict"])
        .arg(format!("-R=identifier \"{IDENTIFIER}\""))
        .arg(&path);
    let output = crate::proc::run_capture(&mut command, None, Duration::from_secs(30))
        .map_err(|e| format!("codesign: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "the release's relay agent is not signed with the identifier {IDENTIFIER} (DC-135)"
        ));
    }
    Ok(())
}

/// Whether the instance with `state_dir` holds a current durable lease,
/// whose commands a restarted relay agent would end (DC-118).
pub fn durable_lease_current(state_dir: &Path) -> bool {
    std::fs::read(state_dir.join("durable.json"))
        .ok()
        .and_then(|data| serde_json::from_slice::<serde_json::Value>(&data).ok())
        .is_some_and(|value| value["current"].as_str().is_some_and(|id| !id.is_empty()))
}

/// Write `bytes` to `path` through a temporary sibling and a rename, with
/// `mode`, in a folder made with `folder_mode` when it is given.
fn write_atomic(
    path: &Path,
    bytes: &[u8],
    mode: u32,
    folder_mode: Option<u32>,
) -> Result<(), String> {
    let at = |e: std::io::Error| format!("{}: {e}", path.display());
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no folder", path.display()))?;
    std::fs::create_dir_all(parent).map_err(at)?;
    if let Some(folder_mode) = folder_mode {
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(folder_mode))
            .map_err(at)?;
    }
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut temporary = tempfile::Builder::new()
        .prefix(&format!(".{name}."))
        .tempfile_in(parent)
        .map_err(at)?;
    temporary
        .as_file()
        .set_permissions(std::fs::Permissions::from_mode(mode))
        .map_err(at)?;
    temporary.write_all(bytes).map_err(at)?;
    temporary.as_file().sync_all().map_err(at)?;
    temporary.persist(path).map_err(|e| at(e.error))?;
    Ok(())
}

fn uid() -> u32 {
    // SAFETY: `getuid` takes no arguments and only reads process state.
    unsafe { libc::getuid() }
}

fn domain() -> String {
    format!("gui/{}", uid())
}

fn job(label: &str) -> String {
    format!("{}/{label}", domain())
}

/// Run `launchctl`, found on `PATH` as the installer finds it.
fn launchctl(arguments: &[&str]) -> Result<std::process::Output, String> {
    let mut command = Command::new("launchctl");
    command.args(arguments);
    crate::proc::run_capture(&mut command, None, Duration::from_secs(30))
        .map_err(|e| format!("launchctl {}: {e}", arguments.join(" ")))
}

/// Whether the job `label` is loaded in the desktop session's domain.
pub fn loaded(label: &str) -> Result<bool, String> {
    Ok(launchctl(&["print", &job(label)])?.status.success())
}

/// Unload the job `label` when it is loaded, and wait until it is gone.
fn unload(label: &str) -> Result<(), String> {
    if !loaded(label)? {
        return Ok(());
    }
    let _ = launchctl(&["bootout", &job(label)])?;
    for _ in 0..50 {
        if !loaded(label)? {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(format!(
        "the relay agent job {label} remains after launchctl bootout"
    ))
}

/// What one instance's relay agent needs.
#[derive(Debug, Clone)]
pub struct Spec {
    /// Which agent it is.
    pub identity: Identity,
    /// The instance's state folder, which holds the credential and the
    /// broker's authority.
    pub state_dir: PathBuf,
    /// The instance's session port, where its broker listens (BK-1).
    pub session_port: u16,
    /// The release folder to install from.
    pub release_dir: PathBuf,
    /// Make a new agent credential first (RA-4).
    pub rotate: bool,
}

impl Spec {
    /// The broker URL the agent dials: the loopback name and the session
    /// port, which neither a rename nor a network change moves (DC-163).
    pub fn broker_url(&self) -> String {
        crate::relay::durable_broker_url(self.session_port)
    }
}

/// What [`ensure`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ensured {
    /// The agent was loaded with every file as wanted.
    Unchanged,
    /// Only its credential or authority changed; the running agent reads
    /// its configuration again before its next dial (RA-3), so it was not
    /// restarted.
    Configured,
    /// The agent was (re)started, with the release's build of this SHA-256.
    Started {
        /// The SHA-256 of the executable it runs.
        build: String,
    },
}

fn differs(path: &Path, bytes: &[u8]) -> bool {
    std::fs::read(path).map_or(true, |have| have != bytes)
}

/// The broker URL of the configuration at `path`, if it has one.
fn configured_broker(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value["broker_url"].as_str().map(str::to_string)
}

/// Bring the relay agent of `spec` to the wanted state: the release's
/// executable, a configuration with the machine's credential and broker,
/// and its LaunchAgent loaded in the desktop session. Nothing is restarted
/// that already runs as wanted, and a new credential or authority alone
/// never restarts it. A moved broker does: the agent's open session keeps
/// dialling the URL it began with until its grace period ends. A restart
/// that `may_restart` forbids, because a durable
/// lease is current and a stopping agent ends the commands it runs
/// (DC-118), is refused with nothing changed but the configuration.
/// Everything is checked before anything is written.
pub fn ensure(spec: &Spec, may_restart: bool) -> Result<Ensured, String> {
    let bytes = release_executable(&spec.release_dir)?;
    let paths = paths(&spec.identity);
    let domain = domain();
    if !launchctl(&["print", &domain])?.status.success() {
        return Err(format!(
            "no desktop session ({domain}) to run the relay agent in; log in at the display first"
        ));
    }
    let credential = crate::relay::durable_agent_credential(&spec.state_dir, spec.rotate)?;
    let authority = crate::relay::authority(&spec.state_dir)?;
    let config = crate::relay::agent_config(&spec.broker_url(), &credential, authority.cert_pem());
    let plist = plist(&spec.identity, &paths);
    let moved = configured_broker(&paths.config).is_some_and(|url| url != spec.broker_url());
    let restart = differs(&paths.executable, &bytes) || differs(&paths.plist, &plist) || moved;
    let configure = differs(&paths.config, &config);
    let label = &spec.identity.label;
    let running = loaded(label)?;
    if configure {
        write_atomic(&paths.config, &config, 0o600, Some(0o700))?;
    }
    if running && !restart {
        return Ok(if configure {
            Ensured::Configured
        } else {
            Ensured::Unchanged
        });
    }
    if running && !may_restart {
        return Err(format!(
            "a durable lease is current, and restarting its relay agent {label} would end its commands; it is left as it is until the lease ends"
        ));
    }
    // The old agent stops first, so no executable is replaced under it.
    unload(label)?;
    write_atomic(&paths.executable, &bytes, 0o755, None)?;
    if let Some(parent) = paths.log.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    write_atomic(&paths.plist, &plist, 0o644, None)?;
    let plist_path = paths.plist.to_string_lossy().to_string();
    let mut last = String::new();
    for _ in 0..20 {
        let output = launchctl(&["bootstrap", &domain, &plist_path])?;
        if output.status.success() {
            return Ok(Ensured::Started {
                build: release::sha256_hex(&bytes),
            });
        }
        last = String::from_utf8_lossy(&output.stderr).trim().to_string();
        std::thread::sleep(Duration::from_millis(250));
    }
    Err(format!(
        "launchctl bootstrap {domain} {plist_path} failed: {last}"
    ))
}

fn remove_file(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

fn remove_tree(path: &Path) -> Result<(), String> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

/// Unload the relay agent `identity` and remove its LaunchAgent,
/// executable, configuration and records; a second instance's agent root
/// goes whole. Returns whether there was a LaunchAgent. Nothing to remove
/// is not a failure.
pub fn remove(identity: &Identity) -> Result<bool, String> {
    let paths = paths(identity);
    let present = paths.plist.exists();
    unload(&identity.label)?;
    if identity.installed() {
        for file in [&paths.plist, &paths.executable, &paths.config] {
            remove_file(file)?;
        }
        remove_tree(&paths.records)?;
    } else {
        remove_tree(&identity.root)?;
    }
    Ok(present)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_installed_instance_keeps_the_default_label_and_the_operators_home() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path().canonicalize().unwrap();
        let installed = identity(&home.join(DEFAULT_STATE), &home);
        assert_eq!(installed.label, DEFAULT_LABEL);
        assert_eq!(installed.home, home);
        assert_eq!(installed.root, home);
        assert!(installed.installed());
        let paths = paths(&installed);
        assert_eq!(
            paths.plist,
            home.join("Library/LaunchAgents/com.wezzard.relay-agent.plist")
        );
        assert_eq!(paths.config, home.join(".config/relay-agent/config.json"));
    }

    #[test]
    fn a_second_instance_derives_its_own_agent_from_its_state_folder() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path().canonicalize().unwrap();
        let state = home.join("acceptance/state");
        let second = identity(&state, &home);
        assert!(second.label.starts_with("com.wezzard.relay-agent."));
        assert_eq!(second.label.len(), DEFAULT_LABEL.len() + 13);
        assert_eq!(second.root, state.join(INSTANCE_ROOT));
        assert_eq!(second.home, home, "its commands keep the operator's HOME");
        assert!(!second.installed());
        // The same folder always names the same agent, another folder another.
        assert_eq!(identity(&state, &home), second);
        assert_ne!(identity(&home.join("other"), &home).label, second.label);
        // None of its files is the installed agent's.
        let ours = paths(&second);
        let theirs = paths(&identity(&home.join(DEFAULT_STATE), &home));
        for (a, b) in [
            (&ours.plist, &theirs.plist),
            (&ours.executable, &theirs.executable),
            (&ours.config, &theirs.config),
            (&ours.records, &theirs.records),
        ] {
            assert_ne!(a, b);
            assert!(a.starts_with(&state), "{}", a.display());
        }
    }

    #[test]
    fn the_launch_agent_runs_in_the_desktop_session_and_stays_alive() {
        let id = Identity {
            label: "com.example.agent".into(),
            home: PathBuf::from("/home/test"),
            root: PathBuf::from("/home/test"),
            path: None,
        };
        let paths = paths(&id);
        let value = crate::plist::parse(&plist(&id, &paths)).unwrap();
        assert_eq!(value["Label"], "com.example.agent");
        assert_eq!(value["LimitLoadToSessionType"], "Aqua");
        assert_eq!(value["RunAtLoad"], true);
        assert_eq!(value["KeepAlive"], true);
        assert_eq!(
            value["ProgramArguments"],
            json!(["/home/test/.local/libexec/relay-agent/relay-agent", "run"])
        );
        assert_eq!(value["EnvironmentVariables"]["HOME"], "/home/test");
        assert!(value["EnvironmentVariables"]
            .get("RELAY_AGENT_ROOT")
            .is_none());
        // A second instance's agent keeps the operator's HOME and is told
        // where its own files are.
        assert!(value["EnvironmentVariables"].get("PATH").is_none());
        let second = Identity {
            root: PathBuf::from("/state/relay/agent"),
            path: Some("/home/test/.local/bin:/usr/bin:/bin".into()),
            ..id
        };
        let paths = super::paths(&second);
        let value = crate::plist::parse(&plist(&second, &paths)).unwrap();
        assert_eq!(value["EnvironmentVariables"]["HOME"], "/home/test");
        assert_eq!(
            value["EnvironmentVariables"]["RELAY_AGENT_ROOT"],
            "/state/relay/agent"
        );
        assert_eq!(
            value["EnvironmentVariables"]["PATH"],
            "/home/test/.local/bin:/usr/bin:/bin"
        );
        assert_eq!(
            value["ProgramArguments"][0],
            "/state/relay/agent/.local/libexec/relay-agent/relay-agent"
        );
    }

    #[test]
    fn a_durable_lease_is_read_from_the_state() {
        let state = tempfile::tempdir().unwrap();
        assert!(!durable_lease_current(state.path()));
        std::fs::write(state.path().join("durable.json"), r#"{"current": null}"#).unwrap();
        assert!(!durable_lease_current(state.path()));
        std::fs::write(state.path().join("durable.json"), r#"{"current": "abc"}"#).unwrap();
        assert!(durable_lease_current(state.path()));
    }
}
