//! Shared process-level harness for the `machine-service` daemon test suites.
//!
//! The Python integration suites start the daemon in-process and replace
//! `tart`/`_ssh`/`_scp` with `mock.patch.object`. A Rust test cannot patch a
//! separate process, so this harness starts the real `machine-service` binary and
//! supplies fake `tart`, `ssh`, and `scp` executables through `PATH`, exactly
//! as the task requires. The observable seam is therefore the subprocess and
//! HTTP boundary rather than the in-process `Host` trait (which stays reserved
//! for the `machine-service-core` unit tests).
//!
//! Deviation from the Python fixtures (documented per suite): the Python
//! harness also stubbed `wait_ip`, `wait_ssh`, `bootstrap`, `verify_transfer`,
//! and `host_macos_guests` in-process. Here those run for real and are served
//! by the PATH shims, except `host_macos_guests`, which reads `/usr/bin/pgrep`
//! and cannot be shimmed. The tests that observe it assert the documented
//! "no host guests" value and are therefore host-environment dependent.

#![allow(dead_code)]

use std::cell::Cell;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// The daemon binary under test, provided by Cargo for this package.
pub const DAEMON: &str = env!("CARGO_BIN_EXE_machine-service");

// --------------------------------------------------------------------- HTTP

/// One parsed HTTP response.
#[derive(Debug, Clone)]
pub struct Response {
    /// The numeric status code.
    pub status: u16,
    /// The parsed JSON body (`{}` for an empty body).
    pub body: Value,
}

/// Issue one HTTP request to the daemon on `port`.
///
/// Mirrors the Python `LiveService.request` helper: JSON in, JSON out, and an
/// HTTP error status is a normal return value rather than an error.
pub fn http(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&Value>,
    fingerprint: Option<&str>,
) -> Response {
    try_http(port, method, path, body, fingerprint)
        .unwrap_or_else(|error| panic!("request {method} {path} on port {port}: {error}"))
}

/// One raw HTTP exchange with any body and headers, for the byte-carrying
/// endpoints (FD-4): the status, the headers (names lowercased) and the
/// body bytes.
#[allow(dead_code)] // Used by the suites that move bytes.
pub fn raw_http(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> (u16, Vec<(String, String)>, Vec<u8>) {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(180)))
        .expect("timeout");
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).expect("write head");
    stream.write_all(body).expect("write body");
    let mut answer = Vec::new();
    stream.read_to_end(&mut answer).expect("read answer");
    let split = answer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("header end");
    let head = String::from_utf8_lossy(&answer[..split]).into_owned();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse().ok())
        .expect("status");
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    (status, headers, answer[split + 4..].to_vec())
}

/// Like [`http`], but returns transport failures instead of panicking. Used by
/// the startup health poll, where connection-refused is expected.
pub fn try_http(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&Value>,
    fingerprint: Option<&str>,
) -> Result<Response, Box<dyn std::error::Error>> {
    try_http_timeout(
        port,
        method,
        path,
        body,
        fingerprint,
        Duration::from_secs(30),
    )
}

/// Like [`http`], but with an explicit socket read timeout.
///
/// The default 30 s suits the local daemon; a live E2E acquisition or exec may
/// legitimately take minutes, so callers there supply their own bound.
pub fn http_timeout(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&Value>,
    fingerprint: Option<&str>,
    read_timeout: Duration,
) -> Response {
    try_http_timeout(port, method, path, body, fingerprint, read_timeout)
        .unwrap_or_else(|error| panic!("request {method} {path} on port {port}: {error}"))
}

/// Like [`try_http`], but with an explicit socket read timeout.
pub fn try_http_timeout(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&Value>,
    fingerprint: Option<&str>,
    read_timeout: Duration,
) -> Result<Response, Box<dyn std::error::Error>> {
    let payload = body.map(|value| value.to_string()).unwrap_or_default();
    let mut request =
        format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n");
    if body.is_some() {
        request.push_str("Content-Type: application/json\r\n");
    }
    if let Some(fingerprint) = fingerprint {
        request.push_str(&format!("X-VM-Environment-Fingerprint: {fingerprint}\r\n"));
    }
    request.push_str(&format!("Content-Length: {}\r\n\r\n", payload.len()));
    request.push_str(&payload);

    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(read_timeout))?;
    stream.write_all(request.as_bytes())?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    if reader.read_line(&mut status_line)? == 0 {
        return Err("empty response".into());
    }
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| format!("malformed status line: {status_line:?}"))?;
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line)?;
        if read == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("content-length:") {
            content_length = value.trim().parse::<usize>().ok();
        }
        if let Some(value) = lower.strip_prefix("transfer-encoding:") {
            chunked = value.trim() == "chunked";
        }
    }
    // tiny_http sends bodies above 32 KiB chunked.
    let raw = if chunked {
        read_chunked(&mut reader)?
    } else {
        let mut raw = vec![0u8; content_length.unwrap_or(0)];
        reader.read_exact(&mut raw)?;
        raw
    };
    let body = if raw.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(&raw).unwrap_or_else(|error| {
            panic!(
                "parse response body: {error}: {}",
                String::from_utf8_lossy(&raw)
            )
        })
    };
    Ok(Response { status, body })
}

/// A chunked HTTP/1.1 body, without trailers.
fn read_chunked(reader: &mut impl BufRead) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut body = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Err("chunked body ended early".into());
        }
        let size = line.trim().split(';').next().unwrap_or_default();
        let size = usize::from_str_radix(size, 16)
            .map_err(|_| format!("malformed chunk size: {line:?}"))?;
        if size == 0 {
            // The trailer section ends with an empty line.
            loop {
                let mut trailer = String::new();
                if reader.read_line(&mut trailer)? == 0 || trailer.trim().is_empty() {
                    return Ok(body);
                }
            }
        }
        let start = body.len();
        body.resize(start + size, 0);
        reader.read_exact(&mut body[start..])?;
        let mut end = [0u8; 2];
        reader.read_exact(&mut end)?;
        if &end != b"\r\n" {
            return Err("chunk not followed by CRLF".into());
        }
    }
}

// ------------------------------------------------------------------- helpers

/// Allocate a free loopback port the way the Python `_free_port` helper does.
///
/// A process-wide set prevents two parallel tests from being handed the same
/// ephemeral port after the probe listener closes.
pub fn free_port() -> u16 {
    use std::collections::HashSet;
    use std::sync::Mutex;
    static USED: OnceLock<Mutex<HashSet<u16>>> = OnceLock::new();
    let used = USED.get_or_init(|| Mutex::new(HashSet::new()));
    loop {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral port");
        let port = listener.local_addr().expect("local addr").port();
        drop(listener);
        if used.lock().expect("used ports").insert(port) {
            return port;
        }
    }
}

/// Canonicalize a directory the way the Python fixture's `.resolve()` does.
///
/// macOS `/var` is a symlink and lease key creation intentionally rejects
/// symlink ancestors, so every fixture root goes through this.
pub fn canonical_dir(path: &Path) -> PathBuf {
    path.canonicalize()
        .unwrap_or_else(|error| panic!("canonicalize {}: {error}", path.display()))
}

/// Write an executable script and return its path.
pub fn write_exec(dir: &Path, name: &str, content: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).expect("shim dir");
    let path = dir.join(name);
    std::fs::write(&path, content).expect("write shim");
    let mut permissions = std::fs::metadata(&path)
        .expect("shim metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions).expect("chmod shim");
    path
}

/// Prepend `bin` to the ambient `PATH`.
pub fn path_with(bin: &Path) -> String {
    let base = std::env::var("PATH").unwrap_or_default();
    format!("{}:{}", bin.display(), base)
}

// -------------------------------------------------------------------- shims

/// The fake `tart` shim.
///
/// Mirrors `tests/integration/_daemon_main.py::FakeTart` closely enough for the
/// daemon's subprocess boundary: VM existence, running state, `ip`, and a
/// `run` that stays alive past the real boot-settle window (10 s). It also
/// records invocations so a test can prove that Tart was NOT called.
pub const TART_SHIM: &str = r#"#!/bin/sh
set -u
root="${TART_HOME:-}"
[ -n "$root" ] || root="${FAKE_ROOT:-/tmp}"
mkdir -p "$root"
state="$root/fake-vms"
printf '%s\n' "$*" >> "$root/tart-calls.log"
{
  printf '{"argv": ['
  sep=''
  for a in "$@"; do
    printf '%s"%s"' "$sep" "$a"
    sep=', '
  done
  printf '], "store": "%s", "exe": "%s"}\n' "$root" "$0"
} >> "$root/calls.jsonl"
set_running() {
  name="$1"; value="$2"
  tmp="$state.tmp"
  : > "$tmp"
  if [ -f "$state" ]; then
    while IFS='=' read -r n r; do
      [ -n "$n" ] || continue
      if [ "$n" = "$name" ]; then r="$value"; fi
      printf '%s=%s\n' "$n" "$r" >> "$tmp"
    done < "$state"
  fi
  if ! grep -q "^$name=" "$tmp" 2>/dev/null; then
    printf '%s=%s\n' "$name" "$value" >> "$tmp"
  fi
  mv "$tmp" "$state"
}
op="${1:-}"
shift 2>/dev/null || true
case "$op" in
  list)
    printf 'Name\tUUID\tArch\tDisk\tState\n'
    if [ -f "$state" ]; then
      while IFS='=' read -r n r; do
        [ -n "$n" ] || continue
        if [ "$r" = "1" ]; then s=running; else s=stopped; fi
        printf 'local  %s  100  11  aarch64  %s\n' "$n" "$s"
      done < "$state"
    fi
    printf 'local  pilot-macos26-base  100  11  aarch64  stopped\n'
    printf 'local  pilot-ubuntu-base  100  11  aarch64  stopped\n'
    ;;
  clone) set_running "$2" 0 ;;
  set) : ;;
  stop) set_running "$1" 0 ;;
  delete)
    if [ -f "$state" ]; then
      running=$(grep "^$1=" "$state" | head -n1 | cut -d= -f2)
      if [ "${running:-}" = "1" ]; then
        printf 'cannot delete running VM\n' >&2
        exit 1
      fi
      tmp="$state.tmp"; : > "$tmp"
      while IFS='=' read -r n r; do
        [ -n "$n" ] || continue
        [ "$n" = "$1" ] && continue
        printf '%s=%s\n' "$n" "$r" >> "$tmp"
      done < "$state"
      mv "$tmp" "$state"
    fi
    ;;
  ip) printf '192.168.64.99\n' ;;
  run) set_running "$1" 1; sleep 12 ;;
  *) : ;;
esac
exit 0
"#;

/// The fake `ssh` shim.
///
/// Mirrors `tests/integration/_daemon_main.py::fake_ssh` where the subprocess
/// boundary permits: the secrets probe answers `OK`, any stdin script answers
/// `script-ran`, `true` answers empty success, a remote command containing
/// `false` exits 1, and anything else echoes `ran: <remote>`. The isolation
/// probe of an `internet` lease (DC-14) prints the gateway and
/// `FAKE_PROBE_RESULT`, `timeout` by default, and its egress check (DC-16)
/// prints `FAKE_EGRESS_ADDRESS` and HTTPS 200. A remote command
/// containing `fixture-sleep` replaces the shim with `sleep 60`, standing in
/// for a guest command that runs far longer than any test.
///
/// Deviation (forced): the Python fake returned `str(timeout)` for the
/// `fixture-timeout` sentinel because it received the timeout as an argument.
/// An external `ssh` never sees the deadline, so this shim cannot echo it; the
/// exact deadline is asserted at the `Host` seam by `machine-service-core`'s
/// `exec_timeout` suite.
pub const SSH_SHIM: &str = r#"#!/bin/sh
set -u
root="${FAKE_ROOT:-${TART_HOME:-/tmp}}"
mkdir -p "$root"
printf '%s\n' "$*" >> "$root/ssh-calls.log"
case "$*" in
  *fixture-sleep*) exec sleep 60 ;;
  *"vm-service isolation probe"*)
    printf 'gateway=192.168.2.1 result=%s\n' "${FAKE_PROBE_RESULT:-timeout}"
    exit 0 ;;
  *"vm-service egress check"*)
    printf 'egress name=example.com address=%s https=200\n' "${FAKE_EGRESS_ADDRESS:-104.20.23.154}"
    exit 0 ;;
esac
last=""
for arg in "$@"; do last="$arg"; done
input=""
if [ ! -t 0 ]; then
  input=$(cat 2>/dev/null || true)
fi
if [ -n "$input" ]; then
  printf '%s\n' "$input" >> "$root/ssh-stdin.log"
fi
case "$*" in
  *"test -s ~/.config/zsh/secrets.zsh"*) printf 'OK\n'; exit 0 ;;
esac
if [ -n "$input" ]; then
  printf 'script-ran\n'
  exit 0
fi
if [ "$last" = "true" ]; then
  exit 0
fi
case "$*" in
  *false*) printf 'false: failed\n' >&2; exit 1 ;;
esac
printf 'ran: %s\n' "$last"
exit 0
"#;

/// The fake `scp` shim.
///
/// The last two arguments are source and destination. A destination with a
/// remote prefix (`user@host:path`) stores the source bytes; a source with a
/// remote prefix restores them; a folder is copied whole, as `scp -r`
/// does. This is what makes the daemon's real
/// `verify_transfer` checksum check pass without a guest. A path containing
/// `fixture-sleep` replaces the shim with `sleep 60`, standing in for a
/// transfer that runs far longer than any test.
pub const SCP_SHIM: &str = r#"#!/bin/sh
set -u
root="${FAKE_ROOT:-${TART_HOME:-/tmp}}"
store="$root/scp-store"
mkdir -p "$store"
printf '%s\n' "$*" >> "$root/scp-calls.log"
case "$*" in
  *fixture-sleep*) exec sleep 60 ;;
esac
src=""; dst=""
for arg in "$@"; do
  src="$dst"; dst="$arg"
done
case "$dst" in
  *:*)
    remote="${dst#*:}"
    key=$(printf '%s' "$remote" | tr '/' '_')
    rm -rf "$store/$key"
    cp -R -- "$src" "$store/$key" 2>/dev/null || cp -R "$src" "$store/$key"
    ;;
  *)
    remote="${src#*:}"
    key=$(printf '%s' "$remote" | tr '/' '_')
    cp -R -- "$store/$key" "$dst" 2>/dev/null || cp -R "$store/$key" "$dst"
    ;;
esac
exit 0
"#;

/// Install the three shims into `bin`, returning their directory.
pub fn install_shims(bin: &Path) {
    write_exec(bin, "tart", TART_SHIM);
    write_exec(bin, "ssh", SSH_SHIM);
    write_exec(bin, "scp", SCP_SHIM);
}

// ------------------------------------------------------------------- daemon

/// A running daemon subprocess.
pub struct Daemon {
    child: Option<Child>,
    port: u16,
    stderr_path: PathBuf,
    stdout_path: PathBuf,
}

impl Daemon {
    /// Spawn the daemon with `arguments` and extra environment overrides.
    pub fn start(root: &Path, port: u16, arguments: &[&str], env: &[(String, String)]) -> Daemon {
        Self::try_start(root, port, arguments, env)
            .unwrap_or_else(|error| panic!("daemon did not start on port {port}: {error}"))
    }

    /// Spawn and health-check the daemon, returning an error instead of
    /// panicking so a caller can retry on a different port.
    ///
    /// Port collisions are possible because the fixture reserves a port by
    /// binding and closing a probe listener before the daemon binds it; an
    /// unrelated loopback connection can claim the same ephemeral port in the
    /// window. Callers retry with a fresh port.
    pub fn try_start(
        root: &Path,
        port: u16,
        arguments: &[&str],
        env: &[(String, String)],
    ) -> Result<Daemon, String> {
        std::fs::create_dir_all(root).map_err(|error| error.to_string())?;
        let stdout_path = root.join(format!("daemon-{port}.out"));
        let stderr_path = root.join(format!("daemon-{port}.err"));
        let stdout = std::fs::File::create(&stdout_path).map_err(|error| error.to_string())?;
        let stderr = std::fs::File::create(&stderr_path).map_err(|error| error.to_string())?;
        let mut command = Command::new(DAEMON);
        command
            .args(arguments)
            .envs(env.iter().cloned())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        let child = command.spawn().map_err(|error| error.to_string())?;
        let mut daemon = Daemon {
            child: Some(child),
            port,
            stderr_path,
            stdout_path,
        };
        match daemon.wait_health(Duration::from_secs(30)) {
            Ok(()) => Ok(daemon),
            Err(error) => {
                daemon.stop();
                Err(error)
            }
        }
    }

    /// The bound port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The daemon's process ID while it runs.
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }

    /// The captured stderr, for diagnostics.
    pub fn stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr_path).unwrap_or_default()
    }

    fn wait_health(&mut self, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(child) = self.child.as_mut() {
                if let Ok(Some(status)) = child.try_wait() {
                    return Err(format!(
                        "daemon exited early with {status}: stderr:\n{}",
                        self.stderr()
                    ));
                }
            }
            if let Ok(response) = try_http(self.port, "GET", "/health", None, None) {
                if response.status == 200 && response.body.get("ok") == Some(&Value::Bool(true)) {
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "daemon did not come up; stderr:\n{}",
                    self.stderr()
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Terminate the daemon (SIGTERM, then SIGKILL), mirroring the Python
    /// `LiveService.__exit__`.
    pub fn stop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        // SAFETY: `kill` takes a process id owned by this handle.
        unsafe {
            libc::kill(child.id() as i32, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return;
                }
            }
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop();
    }
}

// -------------------------------------------------------------------- machctl

/// Locate the `machctl` binary, building it on demand for `cargo test -p
/// machine-service`, which does not build sibling packages.
pub fn machctl_bin() -> &'static PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| sibling_bin("machctl", "machctl"))
}

/// Locate the `guest-session-agent` binary, building it on demand as
/// [`machctl_bin`] builds `machctl`.
pub fn guest_agent_bin() -> &'static PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| sibling_bin("guest-session", "guest-session-agent"))
}

/// Locate the `relay-agent` binary, building it on demand as
/// [`machctl_bin`] builds `machctl`.
pub fn relay_agent_bin() -> &'static PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| sibling_bin("relay-agent", "relay-agent"))
}

/// A relay agent release in `folder`, in the form `build-relay-agent.sh`
/// makes, whose archives for this host's macOS and Linux platforms hold
/// `executable` (DC-140). Answers the folder.
pub fn write_relay_release(folder: &Path, executable: &Path) -> PathBuf {
    use machine_service_core::service_layout::release;
    let _ = std::fs::remove_dir_all(folder);
    std::fs::create_dir_all(folder).unwrap();
    let stage = tempfile::tempdir().unwrap();
    std::fs::copy(executable, stage.path().join(release::MEMBER)).unwrap();
    let mut sums = String::new();
    for kind in ["macos", "linux"] {
        let file = format!("relay-agent-0.0.0-{}.tar.gz", release::platform_for(kind));
        let status = Command::new("/usr/bin/tar")
            .arg("-C")
            .arg(stage.path())
            .arg("-czf")
            .arg(folder.join(&file))
            .arg(release::MEMBER)
            .status()
            .unwrap();
        assert!(status.success());
        let bytes = std::fs::read(folder.join(&file)).unwrap();
        sums.push_str(&format!("{}  {file}\n", release::sha256_hex(&bytes)));
    }
    std::fs::write(folder.join(release::SUMS), sums).unwrap();
    folder.to_path_buf()
}

/// Build `binary` of `package` into the daemon's target directory unless it
/// is already there.
fn sibling_bin(package: &str, binary: &str) -> PathBuf {
    let daemon = PathBuf::from(DAEMON);
    let binary_dir = daemon
        .parent()
        .expect("daemon binary has a parent directory");
    let candidate = binary_dir.join(binary);
    if candidate.is_file() {
        return candidate;
    }
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf();
    let target_dir = binary_dir.parent().expect("profile directory has a parent");
    let status = Command::new("cargo")
        .args(["build", "-p", package, "--bin", binary])
        .current_dir(&workspace)
        .env("CARGO_TARGET_DIR", target_dir)
        .status()
        .unwrap_or_else(|error| panic!("build {binary}: {error}"));
    assert!(
        status.success(),
        "cargo build -p {package} --bin {binary} failed"
    );
    assert!(
        candidate.is_file(),
        "{binary} not found at {} after building",
        candidate.display()
    );
    candidate
}

/// Run `machctl` with the given arguments and extra environment overrides.
///
/// `CONSUL_HTTP_ADDR` points at a closed loopback port unless the caller
/// overrides it, so the CLI's discovery (FD-2) never reads the real Consul
/// agent or reaches an instance it lists; the CLI then lists only the
/// fixture's daemon.
pub fn run_machctl(arguments: &[&str], env: &[(String, String)]) -> Output {
    let mut command = Command::new(machctl_bin());
    command
        .args(arguments)
        .env_remove("VM_ENVIRONMENT_FILE")
        .env_remove("VM_ENVIRONMENT_FINGERPRINT")
        // The CLI remembers its leases under the state folder (CL-3); only a
        // caller's own value may place them.
        .env_remove("MACHINE_SERVICE_STATE")
        .env("CONSUL_HTTP_ADDR", format!("127.0.0.1:{}", free_port()))
        .envs(env.iter().cloned())
        .stdin(Stdio::null());
    command.output().expect("run machctl")
}

// ---------------------------------------------------------- legacy fixture

use sha2::{Digest, Sha256};
use std::os::unix::fs::MetadataExt;
use tempfile::TempDir;

/// Hex-encoded SHA-256, matching Python `hashlib.sha256(...).hexdigest()`.
pub fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let digest = hasher.finalize();
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn stat_json(path: &Path) -> Value {
    let meta =
        std::fs::metadata(path).unwrap_or_else(|error| panic!("stat {}: {error}", path.display()));
    json!({
        "st_dev": meta.dev(),
        "st_ino": meta.ino(),
        "st_size": meta.size(),
        "st_mtime_ns": meta.mtime() * 1_000_000_000 + meta.mtime_nsec(),
    })
}

/// One isolated legacy-mode fixture: temp state root, a pilot-images tree that
/// produces the same image metadata as `tests/integration/_daemon_main.py`, the
/// default credential pack, real lease-key generation, and the PATH shims.
pub struct LegacyFixture {
    /// Owns the temporary directory for the lifetime of the test.
    pub dir: TempDir,
    /// Canonical fixture root.
    pub root: PathBuf,
    /// The fake `HOME`, which holds the credential pack and the Tart store.
    pub home: PathBuf,
    /// `MACHINE_SERVICE_STATE`.
    pub state_dir: PathBuf,
    /// `PILOT_REPO`.
    pub pilot: PathBuf,
    /// `PILOT_IMAGES_STATE_DIR`.
    pub pilot_state: PathBuf,
    /// `TART_HOME`.
    pub tart_home: PathBuf,
    /// Directory holding the `tart`/`ssh`/`scp` shims.
    pub bin: PathBuf,
    /// The port the daemon is currently bound to; retried on collision.
    port: Cell<u16>,
}

impl LegacyFixture {
    /// Build the fixture, mirroring the Python fixture's on-disk layout.
    pub fn new() -> LegacyFixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = canonical_dir(dir.path());
        let home = root.join("state/home");
        let state_dir = root.join("state");
        let pilot = state_dir.join("pilot-images");
        let pilot_state = state_dir.join("pilot-state");
        let tart_home = home.join(".tart");
        let bin = root.join("bin");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::create_dir_all(&state_dir).expect("state");
        std::fs::create_dir_all(pilot.join("images")).expect("pilot images");
        std::fs::create_dir_all(tart_home.join("vms")).expect("tart vms");
        std::fs::create_dir_all(&bin).expect("bin");

        let pack = home.join(".config/vm-credentials/default");
        std::fs::create_dir_all(&pack).expect("pack");
        std::fs::write(
            pack.join("env.extra"),
            "export MACHINE_SERVICE_TEST_ONLY=1\n",
        )
        .expect("env.extra");

        for (image, conf) in [
            (
                "macos26",
                "LINE_KIND=macos\nBASE_VM=pilot-macos26-base\nCLONE_PREFIX=pilot-mac-\nGUEST_USER=station\nGUEST_PASS=station\n",
            ),
            (
                "ubuntu2404",
                "LINE_KIND=linux\nBASE_VM=pilot-ubuntu-base\nCLONE_PREFIX=pilot-\nGUEST_USER=admin\nGUEST_PASS=admin\nWORK_VM=pilot-ubuntu-work\n",
            ),
        ] {
            let directory = pilot.join("images").join(image);
            std::fs::create_dir_all(&directory).expect("image dir");
            std::fs::write(directory.join("line.conf"), conf).expect("line.conf");
        }

        for name in ["pilot-ubuntu-base", "pilot-ubuntu-work"] {
            let base = tart_home.join("vms").join(name);
            std::fs::create_dir_all(&base).expect("base dir");
            std::fs::write(base.join("config.json"), b"fixture").expect("config.json");
            std::fs::write(base.join("disk.img"), b"fixture").expect("disk.img");
        }

        // The work VM must be visible to `tart list` and stopped.
        std::fs::write(tart_home.join("fake-vms"), "pilot-ubuntu-work=0\n").expect("fake-vms");

        let inventory = json!({
            "schemaVersion": 1,
            "os": "linux",
            "architecture": "arm64",
            "collectedAt": "2026-09-16T00:00:00Z",
            "sources": [{"id": "dpkg", "status": "available"}],
            "applications": [{"id": "fixture", "name": "Fixture App", "aliases": [], "version": null}],
        });
        let raw = json!({
            "schemaVersion": 1,
            "image": "ubuntu2404",
            "inventory": inventory,
            "provenance": {
                "extractionMode": "work",
                "evidenceId": "fixture",
                "rawSha256": "0".repeat(64),
                "collectorSha256": "1".repeat(64),
                "aliasesSha256": "2".repeat(64),
            },
        });
        let raw_bytes = serde_json::to_vec(&raw).expect("inventory json");
        std::fs::write(
            pilot.join("images/ubuntu2404/applications.json"),
            &raw_bytes,
        )
        .expect("applications.json");

        let base = tart_home.join("vms/pilot-ubuntu-base");
        let canonical_base = canonical_dir(&base);
        let association = json!({
            "schemaVersion": 2,
            "image": "ubuntu2404",
            "base": {
                "base_vm": "pilot-ubuntu-base",
                "path": canonical_base.to_string_lossy(),
                "files": {
                    "config.json": stat_json(&base.join("config.json")),
                    "disk.img": stat_json(&base.join("disk.img")),
                },
            },
            "inventorySha256": sha256_hex(&raw_bytes),
        });
        let namespace = sha256_hex(
            canonical_dir(&tart_home.join("vms"))
                .to_string_lossy()
                .as_bytes(),
        );
        let association_path = pilot_state
            .join("stores")
            .join(&namespace)
            .join("base/ubuntu2404.json");
        std::fs::create_dir_all(association_path.parent().expect("association parent"))
            .expect("association dir");
        std::fs::write(
            &association_path,
            serde_json::to_vec(&association).expect("association json"),
        )
        .expect("association");

        install_shims(&bin);

        LegacyFixture {
            dir,
            root,
            home,
            state_dir,
            pilot,
            pilot_state,
            tart_home,
            bin,
            port: Cell::new(0),
        }
    }

    /// The port the daemon is currently bound to.
    pub fn port(&self) -> u16 {
        self.port.get()
    }

    /// Environment overrides for the daemon and for `machctl`.
    pub fn env(&self) -> Vec<(String, String)> {
        vec![
            (
                "MACHINE_SERVICE_STATE".to_string(),
                self.state_dir.display().to_string(),
            ),
            ("MACHINE_SERVICE_PORT".to_string(), self.port().to_string()),
            ("PILOT_REPO".to_string(), self.pilot.display().to_string()),
            (
                "PILOT_IMAGES_STATE_DIR".to_string(),
                self.pilot_state.display().to_string(),
            ),
            (
                "TART_HOME".to_string(),
                self.tart_home.display().to_string(),
            ),
            (
                "FAKE_ROOT".to_string(),
                self.tart_home.display().to_string(),
            ),
            ("HOME".to_string(), self.home.display().to_string()),
            ("PATH".to_string(), path_with(&self.bin)),
        ]
    }

    /// The daemon environment without the fixture's `HOME`/`TART_HOME`
    /// overrides, for tests that assert the environment is not consulted.
    ///
    /// Retries on a fresh port if the reserved one was claimed in the window
    /// between the probe bind and the daemon's bind.
    pub fn start_daemon(&self) -> Daemon {
        for _ in 0..16 {
            let port = free_port();
            self.port.set(port);
            if let Ok(daemon) = Daemon::try_start(&self.root, port, &[], &self.env()) {
                return daemon;
            }
        }
        panic!("could not start the daemon on a free port");
    }

    /// Start the daemon with `extra` environment overrides added to
    /// [`LegacyFixture::env`], retrying on a fresh port like
    /// [`LegacyFixture::start_daemon`].
    pub fn start_daemon_with(&self, extra: &[(String, String)]) -> Daemon {
        for _ in 0..16 {
            let port = free_port();
            self.port.set(port);
            let mut env = self.env();
            env.extend(extra.iter().cloned());
            if let Ok(daemon) = Daemon::try_start(&self.root, port, &[], &env) {
                return daemon;
            }
        }
        panic!("could not start the daemon on a free port");
    }

    /// `machctl` environment: the port plus the fixture HOME (never the ambient
    /// one, so credential lookup cannot see operator state).
    pub fn machctl_env(&self) -> Vec<(String, String)> {
        vec![
            ("MACHINE_SERVICE_PORT".to_string(), self.port().to_string()),
            ("HOME".to_string(), self.home.display().to_string()),
        ]
    }

    /// The `state_dir/state.json` path.
    pub fn state_file(&self) -> PathBuf {
        self.state_dir.join("state.json")
    }

    /// The fake Tart invocation log.
    pub fn tart_calls(&self) -> String {
        std::fs::read_to_string(self.tart_home.join("tart-calls.log")).unwrap_or_default()
    }

    /// The `ssh` invocation log.
    pub fn ssh_calls(&self) -> String {
        std::fs::read_to_string(self.tart_home.join("ssh-calls.log")).unwrap_or_default()
    }

    /// The `ssh` standard-input log, which records guest scripts.
    pub fn ssh_stdin(&self) -> String {
        std::fs::read_to_string(self.tart_home.join("ssh-stdin.log")).unwrap_or_default()
    }

    /// Read the `vms` map from the daemon's `state.json`.
    pub fn read_state(&self) -> serde_json::Map<String, Value> {
        let text = std::fs::read_to_string(self.state_file()).expect("read state");
        let value: Value = serde_json::from_str(&text).expect("parse state");
        value
            .get("vms")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default()
    }

    /// Whether the daemon holds no leases. The real daemon's GC loop persists
    /// an empty `state.json` at startup, so file absence is not an observable
    /// property at the process boundary.
    pub fn vms_empty(&self) -> bool {
        match std::fs::read_to_string(self.state_file()) {
            Ok(text) => serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|value| value.get("vms").and_then(Value::as_object).cloned())
                .map(|vms| vms.is_empty())
                .unwrap_or(true),
            Err(_) => true,
        }
    }

    /// Issue an HTTP request against this fixture's daemon.
    pub fn request(&self, method: &str, path: &str, body: Option<&Value>) -> Response {
        http(self.port(), method, path, body, None)
    }
}
