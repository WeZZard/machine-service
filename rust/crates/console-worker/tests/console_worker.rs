//! Port of `tests/unit/test_console_worker.py`.
//!
//! Synthetic SSH guests and viewers are shell scripts, so no test needs a real
//! VM, a real `tart`, or a real viewer. The one exception is the guest agent's
//! own parser, which belongs to the `guest-console-agent` crate.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::ops::{Deref, DerefMut};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::{AsRawFd, IntoRawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use console_worker::{
    default_launcher, monotonic, real_launch, Launcher, State, Worker, MAX_COMMAND,
};
use serde_json::{json, Value};
use tempfile::TempDir;

/// The synthetic guest: consumes exactly one JSON configuration line, then
/// replays binary RFB on the same stdin/stdout.
const ECHO_SCRIPT: &str = "#!/bin/sh\nIFS= read -r line\nprintf 'RFB 003.008\\n'\ncat\n";

fn wall_time() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn write_exec(dir: &Path, name: &str, content: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, content).unwrap();
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&path, permissions).unwrap();
    path
}

fn octal(byte: u8) -> String {
    format!("\\{:03o}", byte)
}

fn read_exact(stream: &mut TcpStream, size: usize) -> Vec<u8> {
    let mut buffer = vec![0u8; size];
    let mut received = 0;
    // A loaded machine can delay data past the socket's read timeout, which
    // surfaces as `WouldBlock`/`TimedOut` rather than EOF. Retry those against
    // an overall deadline so scheduling jitter cannot masquerade as a failure.
    let deadline = Instant::now() + Duration::from_secs(30);
    while received < size {
        match stream.read(&mut buffer[received..]) {
            Ok(0) => panic!("unexpected EOF"),
            Ok(n) => received += n,
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                assert!(
                    Instant::now() < deadline,
                    "timed out after {received} of {size} bytes"
                );
            }
            Err(e) => panic!("read failed: {e}"),
        }
    }
    buffer
}

/// Asserts the peer closed the connection: `Ok(0)`, never a timeout. Python's
/// `recv` raises on timeout, so coercing the error to `0` would accept a
/// stalled connection as an orderly close and weaken the assertion.
fn assert_eof(stream: &mut TcpStream) {
    let mut byte = [0u8; 1];
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match stream.read(&mut byte) {
            Ok(0) => return,
            Ok(_) => panic!("expected EOF, received data"),
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                assert!(Instant::now() < deadline, "no EOF before deadline");
            }
            Err(e) => panic!("read failed: {e}"),
        }
    }
}

fn wait_until<F: Fn() -> bool>(predicate: F, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        thread::sleep(Duration::from_millis(5));
    }
    predicate()
}

fn process_gone(pid: i32) -> bool {
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    rc == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

/// Serializes tests. The Python suite is `unittest`-sequential and its short
/// lifetime bounds (0.25-0.35 s) assume no competing load; running the ported
/// tests one at a time reproduces that.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial_guard() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A control channel: one JSON object per line, plus recorded events.
struct Control {
    socket: Option<UnixStream>,
    buffer: Vec<u8>,
    events: Vec<Value>,
}

impl Control {
    fn new(socket: UnixStream) -> Self {
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        Control {
            socket: Some(socket),
            buffer: Vec::new(),
            events: Vec::new(),
        }
    }

    fn send(&mut self, value: Value) {
        let mut bytes = serde_json::to_vec(&value).unwrap();
        bytes.push(b'\n');
        self.send_raw(&bytes);
    }

    fn send_raw(&mut self, bytes: &[u8]) {
        self.socket
            .as_mut()
            .expect("control socket open")
            .write_all(bytes)
            .unwrap();
    }

    fn close(&mut self) {
        self.socket = None;
    }

    fn until(&mut self, op: &str) -> Value {
        loop {
            while !self.buffer.contains(&b'\n') {
                let mut chunk = [0u8; 65536];
                let n = self
                    .socket
                    .as_mut()
                    .expect("control socket open")
                    .read(&mut chunk)
                    .unwrap();
                if n == 0 {
                    panic!("EOF before {op}");
                }
                self.buffer.extend_from_slice(&chunk[..n]);
            }
            let position = self.buffer.iter().position(|&b| b == b'\n').unwrap();
            let line: Vec<u8> = self.buffer.drain(..=position).collect();
            let event: Value = serde_json::from_slice(&line[..line.len() - 1]).unwrap();
            self.events.push(event.clone());
            if event.get("op").and_then(|v| v.as_str()) == Some(op) {
                return event;
            }
        }
    }

    fn events_json(&self) -> String {
        serde_json::to_string(&Value::Array(self.events.clone())).unwrap()
    }

    fn connected_count(&self) -> usize {
        self.events
            .iter()
            .filter(|e| e.get("connected").and_then(|v| v.as_bool()) == Some(true))
            .count()
    }
}

/// Kills the child on drop so a failing test cannot leak a worker.
struct ChildGuard(Child);

impl Deref for ChildGuard {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.0
    }
}

impl DerefMut for ChildGuard {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

fn wait_timeout(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Spawns the real `console-worker` binary with an inherited control descriptor.
fn spawn_process() -> (Control, ChildGuard) {
    let (parent, child) = UnixStream::pair().unwrap();
    let child_fd = child.as_raw_fd();
    // Rust sets CLOEXEC on its own descriptors; clear it for the inherited end.
    unsafe {
        libc::fcntl(child_fd, libc::F_SETFD, 0);
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_console-worker"));
    command.arg("--control-fd").arg(child_fd.to_string());
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let process = command.spawn().unwrap();
    drop(child);
    (Control::new(parent), ChildGuard(process))
}

fn threaded(launcher: Launcher) -> (Control, Arc<Mutex<State>>, JoinHandle<String>) {
    let (parent, child) = UnixStream::pair().unwrap();
    let fd = child.into_raw_fd();
    let worker = Worker::new(fd, launcher);
    let state = worker.state_arc();
    let handle = thread::spawn(move || worker.run());
    (Control::new(parent), state, handle)
}

fn threaded_default() -> (Control, Arc<Mutex<State>>, JoinHandle<String>) {
    threaded(default_launcher())
}

/// A latch usable from launcher closures.
struct Gate {
    lock: Mutex<bool>,
    condvar: Condvar,
}

impl Gate {
    fn new() -> Self {
        Gate {
            lock: Mutex::new(false),
            condvar: Condvar::new(),
        }
    }

    fn set(&self) {
        *self.lock.lock().unwrap() = true;
        self.condvar.notify_all();
    }

    fn wait(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut value = self.lock.lock().unwrap();
        while !*value {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let (next, _) = self.condvar.wait_timeout(value, deadline - now).unwrap();
            value = next;
        }
        true
    }
}

struct Fixture {
    dir: TempDir,
}

impl Fixture {
    fn new() -> Self {
        Fixture {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn dir(&self) -> &Path {
        self.dir.path()
    }

    fn guest_echo(&self) -> PathBuf {
        write_exec(self.dir(), "guest.sh", ECHO_SCRIPT)
    }

    fn viewer_default(&self) -> PathBuf {
        write_exec(self.dir(), "viewer", "read password\nexec sleep 30\n")
    }

    fn viewer_capture(&self, marker: &Path) -> PathBuf {
        let content = format!(
            "read password\nprintf \"%s\\n\" \"$password\" > \"{}\"\nexec sleep 30\n",
            marker.display()
        );
        write_exec(self.dir(), "viewer-capture", &content)
    }

    fn config(&self, guest: &Path, viewer: &Path, ttl: f64) -> Value {
        json!({
            "op": "configure",
            "generation": 0,
            "deadline": monotonic() + ttl,
            "ssh_argv": [guest.to_str().unwrap()],
            "guest_config": {
                "version": 1,
                "session": {
                    "id": "desktop",
                    "uid": 1000,
                    "user": "tester",
                    "display": ":0",
                    "xauthority": "/home/tester/.Xauthority"
                },
                "expires_at": wall_time() + 3600.0,
                "password": "pR1v@te!"
            },
            "viewer": {"kind": "turbovnc", "executable": viewer.to_str().unwrap()}
        })
    }
}

#[test]
fn test_ttl_real_process_and_sanitized_events() {
    let _serial = serial_guard();
    let fixture = Fixture::new();
    let guest = fixture.guest_echo();
    let viewer = fixture.viewer_default();
    let (mut control, mut process) = spawn_process();
    control.send(fixture.config(&guest, &viewer, 0.25));
    assert_eq!(control.until("ready"), json!({"op": "ready"}));
    control.send(json!({"op": "open"}));
    assert_eq!(
        control.until("launched")["authentication"],
        "password_supplied"
    );
    assert_eq!(control.until("closed")["reason"], "expired");
    assert!(wait_timeout(&mut process, Duration::from_secs(2)).is_some());
    assert!(!control.events_json().contains("pR1v@te!"));
    let mut out = Vec::new();
    let mut err = Vec::new();
    if let Some(mut stdout) = process.stdout.take() {
        stdout.read_to_end(&mut out).unwrap();
    }
    if let Some(mut stderr) = process.stderr.take() {
        stderr.read_to_end(&mut err).unwrap();
    }
    assert_eq!((out, err), (Vec::new(), Vec::new()));
}

#[test]
fn test_eof_real_process() {
    let _serial = serial_guard();
    let fixture = Fixture::new();
    let guest = fixture.guest_echo();
    let viewer = fixture.viewer_default();
    let (mut control, mut process) = spawn_process();
    control.send(fixture.config(&guest, &viewer, 30.0));
    control.until("ready");
    control.send(json!({"op": "open"}));
    control.until("launched");
    control.close();
    let status = wait_timeout(&mut process, Duration::from_secs(2)).expect("worker exits");
    assert_eq!(status.code(), Some(0));
}

#[test]
fn test_stale_generation_cannot_extend() {
    let _serial = serial_guard();
    let fixture = Fixture::new();
    let guest = fixture.guest_echo();
    let viewer = fixture.viewer_default();
    let (mut control, mut process) = spawn_process();
    // The stale renewal must reach the worker before the session expires,
    // or the worker closes first and the test reads end-of-file. A short
    // wait overshoots on a busy host, so the session gets 2 s, not 0.25 s.
    control.send(fixture.config(&guest, &viewer, 2.0));
    control.until("ready");
    control.send(json!({
        "op": "renew",
        "generation": 0,
        "deadline": monotonic() + 20.0
    }));
    assert_eq!(control.until("error")["reason"], "stale_generation");
    assert_eq!(control.until("closed")["reason"], "expired");
    assert!(wait_timeout(&mut process, Duration::from_secs(2)).is_some());
}

#[test]
fn test_new_generation_renews() {
    let _serial = serial_guard();
    let fixture = Fixture::new();
    let guest = fixture.guest_echo();
    let viewer = fixture.viewer_default();
    let (mut control, state, handle) = threaded_default();
    control.send(fixture.config(&guest, &viewer, 0.3));
    control.until("ready");
    let deadline = monotonic() + 1.0;
    control.send(json!({"op": "renew", "generation": 1, "deadline": deadline}));
    control.until("renewed");
    assert_eq!(state.lock().unwrap().deadline(), deadline);
    control.send(json!({"op": "cancel"}));
    assert_eq!(control.until("closed")["reason"], "cancelled");
    handle.join().unwrap();
}

#[test]
fn test_invalid_and_oversized_controls() {
    let _serial = serial_guard();
    let payloads: Vec<Vec<u8>> = vec![
        b"{invalid\n".to_vec(),
        b"[]\n".to_vec(),
        vec![b'x'; MAX_COMMAND + 1],
        b"{\"op\":\"configure\",\"password\":\"private-marker\"}\n".to_vec(),
    ];
    for payload in payloads {
        let (mut control, mut process) = spawn_process();
        control.send_raw(&payload);
        assert_eq!(control.until("closed")["reason"], "invalid_command");
        assert!(!control.events_json().contains("private-marker"));
        assert!(wait_timeout(&mut process, Duration::from_secs(2)).is_some());
    }
}

#[test]
fn test_blocked_launcher_does_not_delay_revocation_and_late_child_killed() {
    let _serial = serial_guard();
    let fixture = Fixture::new();
    let guest = fixture.guest_echo();
    let viewer = fixture.viewer_default();
    let gate = Arc::new(Gate::new());
    let entered = Arc::new(Gate::new());
    let finished = Arc::new(Gate::new());
    let pids = Arc::new(Mutex::new(Vec::<i32>::new()));
    let launcher: Launcher = {
        let gate = gate.clone();
        let entered = entered.clone();
        let finished = finished.clone();
        let pids = pids.clone();
        Arc::new(move |_argv: &[String], piped: bool| {
            entered.set();
            gate.wait(Duration::from_secs(3));
            let argv = vec!["/bin/sleep".to_string(), "30".to_string()];
            let child = real_launch(&argv, piped)?;
            pids.lock().unwrap().push(child.id() as i32);
            finished.set();
            Ok(child)
        })
    };
    let (mut control, state, handle) = threaded(launcher);
    control.send(fixture.config(&guest, &viewer, 0.25));
    control.until("ready");
    control.send(json!({"op": "open"}));
    assert!(entered.wait(Duration::from_secs(1)));
    let port = state.lock().unwrap().listener_addr().unwrap();
    assert_eq!(control.until("closed")["reason"], "expired");
    handle.join().unwrap();
    assert!(TcpStream::connect_timeout(&port, Duration::from_millis(200)).is_err());
    gate.set();
    assert!(finished.wait(Duration::from_secs(1)));
    assert!(wait_until(
        || state.lock().unwrap().pending() == 0,
        Duration::from_secs(2)
    ));
    let pid = pids.lock().unwrap()[0];
    assert!(process_gone(pid));
}

/// `test_revocation_scrubs_retained_secrets_and_tracks_late_cleanup` port.
///
/// After `cancel`, the retained config must be gone, the password removed from
/// the captured `guest_config`, the launch payload and control input wiped, and
/// the still-blocked launcher must be joined once it returns (pending -> 0).
/// Python's `assertFalse(worker.launch_threads[0].daemon)` is represented by
/// the launch being a joinable `thread::spawn` handle (`join_launch_threads`),
/// since Rust has no daemon-thread flag.
#[test]
fn test_revocation_scrubs_retained_secrets_and_tracks_late_cleanup() {
    let _serial = serial_guard();
    let fixture = Fixture::new();
    let guest = fixture.guest_echo();
    let viewer = fixture.viewer_default();
    let gate = Arc::new(Gate::new());
    let entered = Arc::new(Gate::new());
    let launcher: Launcher = {
        let gate = gate.clone();
        let entered = entered.clone();
        Arc::new(move |_argv: &[String], piped: bool| {
            entered.set();
            gate.wait(Duration::from_secs(3));
            let argv = vec!["/bin/sleep".to_string(), "30".to_string()];
            real_launch(&argv, piped)
        })
    };
    let (mut control, state, handle) = threaded(launcher);
    control.send(fixture.config(&guest, &viewer, 2.0));
    control.until("ready");
    let stored = state
        .lock()
        .unwrap()
        .config_arc()
        .expect("config retained before revocation");
    control.send(json!({"op": "open"}));
    assert!(entered.wait(Duration::from_secs(1)));
    assert_eq!(state.lock().unwrap().pending(), 1);
    assert_eq!(state.lock().unwrap().launch_thread_count(), 1);
    let payload = state
        .lock()
        .unwrap()
        .launch_payload_arc(0)
        .expect("payload retained before revocation");
    assert!(
        payload
            .lock()
            .unwrap()
            .windows(b"pR1v@te!".len())
            .any(|window| window == b"pR1v@te!"),
        "password missing from the retained launch payload"
    );

    control.send(json!({"op": "cancel"}));
    assert_eq!(control.until("cleanup")["state"], "pending");
    control.until("closed");

    assert!(state.lock().unwrap().config_arc().is_none());
    {
        let map = stored.lock().unwrap();
        let guest_config = map
            .get("guest_config")
            .and_then(Value::as_object)
            .expect("captured guest_config");
        assert!(
            !guest_config.contains_key("password"),
            "password survived revocation"
        );
    }
    assert!(payload.lock().unwrap().is_empty());
    assert!(state.lock().unwrap().input_bytes().is_empty());

    gate.set();
    assert!(state
        .lock()
        .unwrap()
        .join_launch_threads(Duration::from_secs(2)));
    assert_eq!(state.lock().unwrap().pending(), 0);
    handle.join().unwrap();
}

#[test]
fn test_binary_bridge_multiple_connections_and_lifetime_limit() {
    let _serial = serial_guard();
    let fixture = Fixture::new();
    let guest = fixture.guest_echo();
    let viewer = fixture.viewer_default();
    let (mut control, state, handle) = threaded_default();
    let mut config = fixture.config(&guest, &viewer, 30.0);
    config["connection_limit"] = json!(2);
    control.send(config);
    control.until("ready");
    control.send(json!({"op": "open"}));
    control.until("launched");
    let address = state.lock().unwrap().listener_addr().unwrap();
    let mut clients = Vec::new();
    for _ in 0..2 {
        let mut client = TcpStream::connect(address).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        assert_eq!(read_exact(&mut client, 12), b"RFB 003.008\n");
        let payload: Vec<u8> = (0..256u32)
            .cycle()
            .take(256 * 100)
            .map(|x| x as u8)
            .collect();
        client.write_all(&payload).unwrap();
        assert_eq!(read_exact(&mut client, payload.len()), payload);
        clients.push(client);
    }
    let mut third = TcpStream::connect(address).unwrap();
    third
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    assert_eof(&mut third);
    control.send(json!({"op": "cancel"}));
    control.until("closed");
    for client in clients.iter_mut() {
        assert_eof(client);
    }
    assert!(state.lock().unwrap().all_children_exited());
    handle.join().unwrap();
}

#[test]
fn test_expiry_closes_active_streams_with_blocked_ssh_launcher() {
    let _serial = serial_guard();
    let fixture = Fixture::new();
    let guest = fixture.guest_echo();
    let viewer = fixture.viewer_default();
    let gate = Arc::new(Gate::new());
    let launcher: Launcher = {
        let gate = gate.clone();
        let guest = guest.clone();
        Arc::new(move |argv: &[String], piped: bool| {
            if argv.first().map(|s| s.as_str()) == guest.to_str() {
                gate.wait(Duration::from_secs(3));
            }
            real_launch(argv, piped)
        })
    };
    let (mut control, state, handle) = threaded(launcher);
    control.send(fixture.config(&guest, &viewer, 0.3));
    control.until("ready");
    control.send(json!({"op": "open"}));
    control.until("launched");
    let address = state.lock().unwrap().listener_addr().unwrap();
    let mut client = TcpStream::connect(address).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    client.write_all(b"RFB-pending").unwrap();
    assert_eq!(control.until("closed")["reason"], "expired");
    assert_eof(&mut client);
    gate.set();
    handle.join().unwrap();
}

#[test]
fn test_apple_launch_credential_free_and_attempt_scoped() {
    let _serial = serial_guard();
    let fixture = Fixture::new();
    let guest = fixture.guest_echo();
    let viewer = fixture.viewer_default();
    let calls = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
    let launcher: Launcher = {
        let calls = calls.clone();
        Arc::new(move |argv: &[String], piped: bool| {
            calls.lock().unwrap().push(argv.to_vec());
            real_launch(&["/bin/sleep".to_string(), "30".to_string()], piped)
        })
    };
    let (mut control, state, handle) = threaded(launcher);
    let mut config = fixture.config(&guest, &viewer, 30.0);
    config["viewer"] = json!({"kind": "apple", "executable": "/Applications/Screen Sharing.app"});
    control.send(config);
    control.until("ready");
    control.send(json!({"op": "open"}));
    assert_eq!(control.until("launched")["authentication"], "required");
    let calls = calls.lock().unwrap();
    assert_eq!(
        &calls[0][..5],
        &[
            "/usr/bin/open".to_string(),
            "-n".to_string(),
            "-W".to_string(),
            "-a".to_string(),
            "/Applications/Screen Sharing.app".to_string()
        ]
    );
    assert!(calls[0][5].starts_with("vnc://127.0.0.1:"));
    assert!(!format!("{:?}", calls).contains("pR1v@te!"));
    drop(calls);
    control.send(json!({"op": "cancel"}));
    control.until("closed");
    assert!(state.lock().unwrap().all_children_exited());
    handle.join().unwrap();
}

#[test]
fn test_ttl_and_eof_close_established_transport() {
    let _serial = serial_guard();
    for revoke in ["ttl", "eof"] {
        let fixture = Fixture::new();
        let guest = fixture.guest_echo();
        let viewer = fixture.viewer_default();
        let (mut control, state, handle) = threaded_default();
        // A long initial grant: the handshake below must never race expiry.
        // The Python suite used a 0.35 s grant, which is safe only because
        // `unittest` runs sequentially and unloaded; under a parallel cargo run
        // the connect/READ/WRITE sequence can exceed a short grant, so the TTL
        // case expires the lease deliberately instead of racing the clock.
        control.send(fixture.config(&guest, &viewer, 30.0));
        control.until("ready");
        control.send(json!({"op": "open"}));
        control.until("launched");
        let address = state.lock().unwrap().listener_addr().unwrap();
        let mut client = TcpStream::connect(address).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        assert_eq!(read_exact(&mut client, 12), b"RFB 003.008\n");
        client.write_all(b"RFB").unwrap();
        assert_eq!(read_exact(&mut client, 3), b"RFB");
        if revoke == "eof" {
            control.close();
        } else {
            // Renew to a near deadline so expiry, not scheduling, decides when
            // the established transport closes. `renew` requires a strictly
            // greater generation than the configure frame used.
            control.send(json!({
                "op": "renew",
                "generation": 1,
                "deadline": monotonic() + 0.25,
            }));
            control.until("closed");
        }
        assert_eof(&mut client);
        handle.join().unwrap();
        assert!(state.lock().unwrap().all_children_exited());
    }
}

#[test]
fn test_viewer_argv_password_only_on_stdin() {
    let _serial = serial_guard();
    let fixture = Fixture::new();
    let guest = fixture.guest_echo();
    let marker = fixture.dir().join("capture");
    let viewer = fixture.viewer_capture(&marker);
    let calls = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
    let launcher: Launcher = {
        let calls = calls.clone();
        Arc::new(move |argv: &[String], piped: bool| {
            calls.lock().unwrap().push(argv.to_vec());
            real_launch(argv, piped)
        })
    };
    let (mut control, _state, handle) = threaded(launcher);
    control.send(fixture.config(&guest, &viewer, 30.0));
    control.until("ready");
    control.send(json!({"op": "open"}));
    control.until("launched");
    assert!(wait_until(
        || std::fs::read_to_string(&marker)
            .map(|s| !s.is_empty())
            .unwrap_or(false),
        Duration::from_secs(1)
    ));
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "pR1v@te!\n");
    let calls = calls.lock().unwrap();
    assert!(!format!("{:?}", calls).contains("pR1v@te!"));
    let last = calls.last().cloned().unwrap();
    assert!(last.contains(&"-AutoPass".to_string()));
    assert!(last.contains(&"-ViewOnly".to_string()));
    assert!(last.contains(&"-NoReconnect".to_string()));
    // The shebang-less viewer wrapper ran and read the password from stdin.
    // macOS libc `execvp` performs the ENOEXEC `/bin/sh` fallback inside
    // `spawn`, so this platform never surfaces ENOEXEC to `launch_process`; the
    // explicit retry is covered by the `enoexec_fallback_retries_with_shell`
    // unit test instead.
    assert_eq!(last[0], viewer.to_str().unwrap());
    drop(calls);
    control.send(json!({"op": "cancel"}));
    control.until("closed");
    handle.join().unwrap();
}

#[test]
fn test_large_embedded_guest_script() {
    let _serial = serial_guard();
    let fixture = Fixture::new();
    let viewer = fixture.viewer_default();
    let script = format!("{}\n#{}", ECHO_SCRIPT, "x".repeat(40000));
    let guest = write_exec(fixture.dir(), "guest-large.sh", &script);
    let (mut control, mut process) = spawn_process();
    let mut config = fixture.config(&guest, &viewer, 30.0);
    config["ssh_argv"] = json!([guest.to_str().unwrap(), "x".repeat(40000)]);
    let serialized = serde_json::to_string(&config).unwrap();
    assert!(serialized.len() > 16384);
    assert!(serialized.len() < MAX_COMMAND);
    control.send(config);
    control.until("ready");
    control.send(json!({"op": "cancel"}));
    control.until("closed");
    assert_eq!(
        wait_timeout(&mut process, Duration::from_secs(2))
            .unwrap()
            .code(),
        Some(0)
    );
}

#[test]
fn test_720_hour_grant() {
    let _serial = serial_guard();
    let fixture = Fixture::new();
    let guest = fixture.guest_echo();
    let viewer = fixture.viewer_default();
    let (mut control, mut process) = spawn_process();
    control.send(fixture.config(&guest, &viewer, 720.0 * 3600.0));
    control.until("ready");
    control.send(json!({
        "op": "renew",
        "generation": 1,
        "deadline": monotonic() + 720.0 * 3600.0
    }));
    control.until("renewed");
    control.send(json!({"op": "cancel"}));
    control.until("closed");
    assert_eq!(
        wait_timeout(&mut process, Duration::from_secs(2))
            .unwrap()
            .code(),
        Some(0)
    );
}

#[test]
fn test_password_free_macos() {
    let _serial = serial_guard();
    let fixture = Fixture::new();
    let guest = fixture.guest_echo();
    let viewer = fixture.viewer_default();
    let calls = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
    let launcher: Launcher = {
        let calls = calls.clone();
        Arc::new(move |argv: &[String], piped: bool| {
            calls.lock().unwrap().push(argv.to_vec());
            real_launch(&["/bin/sleep".to_string(), "30".to_string()], piped)
        })
    };
    let (mut control, _state, handle) = threaded(launcher);
    let mut config = fixture.config(&guest, &viewer, 30.0);
    config["guest_config"] = json!({
        "version": 1,
        "session": {"id": "console", "uid": 501, "user": "tester"},
        "expires_at": wall_time() + 3600.0
    });
    config["viewer"] = json!({"kind": "apple", "executable": "/Applications/Screen Sharing.app"});
    control.send(config);
    control.until("ready");
    control.send(json!({"op": "open"}));
    assert_eq!(control.until("launched")["authentication"], "required");
    control.send(json!({"op": "cancel"}));
    control.until("closed");
    assert_eq!(calls.lock().unwrap()[0][0], "/usr/bin/open");
    handle.join().unwrap();
}

#[test]
fn test_unknown_fields_rejected() {
    let _serial = serial_guard();
    let fixture = Fixture::new();
    let guest = fixture.guest_echo();
    let viewer = fixture.viewer_default();
    let commands = [
        json!({"op": "cancel", "extra": true}),
        json!({"op": "open", "extra": true}),
        json!({"op": "renew", "generation": 1, "deadline": monotonic() + 10.0, "extra": true}),
    ];
    for command in commands {
        let (mut control, mut process) = spawn_process();
        control.send(fixture.config(&guest, &viewer, 30.0));
        control.until("ready");
        control.send(command);
        assert_eq!(control.until("closed")["reason"], "invalid_command");
        assert!(wait_timeout(&mut process, Duration::from_secs(2)).is_some());
    }
    for field in ["configure", "guest_config", "viewer"] {
        let (mut control, mut process) = spawn_process();
        let mut config = fixture.config(&guest, &viewer, 30.0);
        match field {
            "configure" => config["extra"] = json!(true),
            "guest_config" => config["guest_config"]["extra"] = json!(true),
            "viewer" => config["viewer"]["extra"] = json!(true),
            _ => {}
        }
        control.send(config);
        assert_eq!(control.until("closed")["reason"], "invalid_command");
        assert!(wait_timeout(&mut process, Duration::from_secs(2)).is_some());
    }
}

#[test]
#[ignore = "test_actual_guest_config_parser_and_validator: the guest-console-agent crate owns \
            read_config/validate_config, so the parsing under test is asserted there"]
fn test_actual_guest_config_parser_and_validator() {
    let _serial = serial_guard();
}

#[test]
fn test_old_guest_schema_and_invalid_session_rejected() {
    let _serial = serial_guard();
    let fixture = Fixture::new();
    let guest = fixture.guest_echo();
    let viewer = fixture.viewer_default();
    let replacements = [
        json!({"uid": 0}),
        json!({"uid": true}),
        json!({"id": ""}),
        json!({"user": ""}),
    ];
    let mut invalid: Vec<Value> = vec![json!({
        "kind": "linux",
        "session": "desktop",
        "password": "pR1v@te!"
    })];
    for replacement in replacements {
        let mut guest_config = fixture.config(&guest, &viewer, 30.0)["guest_config"].clone();
        let session = guest_config["session"].as_object_mut().unwrap();
        for (key, value) in replacement.as_object().unwrap() {
            session.insert(key.clone(), value.clone());
        }
        invalid.push(guest_config);
    }
    for guest_config in invalid {
        let (mut control, mut process) = spawn_process();
        let mut config = fixture.config(&guest, &viewer, 30.0);
        config["guest_config"] = guest_config;
        control.send(config);
        assert_eq!(control.until("closed")["reason"], "invalid_command");
        assert!(wait_timeout(&mut process, Duration::from_secs(2)).is_some());
    }
}

#[test]
fn test_session_dictionary_preserved_in_transport() {
    let _serial = serial_guard();
    let fixture = Fixture::new();
    let viewer = fixture.viewer_default();
    let script = "#!/bin/sh\nIFS= read -r line\nprintf 'RFB 003.008\\n'\nprintf '%s\\n' \"$line\"\ncat >/dev/null\n";
    let guest = write_exec(fixture.dir(), "guest-echo-config.sh", script);
    let (mut control, state, handle) = threaded_default();
    let config = fixture.config(&guest, &viewer, 30.0);
    let expected = config["guest_config"].clone();
    control.send(config);
    control.until("ready");
    control.send(json!({"op": "open"}));
    control.until("launched");
    let address = state.lock().unwrap().listener_addr().unwrap();
    let mut client = TcpStream::connect(address).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    assert_eq!(read_exact(&mut client, 12), b"RFB 003.008\n");
    let mut received = Vec::new();
    while !received.ends_with(b"\n") {
        let mut chunk = [0u8; 4096];
        let n = client.read(&mut chunk).unwrap();
        received.extend_from_slice(&chunk[..n]);
    }
    assert_eq!(
        serde_json::from_slice::<Value>(&received).unwrap(),
        expected
    );
    control.send(json!({"op": "cancel"}));
    control.until("closed");
    handle.join().unwrap();
}

#[test]
fn test_guest_exit_without_valid_banner_never_connects() {
    let _serial = serial_guard();
    let fixture = Fixture::new();
    let viewer = fixture.viewer_default();
    let banners: Vec<&[u8]> = vec![
        b"",
        b"RFB 003.",
        b"RFB 003.009\n",
        b"RFB 003.889\n",
        b"not a banner",
    ];
    for (index, banner) in banners.iter().enumerate() {
        let script = format!(
            "#!/bin/sh\nIFS= read -r line\nprintf '{}'\n",
            banner.iter().map(|b| octal(*b)).collect::<String>()
        );
        let guest = write_exec(fixture.dir(), &format!("guest-banner-{index}.sh"), &script);
        let (mut control, state, handle) = threaded_default();
        control.send(fixture.config(&guest, &viewer, 30.0));
        control.until("ready");
        control.send(json!({"op": "open"}));
        control.until("launched");
        let address = state.lock().unwrap().listener_addr().unwrap();
        let mut client = TcpStream::connect(address).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        assert_eof(&mut client);
        assert_eq!(control.until("transport")["state"], "closed");
        control.send(json!({"op": "cancel"}));
        control.until("closed");
        assert!(!control
            .events
            .iter()
            .any(|e| e.get("connected").and_then(|v| v.as_bool()) == Some(true)));
        handle.join().unwrap();
    }
}

#[test]
fn test_fragmented_supported_banners_and_final_output_drained() {
    let _serial = serial_guard();
    let fixture = Fixture::new();
    let viewer = fixture.viewer_default();
    for version in ["003", "007", "008"] {
        let banner = format!("RFB 003.{version}\n").into_bytes();
        let mut script = String::from("#!/bin/sh\nIFS= read -r line\n");
        for byte in &banner {
            script.push_str(&format!("printf '{}'; sleep 0.002\n", octal(*byte)));
        }
        script.push_str("awk 'BEGIN{for(i=0;i<400000;i++)printf \"z\"}'\n");
        let guest = write_exec(fixture.dir(), &format!("guest-frag-{version}.sh"), &script);
        let (mut control, state, handle) = threaded_default();
        control.send(fixture.config(&guest, &viewer, 30.0));
        control.until("ready");
        control.send(json!({"op": "open"}));
        control.until("launched");
        let address = state.lock().unwrap().listener_addr().unwrap();
        let mut client = TcpStream::connect(address).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        assert_eq!(control.until("transport")["state"], "connected");
        thread::sleep(Duration::from_millis(50));
        let mut expected = banner.clone();
        expected.extend(std::iter::repeat_n(b'z', 400000));
        assert_eq!(read_exact(&mut client, 400012), expected);
        assert_eof(&mut client);
        assert_eq!(control.until("transport")["state"], "closed");
        control.send(json!({"op": "cancel"}));
        control.until("closed");
        assert_eq!(control.connected_count(), 1);
        handle.join().unwrap();
    }
}

#[test]
fn test_fragmented_apple_banner_without_password() {
    let _serial = serial_guard();
    let fixture = Fixture::new();
    let viewer = fixture.viewer_default();
    let launcher: Launcher = Arc::new(move |argv: &[String], piped: bool| {
        if argv.first().map(|s| s.as_str()) == Some("/usr/bin/open") {
            real_launch(&["/bin/sleep".to_string(), "30".to_string()], piped)
        } else {
            real_launch(argv, piped)
        }
    });
    let banner = b"RFB 003.889\n";
    let mut script = String::from("#!/bin/sh\nIFS= read -r line\n");
    for byte in banner {
        script.push_str(&format!("printf '{}'; sleep 0.003\n", octal(*byte)));
    }
    script.push_str("cat >/dev/null\n");
    let guest = write_exec(fixture.dir(), "guest-apple.sh", &script);
    let (mut control, state, handle) = threaded(launcher);
    let mut config = fixture.config(&guest, &viewer, 30.0);
    config["guest_config"] = json!({
        "version": 1,
        "session": {"id": "console", "uid": 501, "user": "tester"},
        "expires_at": wall_time() + 3600.0
    });
    config["viewer"] = json!({"kind": "apple", "executable": "/Applications/Screen Sharing.app"});
    control.send(config);
    control.until("ready");
    control.send(json!({"op": "open"}));
    assert_eq!(
        control.until("launched"),
        json!({"op": "launched", "authentication": "required"})
    );
    let address = state.lock().unwrap().listener_addr().unwrap();
    let mut client = TcpStream::connect(address).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    assert_eq!(read_exact(&mut client, 12), b"RFB 003.889\n");
    assert_eq!(
        control.until("transport"),
        json!({"op": "transport", "state": "connected", "connected": true})
    );
    control.send(json!({"op": "cancel"}));
    control.until("closed");
    let authentication: Vec<&Value> = control
        .events
        .iter()
        .filter(|e| e.get("authentication").is_some())
        .collect();
    assert_eq!(authentication.len(), 1);
    assert_eq!(authentication[0]["authentication"], "required");
    assert!(!control
        .events
        .iter()
        .any(|e| e.get("pixels").is_some() || e.get("authenticated").is_some()));
    assert_eq!(control.connected_count(), 1);
    handle.join().unwrap();
}

#[test]
fn test_signal_exit() {
    let _serial = serial_guard();
    let fixture = Fixture::new();
    let guest = fixture.guest_echo();
    let viewer = fixture.viewer_default();
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        let (mut control, mut process) = spawn_process();
        control.send(fixture.config(&guest, &viewer, 30.0));
        control.until("ready");
        unsafe {
            libc::kill(process.id() as libc::pid_t, signal);
        }
        assert_eq!(control.until("closed")["reason"], "signal");
        assert_eq!(
            wait_timeout(&mut process, Duration::from_secs(2))
                .unwrap()
                .code(),
            Some(0)
        );
    }
}
