//! The durable Mac's own relay agent (`docs/arch/relay-agent.md` RA-2,
//! RA-4; `docs/arch/installation.md` IN-3; DC-140).
//!
//! On a Mac whose instance exposes it as a durable machine, the installer
//! installs the relay agent from the release beside the installation, as a
//! LaunchAgent of the operator's account in the desktop session
//! (`LimitLoadToSessionType` `Aqua`, `RunAtLoad`, `KeepAlive`), with the
//! machine's agent credential and a configuration that points at the Mac's
//! own broker. Every path derives from the home folder and the label, so a
//! second instance's test agent (TL-6) is the same installation under
//! another `HOME` and label.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde_json::json;
use service_layout::release::{self, Release};

use crate::installer::{run_command, InstallError};

/// The LaunchAgent label of the installed relay agent (pilot-images RA-I2).
pub const DEFAULT_LABEL: &str = machine_service_core::relay::LAUNCH_AGENT_LABEL;
/// The code-signing identifier every release build carries (DC-135).
pub const IDENTIFIER: &str = "com.wezzard.relay-agent";
/// Where the executable lives under the home folder, as in every guest
/// (pilot-images RA-I1).
pub const EXECUTABLE: &str = machine_service_core::relay::GUEST_EXECUTABLE;
/// The relay agent's execution records under the home folder (DC-121).
pub const RECORDS: &str = ".local/state/relay-agent";

/// The files of one relay agent installation.
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

/// The files of the relay agent labelled `label` under `home`.
pub fn paths(home: &Path, label: &str) -> Paths {
    Paths {
        plist: home.join(format!("Library/LaunchAgents/{label}.plist")),
        executable: home.join(EXECUTABLE),
        config: home.join(machine_service_core::relay::AGENT_CONFIG),
        log: home.join(format!("Library/Logs/{label}.log")),
        records: home.join(RECORDS),
    }
}

/// What one installation of the relay agent needs.
#[derive(Debug, Clone)]
pub struct Spec {
    /// The agent's home folder.
    pub home: PathBuf,
    /// The LaunchAgent label.
    pub label: String,
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

/// The macOS executable of the release in `release_dir`, checked against
/// its `SHA256SUMS`, holding only `relay-agent`, and signed with
/// [`IDENTIFIER`] (DC-135, DC-140).
pub fn release_executable(release_dir: &Path) -> Result<Vec<u8>, InstallError> {
    let platform = release::platform_for("macos");
    let release = Release::open(release_dir).map_err(InstallError::Refused)?;
    release
        .check_members(&platform)
        .map_err(InstallError::Refused)?;
    let bytes = release
        .executable(&platform)
        .map_err(InstallError::Refused)?;
    verify_signature(&bytes)?;
    Ok(bytes)
}

/// Refuse an executable that is not signed with [`IDENTIFIER`]: the
/// privacy grants name that identifier, and an unsigned or differently
/// signed build would lose them.
#[cfg(target_os = "macos")]
fn verify_signature(bytes: &[u8]) -> Result<(), InstallError> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join(release::MEMBER);
    std::fs::write(&path, bytes)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    let mut command = Command::new("/usr/bin/codesign");
    command
        .args(["--verify", "--strict"])
        .arg(format!("-R=identifier \"{IDENTIFIER}\""))
        .arg(&path);
    let output = run_command(&mut command, Duration::from_secs(30))?;
    if !output.status.success() {
        return Err(InstallError::refused(format!(
            "the release's relay agent is not signed with the identifier {IDENTIFIER} (DC-135)"
        )));
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn verify_signature(_bytes: &[u8]) -> Result<(), InstallError> {
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

/// The LaunchAgent of the relay agent: the desktop session only, started at
/// load and kept alive (RA-2).
pub fn plist(label: &str, home: &Path, paths: &Paths) -> Vec<u8> {
    crate::plist::dumps(&json!({
        "Label": label,
        "ProgramArguments": [paths.executable.to_string_lossy(), "run"],
        "EnvironmentVariables": {"HOME": home.to_string_lossy()},
        "RunAtLoad": true,
        "KeepAlive": true,
        "LimitLoadToSessionType": "Aqua",
        "ProcessType": "Interactive",
        "StandardOutPath": paths.log.to_string_lossy(),
        "StandardErrorPath": paths.log.to_string_lossy(),
    }))
}

/// Write `bytes` to `path` through a temporary sibling and a rename, with
/// `mode`, in a folder made with `folder_mode` when it is missing.
fn write_atomic(
    path: &Path,
    bytes: &[u8],
    mode: u32,
    folder_mode: Option<u32>,
) -> Result<(), InstallError> {
    let parent = path
        .parent()
        .ok_or_else(|| InstallError::refused(format!("{} has no folder", path.display())))?;
    std::fs::create_dir_all(parent)?;
    if let Some(folder_mode) = folder_mode {
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(folder_mode))?;
    }
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut temporary = tempfile::Builder::new()
        .prefix(&format!(".{name}."))
        .tempfile_in(parent)?;
    temporary
        .as_file()
        .set_permissions(std::fs::Permissions::from_mode(mode))?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| InstallError::Io(error.error))?;
    Ok(())
}

fn uid() -> u32 {
    // SAFETY: `getuid` takes no arguments and only reads process state.
    unsafe { libc::getuid() }
}

fn job(label: &str) -> String {
    format!("gui/{}/{label}", uid())
}

fn launchctl(arguments: &[&str]) -> Result<crate::installer::CommandOutput, InstallError> {
    let mut command = Command::new("launchctl");
    command.args(arguments);
    run_command(&mut command, Duration::from_secs(30))
}

/// Whether the job `label` is loaded in the desktop session's domain.
fn loaded(label: &str) -> Result<bool, InstallError> {
    Ok(launchctl(&["print", &job(label)])?.status.success())
}

/// Unload the job `label` when it is loaded, and wait until it is gone.
fn unload(label: &str) -> Result<(), InstallError> {
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
    Err(InstallError::refused(format!(
        "the relay agent job {label} remains after launchctl bootout"
    )))
}

/// Install the relay agent of `spec` and (re)start it in the desktop
/// session. Everything is checked before anything is written.
pub fn install(spec: &Spec) -> Result<(), InstallError> {
    let bytes = release_executable(&spec.release_dir)?;
    let paths = paths(&spec.home, &spec.label);
    let domain = format!("gui/{}", uid());
    if !launchctl(&["print", &domain])?.status.success() {
        return Err(InstallError::refused(format!(
            "no desktop session ({domain}) to run the relay agent in; log in at the display first"
        )));
    }
    let credential =
        machine_service_core::relay::durable_agent_credential(&spec.state_dir, spec.rotate)
            .map_err(InstallError::Refused)?;
    let authority =
        machine_service_core::relay::authority(&spec.state_dir).map_err(InstallError::Refused)?;
    let config = machine_service_core::relay::agent_config(
        &machine_service_core::relay::durable_broker_url(spec.session_port),
        &credential,
        authority.cert_pem(),
    );
    // The old agent stops first, so no executable is replaced under it.
    unload(&spec.label)?;
    write_atomic(&paths.executable, &bytes, 0o755, None)?;
    write_atomic(&paths.config, &config, 0o600, Some(0o700))?;
    if let Some(parent) = paths.log.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_atomic(
        &paths.plist,
        &plist(&spec.label, &spec.home, &paths),
        0o644,
        None,
    )?;
    let plist_path = paths.plist.to_string_lossy().to_string();
    let mut last = String::new();
    for _ in 0..20 {
        let output = launchctl(&["bootstrap", &domain, &plist_path])?;
        if output.status.success() {
            println!(
                "loaded relay agent {} ({}; broker {}; release sha256 {})",
                spec.label,
                paths.executable.display(),
                machine_service_core::relay::durable_broker_url(spec.session_port),
                release::sha256_hex(&bytes)
            );
            return Ok(());
        }
        last = String::from_utf8_lossy(&output.stderr).trim().to_string();
        std::thread::sleep(Duration::from_millis(250));
    }
    Err(InstallError::refused(format!(
        "launchctl bootstrap {domain} {plist_path} failed: {last}"
    )))
}

/// Unload the relay agent `label` under `home` and remove its LaunchAgent,
/// executable, configuration and records. Nothing to remove is not a
/// failure.
pub fn remove(home: &Path, label: &str) -> Result<bool, InstallError> {
    let paths = paths(home, label);
    let present = paths.plist.exists();
    unload(label)?;
    for file in [&paths.plist, &paths.executable, &paths.config] {
        match std::fs::remove_file(file) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    match std::fs::remove_dir_all(&paths.records) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    if present {
        println!("removed relay agent {label} ({})", paths.plist.display());
    }
    Ok(present)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_launch_agent_runs_in_the_desktop_session_and_stays_alive() {
        let home = Path::new("/home/test");
        let paths = paths(home, "com.example.agent");
        let value = crate::plist::parse(&plist("com.example.agent", home, &paths)).unwrap();
        assert_eq!(value["Label"], "com.example.agent");
        assert_eq!(value["LimitLoadToSessionType"], "Aqua");
        assert_eq!(value["RunAtLoad"], true);
        assert_eq!(value["KeepAlive"], true);
        assert_eq!(
            value["ProgramArguments"],
            json!(["/home/test/.local/libexec/relay-agent/relay-agent", "run"])
        );
        assert_eq!(value["EnvironmentVariables"]["HOME"], "/home/test");
        assert_eq!(
            paths.plist,
            Path::new("/home/test/Library/LaunchAgents/com.example.agent.plist")
        );
        assert_eq!(
            paths.config,
            Path::new("/home/test/.config/relay-agent/config.json")
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
