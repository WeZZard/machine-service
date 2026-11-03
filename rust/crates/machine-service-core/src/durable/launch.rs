//! A durable lease's command through the relay agent (DM-4, RA-2, RA-8),
//! whatever its entry: machine-service's own HTTP exec, or a relay client's
//! exec or script that the broker hands to this launcher (DC-219).
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
//! daemon's own executable: it reads the request, records its process
//! group with the leader's start time, and only then claims the request by
//! removing it. A release withdraws every request first and reads the
//! recorded groups after, so a command either is recorded before release
//! stops the lease's groups or never starts (DC-220). The launcher
//! then replaces itself with the lease's shell, so the command keeps the
//! relay agent as its parent and the agent's responsibility for privacy
//! permissions, and the agent sees its exit status and output as its own
//! command's. When the lease has loaded agent keys, it runs the shell as its
//! child instead and passes the output on with every key redacted, so no
//! relay message and no record of the relay agent carries one
//! (DC-221). The keys reach the command through its environment
//! only, never through the relay protocol or a process's arguments (AK-5).

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::agents::Redactor;
use super::exec::Program;
use super::process::{self, LeaseSession};
use super::session_env::SessionEnv;

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
    /// `$SHELL -c <line>`; or `zsh -s` with `script` as standard input, or
    /// with the launcher's own standard input when neither is set.
    shell: String,
    line: Option<String>,
    script: Option<PathBuf>,
    env: BTreeMap<String, String>,
    cwd: PathBuf,
    /// Where the launcher records its process group.
    group: PathBuf,
    /// The values the command's output never carries (AK-5).
    #[serde(default)]
    redact: Vec<String>,
}

/// What the launcher runs.
#[derive(Debug, Clone, Copy)]
pub enum Launch<'a> {
    /// A command line or a script, as machine-service's own exec has it.
    Program(&'a Program),
    /// A script that arrives on the launcher's standard input, as the relay
    /// agent feeds a client's script to the command it runs.
    ScriptOnInput,
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

/// Write the request for `launch` in the lease's `session`, and answer the
/// command line the relay agent runs: the launcher of `launcher` on that
/// request. The line names only the two paths.
pub fn prepare(
    files: &Files,
    launcher: &Path,
    launch: Launch<'_>,
    session: &SessionEnv,
    cwd: &Path,
) -> Result<String, String> {
    // The launcher runs the shell the service resolved here, through its
    // tool table (DC-254).
    let zsh = || {
        service_layout::tools::path(service_layout::tools::Tool::Zsh)
            .map(|path| path.to_string_lossy().into_owned())
    };
    let (shell, line, script) = match launch {
        Launch::Program(Program::Line { shell, line }) => (
            service_layout::tools::program(shell)?
                .to_string_lossy()
                .into_owned(),
            Some(line.clone()),
            None,
        ),
        Launch::Program(Program::Script(bytes)) => {
            private_file(&files.script, bytes)?;
            (zsh()?, None, Some(files.script.clone()))
        }
        Launch::ScriptOnInput => (zsh()?, None, None),
    };
    let request = Request {
        shell,
        line,
        script,
        env: session.vars.clone(),
        cwd: cwd.to_path_buf(),
        group: files.group.clone(),
        redact: session
            .secrets
            .iter()
            .map(|secret| secret.expose().to_string())
            .collect(),
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

/// Remove what one execution's launcher has not taken: its request and its
/// script, never its process group's record, which release still needs.
pub fn withdraw(files: &Files) {
    let _ = std::fs::remove_file(&files.request);
    let _ = std::fs::remove_file(&files.script);
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
    let request: Request = serde_json::from_slice(&text)
        .map_err(|error| format!("{} is not a lease command: {error}", path.display()))?;
    // The script is opened before the claim, so a release that withdraws it
    // after the claim cannot take it away from a command that may run.
    let script = request
        .script
        .as_ref()
        .map(|script| {
            std::fs::File::open(script)
                .map_err(|error| format!("cannot open {}: {error}", script.display()))
        })
        .transpose()?;
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
    // The claim: the request holds the session's keys and is removed before
    // anything runs. A release that withdrew it first has the command start
    // nothing (DC-220).
    if let Err(error) = std::fs::remove_file(path) {
        let _ = std::fs::remove_file(&request.group);
        return Err(format!(
            "the lease command at {} was withdrawn: {error}",
            path.display()
        ));
    }
    if let Some(script) = &request.script {
        let _ = std::fs::remove_file(script);
    }
    let mut command = Command::new(&request.shell);
    match (&request.line, script) {
        (Some(line), _) => {
            command.arg("-c").arg(line);
        }
        (None, Some(file)) => {
            command.arg("-s").stdin(file);
        }
        (None, None) => {
            command.arg("-s");
        }
    }
    command
        .env_clear()
        .envs(&request.env)
        .current_dir(&request.cwd);
    let redactor = Redactor::new(&request.redact);
    if redactor.is_empty() {
        let error = command.exec();
        return Err(format!("cannot start {}: {error}", request.shell));
    }
    supervise(command, redactor)
}

/// Run `command` as the launcher's child, in the launcher's process group,
/// with its standard output and standard error on one pipe, and pass the
/// output on redacted; then end as the command ended, with its exit status
/// or its signal, so the relay agent sees what it would have seen of the
/// command itself.
fn supervise(mut command: Command, mut redactor: Redactor) -> Result<Never, String> {
    let shell = command.get_program().to_string_lossy().to_string();
    let (mut output, writer) = std::io::pipe().map_err(|error| error.to_string())?;
    command
        .stdout(writer.try_clone().map_err(|error| error.to_string())?)
        .stderr(writer);
    let spawned = command.spawn();
    // The parent's copies of the pipe's write end go, so the read ends when
    // the command's group closes it.
    drop(command);
    let mut child = spawned.map_err(|error| format!("cannot start {shell}: {error}"))?;
    let mut out = std::io::stdout().lock();
    let mut open = true;
    let mut pass = |out: &mut std::io::StdoutLock<'_>, bytes: &[u8]| {
        // A reader that went away takes nothing more; the command is still
        // drained, so it never blocks on a full pipe.
        if open && !bytes.is_empty() {
            open = out.write_all(bytes).and_then(|_| out.flush()).is_ok();
        }
    };
    let mut buffer = vec![0u8; 64 << 10];
    loop {
        match std::io::Read::read(&mut output, &mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                let redacted = redactor.push(&buffer[..count]);
                pass(&mut out, &redacted);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    pass(&mut out, &redactor.finish());
    drop(out);
    let status = child
        .wait()
        .map_err(|error| format!("cannot wait for {shell}: {error}"))?;
    use std::os::unix::process::ExitStatusExt;
    if let Some(signal) = status.signal() {
        // SAFETY: resetting a signal's disposition and signalling this
        // process have no preconditions; the launcher has nothing to clean.
        unsafe {
            libc::signal(signal, libc::SIG_DFL);
            libc::kill(libc::getpid(), signal);
        }
        std::process::exit(128 + signal);
    }
    std::process::exit(status.code().unwrap_or(FAILED));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> SessionEnv {
        SessionEnv {
            vars: BTreeMap::from([("KEY".to_string(), "secret".to_string())]),
            secrets: vec![super::super::agents::Secret::new("secret")],
        }
    }

    #[test]
    fn a_request_is_private_and_names_the_launcher() {
        let lease = tempfile::tempdir().unwrap();
        let files = files(lease.path(), "exec-1");
        let program = Program::Line {
            shell: "/bin/zsh".into(),
            line: "echo hi".into(),
        };
        let line = prepare(
            &files,
            Path::new("/opt/ms/machine-service"),
            Launch::Program(&program),
            &session(),
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
        let request: Request =
            serde_json::from_slice(&std::fs::read(&files.request).unwrap()).unwrap();
        assert_eq!(request.redact, vec!["secret".to_string()]);
        withdraw_requests(lease.path());
        assert!(!files.request.exists());
    }

    /// DC-219: a client's script names no file; the launcher's
    /// shell reads it on standard input. One execution's leftovers are
    /// withdrawn alone, and its group's record stays for release.
    #[test]
    fn a_script_on_input_and_one_executions_withdrawal() {
        let lease = tempfile::tempdir().unwrap();
        let one = files(lease.path(), "exec-1");
        let two = files(lease.path(), "exec-2");
        for files in [&one, &two] {
            prepare(
                files,
                Path::new("/opt/ms/machine-service"),
                Launch::ScriptOnInput,
                &session(),
                lease.path(),
            )
            .unwrap();
        }
        let request: Request =
            serde_json::from_slice(&std::fs::read(&one.request).unwrap()).unwrap();
        assert_eq!((request.line, request.script), (None, None));
        std::fs::write(&one.group, b"{}").unwrap();
        withdraw(&one);
        assert!(!one.request.exists());
        assert!(one.group.exists());
        assert!(two.request.exists());
    }

    #[test]
    fn a_missing_request_starts_nothing() {
        let lease = tempfile::tempdir().unwrap();
        assert_eq!(run(&lease.path().join("gone.request.json")), FAILED);
        assert!(recorded_groups(lease.path()).is_empty());
    }
}
