//! A durable lease's command through the relay agent (DM-4, RA-2, RA-8).
//!
//! The relay agent runs a command line in the desktop session as its own
//! child, in a new process group, with the agent's environment and home.
//! A lease's command needs more: the session environment of DM-4, which
//! holds the loaded agent keys, the lease's working directory, and a record
//! of its process group before it can start anything, so release can stop
//! it and the sweep can find what it left (DM-7, HY-3).
//!
//! machine-service therefore writes a request into the lease's state folder
//! (mode 600 in a mode-700 folder) and asks the relay agent to run
//! `exec <machine-service> --durable-launch <request>`. The launcher is the
//! daemon's own executable: it reads and removes the request, records its
//! process group with the leader's start time, and replaces itself with the
//! lease's shell, so the command keeps the relay agent as its parent and the
//! agent's responsibility for privacy permissions, and the agent sees its
//! exit status and output as its own command's. The keys reach the command
//! through its environment only, never through the relay protocol, the
//! agent's records or a process's arguments (AK-5).

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::exec::Program;
use super::process::{self, LeaseSession};

/// The daemon's argument that runs the launcher.
pub const ARGUMENT: &str = "--durable-launch";
/// The lease state's folder of relay requests and process-group records.
pub const FOLDER: &str = "relay";
/// The launcher's exit status when it cannot start the command; a shell
/// uses 126 and 127 for its own failures.
pub const FAILED: i32 = 125;

/// One command for the launcher.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Request {
    /// `$SHELL -c <line>`, or `zsh -s` with `script` as standard input.
    shell: String,
    line: Option<String>,
    script: Option<PathBuf>,
    env: BTreeMap<String, String>,
    cwd: PathBuf,
    /// Where the launcher records its process group.
    group: PathBuf,
}

/// The files of one relay execution in a lease's state folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Files {
    /// The request the launcher reads and removes.
    pub request: PathBuf,
    /// The script, when the command is one.
    pub script: PathBuf,
    /// The launcher's record of its process group.
    pub group: PathBuf,
}

/// The files of `execution` under `lease_dir`. The execution ID is hashed,
/// so any ID makes a safe file name.
pub fn files(lease_dir: &Path, execution: &str) -> Files {
    let key = service_layout::release::sha256_hex(execution.as_bytes());
    let folder = lease_dir.join(FOLDER);
    Files {
        request: folder.join(format!("{key}.request.json")),
        script: folder.join(format!("{key}.script")),
        group: folder.join(format!("{key}.group.json")),
    }
}

fn private_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let folder = path.parent().unwrap_or(Path::new("."));
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(folder)
        .map_err(|error| format!("cannot create {}: {error}", folder.display()))?;
    let temporary = folder.join(format!(
        ".{}.{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .and_then(|mut file| file.write_all(bytes).and_then(|_| file.sync_all()))
        .and_then(|_| std::fs::rename(&temporary, path));
    written.map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        format!("cannot write {}: {error}", path.display())
    })
}

/// Write the request for `program`, and answer the command line the relay
/// agent runs: the launcher of `launcher` on that request.
pub fn prepare(
    files: &Files,
    launcher: &Path,
    program: &Program,
    env: &BTreeMap<String, String>,
    cwd: &Path,
) -> Result<String, String> {
    let (shell, line, script) = match program {
        Program::Line { shell, line } => (shell.clone(), Some(line.clone()), None),
        Program::Script(bytes) => {
            private_file(&files.script, bytes)?;
            ("/bin/zsh".to_string(), None, Some(files.script.clone()))
        }
    };
    let request = Request {
        shell,
        line,
        script,
        env: env.clone(),
        cwd: cwd.to_path_buf(),
        group: files.group.clone(),
    };
    let bytes = serde_json::to_vec(&request).map_err(|error| error.to_string())?;
    private_file(&files.request, &bytes)?;
    Ok(format!(
        "exec {} {ARGUMENT} {}",
        crate::ssh::shell_quote(&launcher.to_string_lossy()),
        crate::ssh::shell_quote(&files.request.to_string_lossy())
    ))
}

/// The process group a launcher recorded, once it has.
pub fn recorded_group(path: &Path) -> Option<LeaseSession> {
    let text = std::fs::read_to_string(path).ok()?;
    LeaseSession::from_json(&serde_json::from_str::<Value>(&text).ok()?)
}

/// Every process group recorded under `lease_dir`.
pub fn recorded_groups(lease_dir: &Path) -> Vec<LeaseSession> {
    let Ok(entries) = std::fs::read_dir(lease_dir.join(FOLDER)) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with(".group.json"))
        })
        .filter_map(|path| recorded_group(&path))
        .collect()
}

/// Remove the requests no launcher has taken yet, so a command the relay
/// agent starts after a release finds nothing to run (DM-7).
pub fn withdraw_requests(lease_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(lease_dir.join(FOLDER)) else {
        return;
    };
    for path in entries.flatten().map(|entry| entry.path()) {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if name.ends_with(".request.json") || name.ends_with(".script") {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// The launcher: run the request at `path`. It returns only when the
/// command could not start, with the status to exit with.
pub fn run(path: &Path) -> i32 {
    match launch(path) {
        Ok(never) => match never {},
        Err(message) => {
            eprintln!("machine-service: {message}");
            FAILED
        }
    }
}

enum Never {}

fn launch(path: &Path) -> Result<Never, String> {
    let text = std::fs::read(path)
        .map_err(|error| format!("no lease command at {}: {error}", path.display()))?;
    // The request holds the session's keys: it is read once and removed
    // before anything runs.
    let _ = std::fs::remove_file(path);
    let request: Request = serde_json::from_slice(&text)
        .map_err(|error| format!("{} is not a lease command: {error}", path.display()))?;
    // SAFETY: `getpgrp` has no preconditions.
    let group = unsafe { libc::getpgrp() };
    let leader =
        process::process_info(group).ok_or_else(|| format!("cannot read process group {group}"))?;
    let record = LeaseSession {
        sid: group,
        leader_start: leader.start,
        group: true,
    };
    let bytes = serde_json::to_vec(&record.to_json()).map_err(|error| error.to_string())?;
    private_file(&request.group, &bytes)?;
    let mut command = match (&request.line, &request.script) {
        (Some(line), _) => {
            let mut command = Command::new(&request.shell);
            command.arg("-c").arg(line);
            command
        }
        (None, Some(script)) => {
            let file = std::fs::File::open(script)
                .map_err(|error| format!("cannot open {}: {error}", script.display()))?;
            let _ = std::fs::remove_file(script);
            let mut command = Command::new(&request.shell);
            command.arg("-s").stdin(file);
            command
        }
        (None, None) => return Err("the lease command has neither a line nor a script".into()),
    };
    command
        .env_clear()
        .envs(&request.env)
        .current_dir(&request.cwd);
    let error = command.exec();
    Err(format!("cannot start {}: {error}", request.shell))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_is_private_and_names_the_launcher() {
        let lease = tempfile::tempdir().unwrap();
        let files = files(lease.path(), "exec-1");
        let env = BTreeMap::from([("KEY".to_string(), "secret".to_string())]);
        let line = prepare(
            &files,
            Path::new("/opt/ms/machine-service"),
            &Program::Line {
                shell: "/bin/zsh".into(),
                line: "echo hi".into(),
            },
            &env,
            lease.path(),
        )
        .unwrap();
        assert!(
            line.starts_with("exec /opt/ms/machine-service --durable-launch "),
            "{line}"
        );
        assert!(
            !line.contains("secret"),
            "the keys are never in the command line"
        );
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&files.request), 0o600);
        assert_eq!(mode(files.request.parent().unwrap()), 0o700);
        assert_ne!(files.request, super::files(lease.path(), "exec-2").request);
        withdraw_requests(lease.path());
        assert!(!files.request.exists());
    }

    #[test]
    fn a_missing_request_starts_nothing() {
        let lease = tempfile::tempdir().unwrap();
        assert_eq!(run(&lease.path().join("gone.request.json")), FAILED);
        assert!(recorded_groups(lease.path()).is_empty());
    }
}
