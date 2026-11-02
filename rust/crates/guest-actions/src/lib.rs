//! The actions a guest-side agent carries out for the host: a command line
//! run through the user's shell in its own process group, an atomic file
//! write checked by SHA-256, and a file read.
//!
//! The rules are those of the guest session agent
//! (`docs/arch/guest-session.md`, GS-3), which the relay agent keeps
//! (`docs/arch/relay-agent.md`, RA-5 and RA-9). Both agents share this crate,
//! so the two cannot drift apart.

use std::fs::File;
use std::io::{self, PipeReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use sha2::{Digest, Sha256};

pub mod ownership;

#[cfg(test)]
mod tests;

/// Lower-case hexadecimal.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The SHA-256 of `bytes`, in lower-case hexadecimal.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// The SHA-256 of the running executable, which an agent announces in its
/// hello so the host can tell a stale build; empty when it cannot be read.
pub fn executable_sha256() -> String {
    std::env::current_exe()
        .and_then(std::fs::read)
        .map(|bytes| sha256_hex(&bytes))
        .unwrap_or_default()
}

/// The user's shell, `$SHELL`, else `/bin/sh`.
pub fn user_shell() -> PathBuf {
    std::env::var_os("SHELL")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/bin/sh"))
}

/// The user's home directory, `$HOME`, else `/`.
pub fn user_home() -> PathBuf {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// The command line that runs a script given on standard input: `zsh -s`
/// on macOS and `bash -s` elsewhere, as machine-service has always sent it.
pub fn script_command() -> &'static str {
    if cfg!(target_os = "macos") {
        "zsh -s"
    } else {
        "bash -s"
    }
}

/// Resolve a guest path the way `scp` resolves a remote path: `~` and
/// relative paths are taken from the home directory.
pub fn resolve(path: &str, home: &Path) -> PathBuf {
    if path == "~" {
        home.to_path_buf()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home.join(rest)
    } else {
        home.join(path)
    }
}

/// Kill a whole process group.
pub fn kill_group(group: i32) {
    if group > 0 {
        // SAFETY: `killpg` takes plain integers and has no memory effects.
        unsafe {
            libc::killpg(group, libc::SIGKILL);
        }
    }
}

/// Kill a command's processes: its process group and, when it runs in
/// one, its scope, which also holds the processes that left the group.
pub fn kill_command(group: i32, scope: Option<&str>) {
    kill_group(group);
    if let Some(scope) = scope {
        let _ = ownership::kill_scope(scope);
    }
}

/// A failed action: a stable code for programs and a message for people.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionError {
    /// A stable, machine-readable code such as `not_found`.
    pub code: &'static str,
    /// What went wrong, for people.
    pub message: String,
}

impl ActionError {
    /// An error with `code` and `message`.
    pub fn new(code: &'static str, message: impl Into<String>) -> ActionError {
        ActionError {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ActionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ActionError {}

/// The size and SHA-256 of a byte stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Digest256 {
    /// The number of bytes.
    pub size: u64,
    /// Their SHA-256, in lower-case hexadecimal.
    pub sha256: String,
}

/// A running SHA-256 and byte count.
#[derive(Clone, Default)]
pub struct Tally {
    hasher: Sha256,
    size: u64,
}

impl Tally {
    /// Count `bytes`.
    pub fn update(&mut self, bytes: &[u8]) {
        self.hasher.update(bytes);
        self.size += bytes.len() as u64;
    }

    /// The bytes counted so far.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// The size and SHA-256 of everything counted.
    pub fn finish(self) -> Digest256 {
        Digest256 {
            size: self.size,
            sha256: hex(&self.hasher.finalize()),
        }
    }
}

// ------------------------------------------------------------------ commands

/// A command line started in its own process group, with its standard
/// output and standard error combined on one pipe in the order written.
pub struct Spawned {
    child: Child,
    output: PipeReader,
    /// The command's process group, which is also the shell's process ID.
    pub group: i32,
    /// The systemd scope the command runs in, when it runs in one.
    pub scope: Option<String>,
}

/// How a command ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Exit {
    /// The exit status, when the command exited.
    pub rc: Option<i32>,
    /// The signal that ended it, when one did.
    pub signal: Option<i32>,
    /// Whether the timeout killed it.
    pub timed_out: bool,
}

/// Start `command` through `shell -c`, the way `sshd` runs a remote command,
/// in `home` and in a new process group. Like `sshd`, it names the shell by
/// its file name, so the shell's messages read `bash: …`. The command's
/// standard input is a pipe when `stdin` is true, else `/dev/null`.
pub fn spawn_shell(shell: &Path, home: &Path, command: &str, stdin: bool) -> io::Result<Spawned> {
    spawn_shell_in(shell, home, command, stdin, None)
}

/// [`spawn_shell`], in the systemd user scope `scope` when one is named
/// (DC-139): `systemd-run --user --scope` registers the scope and then
/// executes the shell in place, so everything the command starts, a daemon
/// that leaves the process group included, stays in the scope.
pub fn spawn_shell_in(
    shell: &Path,
    home: &Path,
    command: &str,
    stdin: bool,
    scope: Option<&str>,
) -> io::Result<Spawned> {
    let (output, writer) = io::pipe()?;
    let mut process = match scope {
        Some(scope) => ownership::scope_command(scope, shell),
        None => {
            let mut process = Command::new(shell);
            if let Some(name) = shell.file_name() {
                process.arg0(name);
            }
            process
        }
    };
    process
        .arg("-c")
        .arg(command)
        .current_dir(home)
        .process_group(0)
        .stdin(if stdin { Stdio::piped() } else { Stdio::null() })
        .stdout(writer.try_clone()?)
        .stderr(writer);
    let spawned = process.spawn();
    // The command holds the parent's copies of the pipe's write end; drop
    // them so the reader sees the end of output when the command's group
    // exits.
    drop(process);
    let child = spawned?;
    Ok(Spawned {
        group: child.id() as i32,
        scope: scope.map(str::to_string),
        child,
        output,
    })
}

impl Spawned {
    /// The process ID of the command's shell, which leads its group.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Feed `stdin` to the command, pass its output to `on_output` until the
    /// whole group closes the pipe, and wait for it. After `timeout` the
    /// group is killed. `on_output` returning false kills the group too,
    /// because nobody can receive the rest.
    pub fn supervise(
        mut self,
        stdin: Option<Vec<u8>>,
        timeout: Duration,
        mut on_output: impl FnMut(&[u8]) -> bool,
    ) -> Exit {
        if let (Some(bytes), Some(mut input)) = (stdin, self.child.stdin.take()) {
            thread::spawn(move || {
                let _ = input.write_all(&bytes);
            });
        }
        let (finished, watch) = mpsc::channel::<()>();
        let timed_out = Arc::new(AtomicBool::new(false));
        let watchdog = {
            let timed_out = Arc::clone(&timed_out);
            let group = self.group;
            let scope = self.scope.clone();
            thread::spawn(move || {
                if let Err(mpsc::RecvTimeoutError::Timeout) = watch.recv_timeout(timeout) {
                    timed_out.store(true, Ordering::SeqCst);
                    kill_command(group, scope.as_deref());
                }
            })
        };
        let mut buffer = vec![0u8; 64 << 10];
        loop {
            match self.output.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    if !on_output(&buffer[..count]) {
                        kill_command(self.group, self.scope.as_deref());
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        let status = self.child.wait();
        let _ = finished.send(());
        let _ = watchdog.join();
        let (rc, signal) = match status {
            Ok(status) => (status.code(), status.signal()),
            Err(_) => (None, None),
        };
        Exit {
            rc,
            signal,
            timed_out: timed_out.load(Ordering::SeqCst),
        }
    }
}

// --------------------------------------------------------------------- files

/// A file being written to a temporary sibling of its destination. Dropping
/// it before [`Sealed::rename`] removes the temporary file, so a reader never
/// sees a partial file and an abandoned write leaves nothing behind.
pub struct AtomicFile {
    temporary: PathBuf,
    destination: PathBuf,
    mode: Option<u32>,
    file: Option<File>,
    tally: Tally,
}

/// A written file whose bytes matched and reached the disk, waiting to be
/// renamed into place. Dropping it removes the temporary file.
pub struct Sealed {
    temporary: PathBuf,
    destination: PathBuf,
    digest: Digest256,
    renamed: bool,
}

impl AtomicFile {
    /// Open a write to `path`, resolved against `home`. When `path` is an
    /// existing directory, the file goes into it under `name`, as `scp`
    /// copies into a directory under the source's own name. `tag` names the
    /// writer in the temporary file's name; `unique` keeps two writes apart.
    pub fn create(
        path: &str,
        name: &str,
        mode: Option<u32>,
        home: &Path,
        tag: &str,
        unique: u64,
    ) -> Result<AtomicFile, ActionError> {
        let mut destination = resolve(path, home);
        if destination.is_dir() {
            if name.is_empty() || name == "." || name == ".." || name.contains('/') {
                return Err(ActionError::new(
                    "invalid_request",
                    "a directory destination needs a plain file name",
                ));
            }
            destination = destination.join(name);
        }
        let parent = destination
            .parent()
            .filter(|parent| parent.is_dir())
            .ok_or_else(|| {
                ActionError::new(
                    "not_found",
                    format!("no such directory for {}", destination.display()),
                )
            })?;
        let file_name = destination
            .file_name()
            .ok_or_else(|| ActionError::new("invalid_request", "destination has no file name"))?;
        let temporary = parent.join(format!(
            ".{}.{tag}-{}-{unique}.tmp",
            file_name.to_string_lossy(),
            std::process::id()
        ));
        let file = File::create(&temporary)
            .map_err(|error| ActionError::new("write_failed", error.to_string()))?;
        Ok(AtomicFile {
            temporary,
            destination,
            mode,
            file: Some(file),
            tally: Tally::default(),
        })
    }

    /// The path the file will be renamed to.
    pub fn destination(&self) -> &Path {
        &self.destination
    }

    /// Append `bytes`.
    pub fn append(&mut self, bytes: &[u8]) -> Result<(), ActionError> {
        let file = self.file.as_mut().expect("an open atomic file");
        file.write_all(bytes)
            .map_err(|error| ActionError::new("write_failed", error.to_string()))?;
        self.tally.update(bytes);
        Ok(())
    }

    /// Check the bytes against the declared size and SHA-256, flush them to
    /// the disk and set the mode. Only a sealed file may be renamed.
    pub fn seal(mut self, size: u64, sha256: &str) -> Result<Sealed, ActionError> {
        let digest = std::mem::take(&mut self.tally).finish();
        let file = self.file.take().expect("an open atomic file");
        let sealed = Sealed {
            temporary: std::mem::take(&mut self.temporary),
            destination: std::mem::take(&mut self.destination),
            digest,
            renamed: false,
        };
        if sealed.digest.size != size || sealed.digest.sha256 != sha256 {
            return Err(ActionError::new(
                "checksum_mismatch",
                "file does not match its end frame",
            ));
        }
        file.sync_all()
            .map_err(|error| ActionError::new("write_failed", error.to_string()))?;
        drop(file);
        if let Some(mode) = self.mode {
            std::fs::set_permissions(
                &sealed.temporary,
                std::fs::Permissions::from_mode(mode & 0o7777),
            )
            .map_err(|error| ActionError::new("write_failed", error.to_string()))?;
        }
        Ok(sealed)
    }
}

impl Drop for AtomicFile {
    fn drop(&mut self) {
        if !self.temporary.as_os_str().is_empty() {
            let _ = std::fs::remove_file(&self.temporary);
        }
    }
}

impl Sealed {
    /// The size and SHA-256 of the file.
    pub fn digest(&self) -> &Digest256 {
        &self.digest
    }

    /// Rename the file into place: the write's one visible effect.
    pub fn rename(mut self) -> Result<Digest256, ActionError> {
        std::fs::rename(&self.temporary, &self.destination)
            .map_err(|error| ActionError::new("write_failed", error.to_string()))?;
        self.renamed = true;
        Ok(self.digest.clone())
    }
}

impl Drop for Sealed {
    fn drop(&mut self) {
        if !self.renamed {
            let _ = std::fs::remove_file(&self.temporary);
        }
    }
}

/// Open `path` for a read. A directory is answered by `is_directory`, a
/// missing file by `not_found`.
pub fn open_read(path: &Path) -> Result<File, ActionError> {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => Err(ActionError::new(
            "is_directory",
            format!("{} is a directory", path.display()),
        )),
        Ok(_) => {
            File::open(path).map_err(|error| ActionError::new("read_failed", error.to_string()))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(ActionError::new(
            "not_found",
            format!("no such file {}", path.display()),
        )),
        Err(error) => Err(ActionError::new("read_failed", error.to_string())),
    }
}

/// Read `file` in pieces of at most `chunk` bytes, passing each to `send`,
/// and return the size and SHA-256 of all of them. `send` returning false
/// stops the read with `stopped`.
pub fn read_chunks(
    mut file: impl Read,
    chunk: usize,
    mut send: impl FnMut(&[u8]) -> bool,
) -> Result<Digest256, ActionError> {
    let mut tally = Tally::default();
    let mut buffer = vec![0u8; chunk];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => return Ok(tally.finish()),
            Ok(count) => {
                tally.update(&buffer[..count]);
                if !send(&buffer[..count]) {
                    return Err(ActionError::new("stopped", "the read was stopped"));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(ActionError::new("read_failed", error.to_string())),
        }
    }
}
