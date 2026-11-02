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
        self,
        stdin: Option<Vec<u8>>,
        timeout: Duration,
        on_output: impl FnMut(&[u8]) -> bool,
    ) -> Exit {
        let input = stdin.map(|bytes| Box::new(io::Cursor::new(bytes)) as Box<dyn Read + Send>);
        self.supervise_from(input, timeout, on_output)
    }

    /// [`Self::supervise`], with standard input read from `stdin`, such as
    /// a spooled upload, so the input never has to fit in memory.
    pub fn supervise_from(
        mut self,
        stdin: Option<Box<dyn Read + Send>>,
        timeout: Duration,
        mut on_output: impl FnMut(&[u8]) -> bool,
    ) -> Exit {
        if let (Some(mut reader), Some(mut input)) = (stdin, self.child.stdin.take()) {
            thread::spawn(move || {
                let _ = io::copy(&mut reader, &mut input);
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

/// Where a file written to `path` lands, resolved against `home`, with its
/// parent folder and its file name. When `path` is an existing directory,
/// the file goes into it under `name`, as `scp` copies into a directory
/// under the source's own name.
fn file_destination(
    path: &str,
    name: &str,
    home: &Path,
) -> Result<(PathBuf, PathBuf, String), ActionError> {
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
        })?
        .to_path_buf();
    let file_name = destination
        .file_name()
        .ok_or_else(|| ActionError::new("invalid_request", "destination has no file name"))?
        .to_string_lossy()
        .into_owned();
    Ok((destination, parent, file_name))
}

/// Where a folder written to `path` lands, as `scp -r` places it, with its
/// parent folder and its name. A destination that is a file is refused.
fn tree_destination(
    path: &str,
    name: &str,
    home: &Path,
) -> Result<(PathBuf, PathBuf, String), ActionError> {
    let destination = byte_transfer::tree_destination(&resolve(path, home), name)
        .map_err(|message| ActionError::new("invalid_request", message))?;
    let parent = destination
        .parent()
        .filter(|parent| parent.is_dir())
        .ok_or_else(|| {
            ActionError::new(
                "not_found",
                format!("no such directory for {}", destination.display()),
            )
        })?
        .to_path_buf();
    let file_name = destination
        .file_name()
        .ok_or_else(|| ActionError::new("invalid_request", "destination has no name"))?
        .to_string_lossy()
        .into_owned();
    if destination.exists() && !destination.is_dir() {
        return Err(ActionError::new(
            "not_a_directory",
            format!("{} exists and is not a folder", destination.display()),
        ));
    }
    Ok((destination, parent, file_name))
}

// ------------------------------------------------------------------ spools

/// The bytes of a resumable upload so far, in a file of their own: the
/// temporary sibling of a file's destination, a folder's tar stream beside
/// it, or a command's standard input. A spool lives as long as its upload,
/// across lost links and restarts of the agent; it is reopened by its path
/// and continues where its committed bytes end. Nothing in it is visible at
/// the destination before the commit point.
pub struct Spool {
    path: PathBuf,
    file: File,
    tally: Tally,
    synced: u64,
}

impl Spool {
    /// A new, empty spool at `path`, replacing whatever is there.
    pub fn create(path: &Path) -> Result<Spool, ActionError> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .read(true)
            .open(path)
            .map_err(|error| ActionError::new("write_failed", error.to_string()))?;
        Ok(Spool {
            path: path.to_path_buf(),
            file,
            tally: Tally::default(),
            synced: 0,
        })
    }

    /// Reopen the spool at `path`, keeping its first `committed` bytes and
    /// dropping whatever follows them; the kept bytes are hashed again, so
    /// the stream's SHA-256 still covers it whole. A spool shorter than
    /// `committed` is refused (`write_failed`): its bytes were lost.
    pub fn reopen(path: &Path, committed: u64) -> Result<Spool, ActionError> {
        let failed = |error: io::Error| ActionError::new("write_failed", error.to_string());
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .read(true)
            .open(path)
            .map_err(failed)?;
        let length = file.metadata().map_err(failed)?.len();
        if length < committed {
            return Err(ActionError::new(
                "write_failed",
                format!(
                    "{} holds {length} bytes, fewer than the {committed} committed",
                    path.display()
                ),
            ));
        }
        file.set_len(committed).map_err(failed)?;
        let mut tally = Tally::default();
        let mut buffer = vec![0u8; 1 << 20];
        let mut reader = io::Read::by_ref(&mut file).take(committed);
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => tally.update(&buffer[..count]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(failed(error)),
            }
        }
        if tally.size() != committed {
            return Err(ActionError::new(
                "write_failed",
                format!("{} could not be read back whole", path.display()),
            ));
        }
        Ok(Spool {
            path: path.to_path_buf(),
            file,
            tally,
            synced: committed,
        })
    }

    /// The spool's file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append the next bytes of the stream.
    pub fn append(&mut self, bytes: &[u8]) -> Result<(), ActionError> {
        self.file
            .write_all(bytes)
            .map_err(|error| ActionError::new("write_failed", error.to_string()))?;
        self.tally.update(bytes);
        Ok(())
    }

    /// The bytes appended so far.
    pub fn written(&self) -> u64 {
        self.tally.size()
    }

    /// The bytes known to be on the disk: those of the last [`Self::sync`].
    pub fn committed(&self) -> u64 {
        self.synced
    }

    /// Flush every byte appended to the disk, and return how many there
    /// are: the committed offset a sender continues from.
    pub fn sync(&mut self) -> Result<u64, ActionError> {
        if self.synced != self.written() {
            self.file
                .sync_data()
                .map_err(|error| ActionError::new("write_failed", error.to_string()))?;
            self.synced = self.written();
        }
        Ok(self.synced)
    }

    /// Check the stream against the size and SHA-256 of its end. Fewer
    /// bytes than the end names is `upload_incomplete`: the spool is kept
    /// and the sender continues from its committed offset. More bytes, or
    /// another SHA-256, is `checksum_mismatch`: the stream is wrong and the
    /// caller discards it. Neither ever reaches the destination.
    pub fn check(&mut self, size: u64, sha256: &str) -> Result<Digest256, ActionError> {
        let written = self.written();
        if written < size {
            return Err(ActionError::new(
                "upload_incomplete",
                format!("the agent holds {written} of the {size} bytes the end names"),
            ));
        }
        let digest = self.tally.clone().finish();
        if digest.size != size || digest.sha256 != sha256 {
            return Err(ActionError::new(
                "checksum_mismatch",
                "the upload does not match its end",
            ));
        }
        self.file
            .sync_all()
            .map_err(|error| ActionError::new("write_failed", error.to_string()))?;
        self.synced = written;
        Ok(digest)
    }

    /// Remove the spool's file.
    pub fn discard(self) {
        let _ = std::fs::remove_file(&self.path);
    }

    /// The spool's bytes from the start, for a command's standard input.
    pub fn reader(&self) -> io::Result<File> {
        File::open(&self.path)
    }
}

/// Where a resumable file write lands: its destination, and the temporary
/// sibling its bytes are spooled to, named by `key` so that a resumed
/// upload of the same execution finds it again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileTarget {
    /// The path the file is renamed to at the commit point.
    pub destination: PathBuf,
    /// The hidden sibling that holds the bytes until then.
    pub partial: PathBuf,
}

/// Resolve a resumable write to `path`, as [`AtomicFile::create`] does.
pub fn file_target(
    path: &str,
    name: &str,
    home: &Path,
    tag: &str,
    key: &str,
) -> Result<FileTarget, ActionError> {
    let (destination, parent, file_name) = file_destination(path, name, home)?;
    Ok(FileTarget {
        partial: parent.join(format!(".{file_name}.{tag}-{key}.part")),
        destination,
    })
}

/// Seal a spooled file whose stream [`Spool::check`] accepted: its bytes
/// are on the disk; set its mode, and it waits to be renamed into place.
pub fn seal_spooled_file(
    spool: Spool,
    digest: Digest256,
    destination: &Path,
    mode: Option<u32>,
) -> Result<Sealed, ActionError> {
    let sealed = Sealed {
        temporary: spool.path.clone(),
        destination: destination.to_path_buf(),
        digest,
        renamed: false,
    };
    drop(spool);
    if let Some(mode) = mode {
        std::fs::set_permissions(
            &sealed.temporary,
            std::fs::Permissions::from_mode(mode & 0o7777),
        )
        .map_err(|error| ActionError::new("write_failed", error.to_string()))?;
    }
    Ok(sealed)
}

/// Where a resumable folder write lands: its destination, the hidden
/// sibling its tar stream is spooled to, and the one it is unpacked into,
/// both named by `key`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeTarget {
    /// The path the folder is put at, at the commit point.
    pub destination: PathBuf,
    /// The hidden sibling that holds the tar stream.
    pub partial: PathBuf,
    /// The hidden sibling the stream is unpacked into at the commit point.
    pub staged: PathBuf,
}

/// Resolve a resumable folder write to `path`, as [`AtomicTree::create`]
/// does.
pub fn tree_target(
    path: &str,
    name: &str,
    home: &Path,
    tag: &str,
    key: &str,
) -> Result<TreeTarget, ActionError> {
    let (destination, parent, file_name) = tree_destination(path, name, home)?;
    let stem = format!(".{file_name}.{tag}-{key}");
    Ok(TreeTarget {
        partial: parent.join(format!("{stem}.tar.part")),
        staged: parent.join(format!("{stem}.tree.tmp")),
        destination,
    })
}

/// Unpack a spooled tar stream that [`Spool::check`] accepted into
/// `staged`, beside `destination`, and remove the spool. A stream that
/// names a path outside the folder, a link or a device is refused whole
/// (`bad_archive`), and nothing is left of it.
pub fn seal_spooled_tree(
    spool: Spool,
    digest: Digest256,
    staged: &Path,
    destination: &Path,
) -> Result<SealedTree, ActionError> {
    let path = spool.path.clone();
    drop(spool);
    let _ = std::fs::remove_dir_all(staged);
    let unpacked = byte_transfer::unpack(&path, staged)
        .map_err(|message| ActionError::new("bad_archive", message));
    let _ = std::fs::remove_file(&path);
    unpacked?;
    Ok(SealedTree {
        staged: staged.to_path_buf(),
        destination: destination.to_path_buf(),
        digest,
        placed: false,
    })
}

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
        let (destination, parent, file_name) = file_destination(path, name, home)?;
        let temporary = parent.join(format!(
            ".{file_name}.{tag}-{}-{unique}.tmp",
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

// --------------------------------------------------------------------- trees

/// A folder being written: its tar stream is spooled to a hidden sibling
/// of its destination, then checked, unpacked into another hidden sibling,
/// and only then put in place. Dropping it removes what it made.
pub struct AtomicTree {
    spool: PathBuf,
    staged: PathBuf,
    destination: PathBuf,
    file: Option<File>,
    tally: Tally,
}

/// A written folder whose stream matched and unpacked, waiting to be put
/// in place. Dropping it removes the unpacked folder.
pub struct SealedTree {
    staged: PathBuf,
    destination: PathBuf,
    digest: Digest256,
    placed: bool,
}

impl AtomicTree {
    /// Open a folder write to `path`, resolved against `home`. When `path`
    /// is an existing folder, the folder goes into it under `name`, as
    /// `scp -r` copies a folder into a folder.
    pub fn create(
        path: &str,
        name: &str,
        home: &Path,
        tag: &str,
        unique: u64,
    ) -> Result<AtomicTree, ActionError> {
        let (destination, parent, file_name) = tree_destination(path, name, home)?;
        let stem = format!(".{file_name}.{tag}-{}-{unique}", std::process::id());
        let spool = parent.join(format!("{stem}.tar.tmp"));
        let file = File::create(&spool)
            .map_err(|error| ActionError::new("write_failed", error.to_string()))?;
        Ok(AtomicTree {
            spool,
            staged: parent.join(format!("{stem}.tree.tmp")),
            destination,
            file: Some(file),
            tally: Tally::default(),
        })
    }

    /// The path the folder will be put at.
    pub fn destination(&self) -> &Path {
        &self.destination
    }

    /// Append bytes of the tar stream.
    pub fn append(&mut self, bytes: &[u8]) -> Result<(), ActionError> {
        let file = self.file.as_mut().expect("an open spool");
        file.write_all(bytes)
            .map_err(|error| ActionError::new("write_failed", error.to_string()))?;
        self.tally.update(bytes);
        Ok(())
    }

    /// Check the stream against the declared size and SHA-256, and unpack
    /// it beside the destination. A stream that names a path outside the
    /// folder, a link or a device is refused whole (`bad_archive`).
    pub fn seal(mut self, size: u64, sha256: &str) -> Result<SealedTree, ActionError> {
        let digest = std::mem::take(&mut self.tally).finish();
        drop(self.file.take());
        if digest.size != size || digest.sha256 != sha256 {
            return Err(ActionError::new(
                "checksum_mismatch",
                "the folder's stream does not match its end",
            ));
        }
        byte_transfer::unpack(&self.spool, &self.staged)
            .map_err(|message| ActionError::new("bad_archive", message))?;
        let _ = std::fs::remove_file(&self.spool);
        Ok(SealedTree {
            staged: std::mem::take(&mut self.staged),
            destination: std::mem::take(&mut self.destination),
            digest,
            placed: false,
        })
    }
}

impl Drop for AtomicTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.spool);
        if !self.staged.as_os_str().is_empty() {
            let _ = std::fs::remove_dir_all(&self.staged);
        }
    }
}

impl SealedTree {
    /// The size and SHA-256 of the folder's tar stream.
    pub fn digest(&self) -> &Digest256 {
        &self.digest
    }

    /// Put the folder in place: a rename, or an atomic exchange with the
    /// folder already there, which is then removed. The write's one visible
    /// effect.
    pub fn place(mut self) -> Result<Digest256, ActionError> {
        byte_transfer::place_tree(&self.staged, &self.destination)
            .map_err(|error| ActionError::new("write_failed", error.to_string()))?;
        self.placed = true;
        Ok(self.digest.clone())
    }
}

impl Drop for SealedTree {
    fn drop(&mut self) {
        if !self.placed {
            let _ = std::fs::remove_dir_all(&self.staged);
        }
    }
}

/// Send the tar stream of the folder at `path` in pieces of at most `chunk`
/// bytes, and return the size and SHA-256 of the stream. A file is
/// answered by `not_a_directory`, a missing path by `not_found`, and a
/// folder that holds anything but files and folders by
/// `unsupported_entry`, before any byte is sent. `send` returning false
/// stops the read with `stopped`.
pub fn read_tree(
    path: &Path,
    chunk: usize,
    send: impl FnMut(&[u8]) -> bool,
) -> Result<Digest256, ActionError> {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(ActionError::new(
                "not_a_directory",
                format!("{} is not a folder", path.display()),
            ))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(ActionError::new(
                "not_found",
                format!("no such folder {}", path.display()),
            ))
        }
        Err(error) => return Err(ActionError::new("read_failed", error.to_string())),
    }
    let writer = ChunkWriter {
        buffer: Vec::with_capacity(chunk),
        chunk,
        tally: Tally::default(),
        send,
        stopped: false,
    };
    let mut writer = byte_transfer::pack_into(path, writer).map_err(|message| {
        if message.contains("not a file or a folder") {
            ActionError::new("unsupported_entry", message)
        } else {
            ActionError::new("read_failed", message)
        }
    })?;
    writer
        .flush_chunk()
        .map_err(|_| ActionError::new("stopped", "the read was stopped"))?;
    Ok(writer.tally.finish())
}

/// A writer that hands on its bytes in pieces of `chunk` bytes.
struct ChunkWriter<F: FnMut(&[u8]) -> bool> {
    buffer: Vec<u8>,
    chunk: usize,
    tally: Tally,
    send: F,
    stopped: bool,
}

impl<F: FnMut(&[u8]) -> bool> ChunkWriter<F> {
    fn flush_chunk(&mut self) -> io::Result<()> {
        if self.stopped {
            return Err(io::Error::other("stopped"));
        }
        if !self.buffer.is_empty() {
            self.tally.update(&self.buffer);
            if !(self.send)(&self.buffer) {
                self.stopped = true;
                return Err(io::Error::other("stopped"));
            }
            self.buffer.clear();
        }
        Ok(())
    }
}

impl<F: FnMut(&[u8]) -> bool> Write for ChunkWriter<F> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let room = self.chunk - self.buffer.len();
        let taken = bytes.len().min(room);
        self.buffer.extend_from_slice(&bytes[..taken]);
        if self.buffer.len() == self.chunk {
            self.flush_chunk()?;
        }
        Ok(taken)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
