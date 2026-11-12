//! Standard OpenSSH identities scoped to a single VM lease.
//!
//! Port of `bin/lease_keys.py`. The caller owns VM lifecycle/locking. Never
//! recreate keys for an existing lease, including after an interrupted
//! create/bootstrap. Bootstrap trusts the FIRST host key seen (OpenSSH
//! accept-new TOFU), not a preverified VM identity. Subsequent connections use
//! strict host checking. Only call `cleanup` after verifying that the VM is
//! absent. Bootstrap failure deliberately leaves keys and host pins intact.

use std::collections::HashMap;
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File, Metadata};
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv6Addr};
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const PASSWORD_ENV: &str = "MACHINE_SERVICE_SSH_PASSWORD";

/// Error raised by every public `lease_keys` operation.
///
/// The `Display` text of each variant matches the Python `RuntimeError`
/// message. `Io` is an internal carrier for `OSError`; the public entry points
/// translate it to the same message the Python caller would observe.
#[derive(Debug, thiserror::Error)]
pub enum LeaseKeyError {
    #[error("Unsafe lease key path")]
    UnsafePath,
    #[error("Symlink in lease key path")]
    SymlinkInPath,
    #[error("Unsafe lease key ownership, type or permissions")]
    UnsafeOwnership,
    #[error("Invalid VM name for lease keys")]
    InvalidVmName,
    #[error("Unsafe lease key state directory")]
    UnsafeStateDir,
    #[error("Invalid lease key directory")]
    InvalidKeyDirectory,
    #[error("Empty lease identity")]
    EmptyIdentity,
    #[error("Unexpected entry in lease key directory")]
    UnexpectedEntry,
    #[error("Lease key generation failed")]
    KeyGenerationFailed,
    #[error("Could not create lease keys; existing keys are never replaced")]
    CreateFailed,
    #[error("Lease keys are missing or inaccessible")]
    KeysMissing,
    #[error("Invalid lease public key")]
    InvalidPublicKey,
    #[error("Invalid SSH bootstrap credentials")]
    InvalidBootstrapCredentials,
    #[error("Invalid SSH bootstrap deadline")]
    InvalidBootstrapDeadline,
    #[error("SSH bootstrap deadline exceeded")]
    BootstrapDeadlineExceeded,
    #[error("SSH bootstrap failed; remote details withheld")]
    BootstrapFailed,
    #[error("Could not remove SSH bootstrap helper")]
    HelperRemovalFailed,
    #[error("Could not safely remove lease keys")]
    CleanupFailed,
    /// Internal carrier for `OSError`; never surfaces from a public entry point.
    #[error(transparent)]
    Io(#[from] io::Error),
}

type EnvMap = HashMap<OsString, OsString>;

// ---------------------------------------------------------------------------
// Internal process seams
// ---------------------------------------------------------------------------

/// Combined result of a child process used by the key-generation seam.
enum RunResult {
    Exited { code: i32, stderr: Vec<u8> },
    TimedOut,
}

/// Runs `ssh-keygen`; a seam so tests can inject partial-generation failures.
trait Keygen {
    fn run(&self, argv: &[String], timeout: Duration) -> io::Result<RunResult>;
}

struct RealKeygen;

impl Keygen for RealKeygen {
    fn run(&self, argv: &[String], timeout: Duration) -> io::Result<RunResult> {
        let mut cmd = service_layout::tools::command_for(&argv[0]).map_err(io::Error::other)?;
        cmd.args(&argv[1..]);
        spawn_and_wait(cmd, None, false, timeout)
    }
}

/// Runs the bootstrap `ssh`; a seam so tests can inject transport outcomes.
trait SshTransport {
    fn run(
        &self,
        argv: &[String],
        stdin: &[u8],
        env: &EnvMap,
        timeout: Duration,
    ) -> io::Result<RunResult>;
}

struct RealSsh;

impl SshTransport for RealSsh {
    fn run(
        &self,
        argv: &[String],
        stdin: &[u8],
        env: &EnvMap,
        timeout: Duration,
    ) -> io::Result<RunResult> {
        let mut cmd = service_layout::tools::command_for(&argv[0]).map_err(io::Error::other)?;
        cmd.args(&argv[1..]);
        cmd.env_clear();
        cmd.envs(env);
        // SAFETY: `pre_exec` runs between fork and exec; `setsid` is
        // async-signal-safe and mirrors Python's `start_new_session=True`.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        spawn_and_wait(cmd, Some(stdin), true, timeout)
    }
}

/// Clock seam so the bootstrap deadline can be exercised deterministically.
trait Clock {
    fn now(&self) -> f64;
    fn sleep(&self, secs: f64);
}

struct RealClock {
    start: Instant,
}

impl RealClock {
    fn new() -> Self {
        RealClock {
            start: Instant::now(),
        }
    }
}

impl Clock for RealClock {
    fn now(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }

    fn sleep(&self, secs: f64) {
        if secs > 0.0 {
            std::thread::sleep(Duration::from_secs_f64(secs));
        }
    }
}

/// Spawn `cmd`, feed optional stdin, poll until exit or `timeout`, then kill.
///
/// `std::process` has no `subprocess.run(timeout=...)`; this is the small
/// polling helper. A separate thread drains stderr so a chatty child cannot
/// deadlock against the stdin writer.
fn spawn_and_wait(
    mut cmd: Command,
    stdin: Option<&[u8]>,
    capture_stderr: bool,
    timeout: Duration,
) -> io::Result<RunResult> {
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    cmd.stdout(Stdio::null());
    cmd.stderr(if capture_stderr {
        Stdio::piped()
    } else {
        Stdio::null()
    });

    let mut child = cmd.spawn()?;

    let stderr_reader = if capture_stderr {
        child.stderr.take().map(|mut stream| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = stream.read_to_end(&mut buf);
                buf
            })
        })
    } else {
        None
    };

    if let Some(data) = stdin {
        if let Some(mut sink) = child.stdin.take() {
            if let Err(error) = sink.write_all(data) {
                // Python's `communicate` swallows a broken stdin pipe.
                if error.kind() != io::ErrorKind::BrokenPipe {
                    return Err(error);
                }
            }
        }
    }

    let deadline = Instant::now() + timeout;
    let status: Option<ExitStatus> = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(5));
    };

    let stderr = match stderr_reader {
        Some(handle) => handle.join().unwrap_or_default(),
        None => Vec::new(),
    };

    match status {
        Some(status) => Ok(RunResult::Exited {
            code: status.code().unwrap_or(-1),
            stderr,
        }),
        None => Ok(RunResult::TimedOut),
    }
}

// ---------------------------------------------------------------------------
// Path and permission primitives
// ---------------------------------------------------------------------------

fn current_uid() -> u32 {
    // SAFETY: `getuid` takes no arguments and cannot fail.
    unsafe { libc::getuid() }
}

/// Lexical `Path.absolute()`: prepend the cwd when relative and remove `.`
/// components. `..` is rejected before this point, so no parent resolution is
/// needed; symlinks are deliberately not resolved.
fn absolute_lexical(path: &Path) -> io::Result<PathBuf> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut out = PathBuf::new();
    for component in joined.components() {
        if matches!(component, Component::CurDir) {
            continue;
        }
        out.push(component.as_os_str());
    }
    Ok(out)
}

/// Port of `_path`: reject `..`, NUL/CR/LF/`${`/`%`, then reject a symlink in
/// any component of the absolute path.
fn validate_path(path: &Path) -> Result<PathBuf, LeaseKeyError> {
    for component in path.components() {
        if matches!(component, Component::ParentDir) {
            return Err(LeaseKeyError::UnsafePath);
        }
    }
    let raw = path.as_os_str().as_bytes();
    if raw.iter().any(|&b| b == 0 || b == b'\n' || b == b'\r')
        || raw.windows(2).any(|window| window == b"${")
        || raw.contains(&b'%')
    {
        return Err(LeaseKeyError::UnsafePath);
    }
    let absolute = absolute_lexical(path)?;
    for ancestor in absolute.ancestors().collect::<Vec<_>>().into_iter().rev() {
        if fs::symlink_metadata(ancestor)
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err(LeaseKeyError::SymlinkInPath);
        }
    }
    Ok(absolute)
}

/// Port of `_check`'s predicate against already-fetched metadata.
fn check_meta(meta: &Metadata, modes: &[u32], folder: bool, uid: u32) -> Result<(), LeaseKeyError> {
    let kind_ok = if folder {
        meta.is_dir()
    } else {
        meta.is_file()
    };
    let mode = meta.mode() & 0o7777;
    if !kind_ok || meta.uid() != uid || !modes.contains(&mode) || (!folder && meta.nlink() != 1) {
        return Err(LeaseKeyError::UnsafeOwnership);
    }
    Ok(())
}

/// Port of `_check`: `lstat` (never follows a link) plus ownership/type/mode.
fn check(path: &Path, modes: &[u32], folder: bool) -> Result<Metadata, LeaseKeyError> {
    let meta = fs::symlink_metadata(path).map_err(LeaseKeyError::Io)?;
    check_meta(&meta, modes, folder, current_uid())?;
    Ok(meta)
}

fn valid_vm_name(vm: &str) -> bool {
    let bytes = vm.as_bytes();
    if bytes.is_empty() || bytes.len() > 128 {
        return false;
    }
    if !bytes[0].is_ascii_alphanumeric() {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|&b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-')
}

/// Port of `_location`.
fn location(state_dir: &Path, vm: &str) -> Result<PathBuf, LeaseKeyError> {
    if !valid_vm_name(vm) {
        return Err(LeaseKeyError::InvalidVmName);
    }
    let state = validate_path(state_dir)?;
    let meta = fs::symlink_metadata(&state).map_err(LeaseKeyError::Io)?;
    if !meta.is_dir() || meta.uid() != current_uid() || (meta.mode() & 0o022) != 0 {
        return Err(LeaseKeyError::UnsafeStateDir);
    }
    Ok(state.join("ssh").join(vm))
}

/// Port of `_files`: validate an existing lease key directory and return it.
fn files(path: &Path) -> Result<PathBuf, LeaseKeyError> {
    let path = validate_path(path)?;
    let parent = match path.parent() {
        Some(parent) => parent,
        None => return Err(LeaseKeyError::InvalidKeyDirectory),
    };
    if parent.file_name() != Some(OsStr::new("ssh")) {
        return Err(LeaseKeyError::InvalidKeyDirectory);
    }
    let vm = match path.file_name().and_then(OsStr::to_str) {
        Some(vm) => vm,
        None => return Err(LeaseKeyError::InvalidKeyDirectory),
    };
    let grandparent = match parent.parent() {
        Some(grandparent) => grandparent,
        None => return Err(LeaseKeyError::InvalidKeyDirectory),
    };
    if location(grandparent, vm)? != path {
        return Err(LeaseKeyError::InvalidKeyDirectory);
    }
    check(parent, &[0o700], true)?;
    check(&path, &[0o700], true)?;
    let identity = check(&path.join("identity"), &[0o600], false)?;
    if identity.len() == 0 {
        return Err(LeaseKeyError::EmptyIdentity);
    }
    let public = check(&path.join("identity.pub"), &[0o600, 0o644], false)?;
    if public.len() == 0 {
        return Err(LeaseKeyError::EmptyIdentity);
    }
    check(&path.join("known_hosts"), &[0o600], false)?;
    Ok(path)
}

// ---------------------------------------------------------------------------
// OpenSSH option construction
// ---------------------------------------------------------------------------

fn options(settings: &[&str]) -> Vec<String> {
    let mut out = Vec::with_capacity(settings.len() * 2);
    for setting in settings {
        out.push("-o".to_string());
        out.push((*setting).to_string());
    }
    out
}

/// Port of `_common`. OpenSSH parses `-o` values as config, so the known_hosts
/// path is quoted for `%`, `\` and `"` expansion.
fn common(path: &Path, bootstrap: bool) -> Vec<String> {
    let hosts = path
        .join("known_hosts")
        .to_string_lossy()
        .replace('%', "%%")
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    let known_hosts = format!("UserKnownHostsFile=\"{hosts}\"");
    let strict = if bootstrap {
        "StrictHostKeyChecking=accept-new"
    } else {
        "StrictHostKeyChecking=yes"
    };
    let mut out = vec!["-F".to_string(), "/dev/null".to_string()];
    out.extend(options(&[
        "IdentitiesOnly=yes",
        "IdentityAgent=none",
        "ForwardAgent=no",
        "ClearAllForwardings=yes",
        "PermitLocalCommand=no",
        "GlobalKnownHostsFile=/dev/null",
        known_hosts.as_str(),
        strict,
        "ConnectTimeout=8",
        "LogLevel=ERROR",
    ]));
    out
}

/// Validated options for the parent's normal ssh/scp commands; no password.
pub fn key_args(key_dir: &Path) -> Result<Vec<String>, LeaseKeyError> {
    match key_args_inner(key_dir) {
        Ok(args) => Ok(args),
        Err(LeaseKeyError::Io(_)) => Err(LeaseKeyError::KeysMissing),
        Err(error) => Err(error),
    }
}

fn key_args_inner(key_dir: &Path) -> Result<Vec<String>, LeaseKeyError> {
    let path = files(key_dir)?;
    let mut args = common(&path, false);
    args.push("-i".to_string());
    args.push(path.join("identity").to_string_lossy().into_owned());
    args.extend(options(&[
        "BatchMode=yes",
        "PreferredAuthentications=publickey",
        "PubkeyAuthentication=yes",
        "PasswordAuthentication=no",
        "KbdInteractiveAuthentication=no",
    ]));
    Ok(args)
}

// ---------------------------------------------------------------------------
// Public key parsing and provisioning script
// ---------------------------------------------------------------------------

/// Internal failure classes of `_provision_script`; only `Invalid` is a
/// `RuntimeError` in Python, the others are `OSError`/`UnicodeError`.
#[derive(Debug)]
enum ProvisionError {
    Io,
    NotAscii,
    Invalid,
}

fn valid_public_key(public: &str) -> bool {
    let rest = match public.strip_prefix("ssh-ed25519 ") {
        Some(rest) => rest,
        None => return false,
    };
    let bytes = rest.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let b = bytes[index];
        if b.is_ascii_alphanumeric() || b == b'+' || b == b'/' {
            index += 1;
        } else {
            break;
        }
    }
    if index == 0 {
        return false;
    }
    let mut padding = 0;
    while index < bytes.len() && bytes[index] == b'=' {
        padding += 1;
        if padding > 2 {
            return false;
        }
        index += 1;
    }
    if index == bytes.len() {
        return true;
    }
    if bytes[index] == b' ' {
        return !rest[index + 1..].contains(['\r', '\n']);
    }
    false
}

/// Port of `shlex.quote` for the ASCII public-key line.
fn shlex_quote(value: &str) -> String {
    fn safe(c: char) -> bool {
        c.is_ascii_alphanumeric()
            || c == '_'
            || matches!(c, '@' | '%' | '+' | '=' | ':' | ',' | '.' | '/' | '-')
    }
    if value.is_empty() {
        return "''".to_string();
    }
    if value.chars().all(safe) {
        return value.to_string();
    }
    let mut out = String::from("'");
    for c in value.chars() {
        if c == '\'' {
            out.push_str("'\"'\"'");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Port of `_provision_script`; the script is byte-identical to Python's.
fn provision_script(path: &Path) -> Result<String, ProvisionError> {
    let raw = read_nofollow(&path.join("identity.pub")).map_err(|_| ProvisionError::Io)?;
    if !raw.is_ascii() {
        return Err(ProvisionError::NotAscii);
    }
    let public = String::from_utf8(raw).map_err(|_| ProvisionError::NotAscii)?;
    let public = public.trim();
    if public.len() > 4096 || !valid_public_key(public) {
        return Err(ProvisionError::Invalid);
    }
    Ok(format!(
        "set -eu\numask 077\nssh_dir=\"$HOME/.ssh\"\nauthorized=\"$ssh_dir/authorized_keys\"\n[ ! -L \"$ssh_dir\" ] || exit 73\nif [ -e \"$ssh_dir\" ]; then\n    [ -d \"$ssh_dir\" ] || exit 73\nelse\n    mkdir \"$ssh_dir\"\nfi\nchmod 700 \"$ssh_dir\"\n[ ! -L \"$authorized\" ] || exit 73\nif [ -e \"$authorized\" ]; then\n    [ -f \"$authorized\" ] || exit 73\nelse\n    : > \"$authorized\"\nfi\nchmod 600 \"$authorized\"\nkey={key}\nif ! grep -F -x -q -- \"$key\" \"$authorized\"; then\n    printf '\\n%s\\n' \"$key\" >> \"$authorized\"\nfi\n",
        key = shlex_quote(public)
    ))
}

/// Read a whole file with `O_NOFOLLOW`, up to `limit` bytes.
fn read_nofollow(path: &Path) -> io::Result<Vec<u8>> {
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    // SAFETY: `open` is a plain syscall wrapper; the fd is owned by `File`.
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a valid, uniquely owned descriptor returned by `open`.
    let file = unsafe { File::from_raw_fd(fd) };
    let mut buf = Vec::new();
    file.take(4097).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Create a file with `O_CREAT | O_EXCL | O_NOFOLLOW` and the given mode.
fn create_exclusive_nofollow(path: &Path, mode: u32) -> io::Result<()> {
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    // SAFETY: variadic `open` with the mode argument required by `O_CREAT`.
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` was just opened and is closed exactly once.
    unsafe { libc::close(fd) };
    Ok(())
}

// ---------------------------------------------------------------------------
// create
// ---------------------------------------------------------------------------

/// Exclusively create `state_dir/ssh/vm` (0700); never overwrite/rekey.
pub fn create(state_dir: &Path, vm: &str) -> Result<PathBuf, LeaseKeyError> {
    create_with(state_dir, vm, &RealKeygen)
}

fn create_with(state_dir: &Path, vm: &str, keygen: &dyn Keygen) -> Result<PathBuf, LeaseKeyError> {
    let mut created = false;
    let outcome = reserve_and_generate(state_dir, vm, keygen, &mut created);
    match outcome {
        Ok(path) => Ok(path),
        Err(_) => {
            if created {
                cleanup(state_dir, vm)?;
            }
            Err(LeaseKeyError::CreateFailed)
        }
    }
}

fn reserve_and_generate(
    state_dir: &Path,
    vm: &str,
    keygen: &dyn Keygen,
    created: &mut bool,
) -> Result<PathBuf, LeaseKeyError> {
    let path = location(state_dir, vm)?;
    let parent = match path.parent() {
        Some(parent) => parent.to_path_buf(),
        None => return Err(LeaseKeyError::InvalidKeyDirectory),
    };
    // Atomic reservation: EEXIST always fails closed.
    match fs::DirBuilder::new().mode(0o700).create(&parent) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(LeaseKeyError::Io(error)),
    }
    check(&parent, &[0o700], true)?;
    match fs::DirBuilder::new().mode(0o700).create(&path) {
        Ok(()) => *created = true,
        Err(error) => return Err(LeaseKeyError::Io(error)),
    }
    check(&path, &[0o700], true)?;
    let argv = vec![
        "ssh-keygen".to_string(),
        "-q".to_string(),
        "-t".to_string(),
        "ed25519".to_string(),
        "-N".to_string(),
        String::new(),
        "-C".to_string(),
        "vm-service-lease".to_string(),
        "-f".to_string(),
        path.join("identity").to_string_lossy().into_owned(),
    ];
    match keygen.run(&argv, Duration::from_secs(30)) {
        Ok(RunResult::Exited { code: 0, .. }) => {}
        Ok(_) => return Err(LeaseKeyError::KeyGenerationFailed),
        Err(error) => return Err(LeaseKeyError::Io(error)),
    }
    check(&path.join("identity"), &[0o600], false)?;
    check(&path.join("identity.pub"), &[0o600, 0o644], false)?;
    fs::set_permissions(path.join("identity.pub"), fs::Permissions::from_mode(0o600))?;
    create_exclusive_nofollow(&path.join("known_hosts"), 0o600)?;
    files(&path)
}

// ---------------------------------------------------------------------------
// directory
// ---------------------------------------------------------------------------

/// Validate existing keys and return their directory, without changing it.
pub fn directory(state_dir: &Path, vm: &str) -> Result<PathBuf, LeaseKeyError> {
    match (|| -> Result<PathBuf, LeaseKeyError> {
        let path = location(state_dir, vm)?;
        files(&path)
    })() {
        Ok(path) => Ok(path),
        Err(LeaseKeyError::Io(_)) => Err(LeaseKeyError::KeysMissing),
        Err(error) => Err(error),
    }
}

// ---------------------------------------------------------------------------
// bootstrap
// ---------------------------------------------------------------------------

/// Install this lease's public key using native `SSH_ASKPASS`, bounded retries.
///
/// The password lives only in the subprocess environment; it never appears in
/// argv, the helper source, or an error message. Only connectivity and
/// authentication failures are retried; host-key failures fail immediately.
pub fn bootstrap(
    ip: &str,
    user: &str,
    password: &str,
    key_dir: &Path,
    timeout_s: f64,
) -> Result<(), LeaseKeyError> {
    let base_env: EnvMap = std::env::vars_os().collect();
    bootstrap_with(
        ip,
        user,
        password,
        key_dir,
        timeout_s,
        &RealClock::new(),
        &RealSsh,
        &base_env,
    )
}

// Mirrors the Python `bootstrap(...)` parameter list; grouping them would only
// obscure which argument is a credential and which is a constraint.
#[allow(clippy::too_many_arguments)]
fn bootstrap_with(
    ip: &str,
    user: &str,
    password: &str,
    key_dir: &Path,
    timeout_s: f64,
    clock: &dyn Clock,
    transport: &dyn SshTransport,
    base_env: &EnvMap,
) -> Result<(), LeaseKeyError> {
    let mut helper: Option<PathBuf> = None;
    let outcome = bootstrap_body(
        ip,
        user,
        password,
        key_dir,
        timeout_s,
        clock,
        transport,
        base_env,
        &mut helper,
    );
    if let Some(path) = helper {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return Err(LeaseKeyError::HelperRemovalFailed),
        }
    }
    outcome
}

/// Mirrors CPython `ipaddress.ip_address` for a `str` (`bin/lease_keys.py:183`).
/// CPython tries IPv4, then IPv6; an IPv6 literal may carry one `%zone`
/// suffix, and the zone may contain neither `%` nor `/`. `IpAddr::from_str`
/// accepts neither the zone nor the IPv6-only restriction, so zone-qualified
/// literals are validated here.
fn is_ip_literal(ip: &str) -> bool {
    if ip.parse::<IpAddr>().is_ok() {
        return true;
    }
    match ip.split_once('%') {
        Some((address, zone)) => {
            !zone.is_empty()
                && !zone.contains('%')
                && !zone.contains('/')
                && address.parse::<Ipv6Addr>().is_ok()
        }
        None => false,
    }
}

fn valid_ssh_user(user: &str) -> bool {
    let bytes = user.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    if !(bytes[0].is_ascii_alphabetic() || bytes[0] == b'_') {
        return false;
    }
    let body = if bytes.last() == Some(&b'$') {
        &bytes[..bytes.len() - 1]
    } else {
        bytes
    };
    body[1..]
        .iter()
        .all(|&b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-')
}

#[allow(clippy::too_many_arguments)]
fn bootstrap_body(
    ip: &str,
    user: &str,
    password: &str,
    key_dir: &Path,
    timeout_s: f64,
    clock: &dyn Clock,
    transport: &dyn SshTransport,
    base_env: &EnvMap,
    helper: &mut Option<PathBuf>,
) -> Result<(), LeaseKeyError> {
    if !is_ip_literal(ip) {
        // Python `ValueError` from `ipaddress.ip_address`.
        return Err(LeaseKeyError::BootstrapFailed);
    }
    if !valid_ssh_user(user)
        || password.is_empty()
        || password.bytes().any(|b| b == 0 || b == b'\n' || b == b'\r')
    {
        return Err(LeaseKeyError::InvalidBootstrapCredentials);
    }
    if !timeout_s.is_finite() || timeout_s <= 0.0 {
        return Err(LeaseKeyError::InvalidBootstrapDeadline);
    }
    let deadline = clock.now() + timeout_s;
    let path = match files(key_dir) {
        Ok(path) => path,
        Err(LeaseKeyError::Io(_)) => return Err(LeaseKeyError::BootstrapFailed),
        Err(error) => return Err(error),
    };
    let script = match provision_script(&path) {
        Ok(script) => script,
        Err(ProvisionError::Invalid) => return Err(LeaseKeyError::InvalidPublicKey),
        Err(_) => return Err(LeaseKeyError::BootstrapFailed),
    };
    let helper_path = create_helper(&path).map_err(|_| LeaseKeyError::BootstrapFailed)?;
    *helper = Some(helper_path.clone());

    let mut env = base_env.clone();
    env.remove(OsStr::new("SSH_AUTH_SOCK"));
    env.remove(OsStr::new("SSH_AGENT_PID"));
    env.insert(OsString::from(PASSWORD_ENV), OsString::from(password));
    env.insert(OsString::from("SSH_ASKPASS"), helper_path.into_os_string());
    env.insert(
        OsString::from("SSH_ASKPASS_REQUIRE"),
        OsString::from("force"),
    );
    env.insert(OsString::from("DISPLAY"), OsString::from("vm-service:0"));
    env.insert(OsString::from("LC_ALL"), OsString::from("C"));

    let mut argv = vec!["ssh".to_string()];
    argv.extend(common(&path, true));
    argv.extend(options(&[
        "BatchMode=no",
        "PreferredAuthentications=password",
        "PubkeyAuthentication=no",
        "PasswordAuthentication=yes",
        "KbdInteractiveAuthentication=no",
        "NumberOfPasswordPrompts=1",
    ]));
    argv.push("-l".to_string());
    argv.push(user.to_string());
    argv.push("--".to_string());
    argv.push(ip.to_string());
    argv.push("/bin/sh".to_string());
    argv.push("-s".to_string());

    const RETRY_MESSAGES: [&str; 9] = [
        "connection refused",
        "connection timed out",
        "operation timed out",
        "no route to host",
        "network is unreachable",
        "connection closed",
        "connection reset",
        "connection aborted",
        "permission denied",
    ];

    loop {
        let remaining = deadline - clock.now();
        if remaining <= 0.0 {
            return Err(LeaseKeyError::BootstrapDeadlineExceeded);
        }
        let attempt = Duration::from_secs_f64(remaining.min(30.0));
        let mut retry;
        match transport.run(&argv, script.as_bytes(), &env, attempt) {
            Err(_) => return Err(LeaseKeyError::BootstrapFailed),
            Ok(RunResult::TimedOut) => retry = true,
            Ok(RunResult::Exited { code, stderr }) => {
                if code == 0 {
                    if clock.now() > deadline {
                        return Err(LeaseKeyError::BootstrapDeadlineExceeded);
                    }
                    return Ok(());
                }
                let error = String::from_utf8_lossy(&stderr).to_lowercase();
                retry = code == 255 && RETRY_MESSAGES.iter().any(|needle| error.contains(needle));
                // Host identity failures must never be reinterpreted as readiness.
                if error.contains("host key verification failed")
                    || error.contains("host identification has changed")
                {
                    retry = false;
                }
            }
        }
        if !retry {
            return Err(LeaseKeyError::BootstrapFailed);
        }
        let remaining = deadline - clock.now();
        if remaining <= 0.0 {
            return Err(LeaseKeyError::BootstrapDeadlineExceeded);
        }
        clock.sleep(remaining.min(2.0));
    }
}

/// Create the native `sh` askpass helper with 0700 permissions.
///
/// Deviation from Python: the helper is a POSIX `sh` script rather than a
/// `sys.executable` Python script. The name keeps the `.askpass-` prefix; the
/// `.sh` suffix is accepted by `cleanup` alongside the legacy `.py` pattern.
fn create_helper(dir: &Path) -> io::Result<PathBuf> {
    const HELPER: &str = "#!/bin/sh\nprintf '%s\\n' \"$MACHINE_SERVICE_SSH_PASSWORD\"\n";
    let mut named = tempfile::Builder::new()
        .prefix(".askpass-")
        .suffix(".sh")
        .tempfile_in(dir)?;
    named.write_all(HELPER.as_bytes())?;
    named.flush()?;
    // `fchmod` equivalent: independent of the process umask.
    named
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o700))?;
    named.into_temp_path().keep().map_err(|error| error.error)
}

// ---------------------------------------------------------------------------
// cleanup
// ---------------------------------------------------------------------------

/// Delete only an owned safe lease directory, AFTER the caller confirms VM
/// absence. An absent directory is a no-op; partial creation is removable.
pub fn cleanup(state_dir: &Path, vm: &str) -> Result<(), LeaseKeyError> {
    match cleanup_inner(state_dir, vm) {
        Ok(()) => Ok(()),
        Err(LeaseKeyError::Io(_)) => Err(LeaseKeyError::CleanupFailed),
        Err(error) => Err(error),
    }
}

fn is_askpass_name(name: &str) -> bool {
    let Some(middle) = name.strip_prefix(".askpass-") else {
        return false;
    };
    let Some(middle) = middle
        .strip_suffix(".py")
        .or_else(|| middle.strip_suffix(".sh"))
    else {
        return false;
    };
    !middle.is_empty()
        && middle
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn cleanup_inner(state_dir: &Path, vm: &str) -> Result<(), LeaseKeyError> {
    let path = location(state_dir, vm)?;
    let parent = match path.parent() {
        Some(parent) => parent.to_path_buf(),
        None => return Err(LeaseKeyError::InvalidKeyDirectory),
    };
    match check(&parent, &[0o700], true) {
        Ok(_) => {}
        Err(LeaseKeyError::Io(error)) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    }
    match check(&path, &[0o700], true) {
        Ok(_) => {}
        Err(LeaseKeyError::Io(error)) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    }
    let mut entries = Vec::new();
    for entry in fs::read_dir(&path).map_err(LeaseKeyError::Io)? {
        entries.push(entry.map_err(LeaseKeyError::Io)?.path());
    }
    for entry in &entries {
        let name = match entry.file_name().and_then(OsStr::to_str) {
            Some(name) => name,
            None => return Err(LeaseKeyError::UnexpectedEntry),
        };
        let modes: &[u32] =
            if name == "identity" || name == "known_hosts" || name == "known_hosts.old" {
                &[0o600]
            } else if name == "identity.pub" {
                &[0o600, 0o644]
            } else if is_askpass_name(name) {
                &[0o600, 0o700]
            } else {
                return Err(LeaseKeyError::UnexpectedEntry);
            };
        check(entry, modes, false)?;
    }
    for entry in &entries {
        fs::remove_file(entry).map_err(LeaseKeyError::Io)?;
    }
    fs::remove_dir(&path).map_err(LeaseKeyError::Io)?;
    Ok(())
}

#[cfg(test)]
mod tests;
