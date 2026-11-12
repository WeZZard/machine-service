//! TL-2 fixtures: throwaway certificates and an in-process fault proxy.

#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use remote_session::{tls, Timing};
use rustls::pki_types::ServerName;
use rustls::{ClientConnection, ServerConnection, StreamOwned};

/// A self-signed certificate for `localhost`, as PEM.
pub struct Cert {
    pub cert_pem: String,
    pub key_pem: String,
}

impl Cert {
    pub fn localhost() -> Self {
        let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        Self {
            cert_pem: ck.cert.pem(),
            key_pem: ck.signing_key.serialize_pem(),
        }
    }

    pub fn server_config(&self) -> Arc<rustls::ServerConfig> {
        tls::server_config(self.cert_pem.as_bytes(), self.key_pem.as_bytes()).unwrap()
    }

    pub fn client_config(&self) -> Arc<rustls::ClientConfig> {
        tls::client_config(&[self.cert_pem.as_bytes()]).unwrap()
    }
}

/// Fast timing for tests: heartbeat 100 ms, dead after 1 s.
pub fn fast_timing() -> Timing {
    Timing {
        heartbeat: Duration::from_millis(100),
        dead_after: Duration::from_secs(1),
        handshake_timeout: Duration::from_secs(5),
        poll_interval: Duration::from_millis(5),
    }
}

/// How the proxy treats the bytes it carries.
#[derive(Clone)]
pub enum Mode {
    /// Forward TCP bytes as they are.
    Tcp,
    /// End the client's TLS with `front` and open a new TLS connection to
    /// the target, trusting `back` (TC-5).
    Tls {
        front: Arc<rustls::ServerConfig>,
        back: Arc<rustls::ClientConfig>,
    },
}

struct Conn {
    sockets: Vec<TcpStream>,
    killed: Arc<AtomicBool>,
}

struct State {
    target: Mutex<SocketAddr>,
    mode: Mode,
    hang: AtomicBool,
    refuse: AtomicBool,
    latency_ms: AtomicU64,
    jitter_ms: AtomicU64,
    /// Bytes per second each forwarding loop writes; 0 is no cap.
    bandwidth: AtomicU64,
    /// Carry both directions of a connection through one queue in arrival
    /// order, as a proxy that handles a connection's messages one after the
    /// other does: what the client sends waits behind what it still owes
    /// the client.
    one_queue: AtomicBool,
    accepted: AtomicU64,
    conns: Mutex<Vec<Conn>>,
    stop: AtomicBool,
}

/// A proxy between a client and a server that can reset connections, hang
/// silently while keeping them open, add latency and jitter, and refuse new
/// connections.
pub struct FaultProxy {
    pub addr: SocketAddr,
    state: Arc<State>,
}

impl FaultProxy {
    pub fn start(target: SocketAddr, mode: Mode) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let state = Arc::new(State {
            target: Mutex::new(target),
            mode,
            hang: AtomicBool::new(false),
            refuse: AtomicBool::new(false),
            latency_ms: AtomicU64::new(0),
            jitter_ms: AtomicU64::new(0),
            bandwidth: AtomicU64::new(0),
            one_queue: AtomicBool::new(false),
            accepted: AtomicU64::new(0),
            conns: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
        });
        let s = state.clone();
        std::thread::spawn(move || {
            while !s.stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((client, _)) => {
                        client.set_nonblocking(false).unwrap();
                        if s.refuse.load(Ordering::SeqCst) {
                            reset(&client);
                            continue;
                        }
                        s.accepted.fetch_add(1, Ordering::SeqCst);
                        let s = s.clone();
                        std::thread::spawn(move || serve(s, client));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        Self { addr, state }
    }

    pub fn url(&self, scheme: &str) -> String {
        format!("{scheme}://localhost:{}/relay", self.addr.port())
    }

    /// Reset every open connection with a TCP RST.
    pub fn reset(&self) {
        for conn in self.state.conns.lock().unwrap().drain(..) {
            conn.killed.store(true, Ordering::SeqCst);
            for s in &conn.sockets {
                reset(s);
            }
        }
    }

    /// Stop forwarding in both directions, keeping connections open.
    pub fn set_hang(&self, on: bool) {
        self.state.hang.store(on, Ordering::SeqCst);
    }

    /// Reset new connections as soon as they arrive.
    pub fn set_refuse(&self, on: bool) {
        self.state.refuse.store(on, Ordering::SeqCst);
    }

    pub fn set_latency(&self, base: Duration, jitter: Duration) {
        self.state
            .latency_ms
            .store(base.as_millis() as u64, Ordering::SeqCst);
        self.state
            .jitter_ms
            .store(jitter.as_millis() as u64, Ordering::SeqCst);
    }

    /// Cap each forwarding loop at `bytes_per_second`, holding the excess
    /// in an unbounded buffer, as a shaped link behind a deep queue does.
    pub fn set_bandwidth(&self, bytes_per_second: u64) {
        self.state
            .bandwidth
            .store(bytes_per_second, Ordering::SeqCst);
    }

    /// Carry both directions of new connections through one queue.
    pub fn set_one_queue(&self, on: bool) {
        self.state.one_queue.store(on, Ordering::SeqCst);
    }

    pub fn accepted(&self) -> u64 {
        self.state.accepted.load(Ordering::SeqCst)
    }
}

impl Drop for FaultProxy {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::SeqCst);
        self.reset();
    }
}

/// Close with SO_LINGER 0, which sends a TCP RST.
fn reset(s: &TcpStream) {
    let linger = libc::linger {
        l_onoff: 1,
        l_linger: 0,
    };
    // SAFETY: setsockopt on a socket this test owns, with a valid linger.
    unsafe {
        libc::setsockopt(
            s.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_LINGER,
            &linger as *const libc::linger as *const libc::c_void,
            std::mem::size_of::<libc::linger>() as libc::socklen_t,
        );
    }
    let _ = s.shutdown(Shutdown::Both);
}

fn serve(state: Arc<State>, client: TcpStream) {
    let target = *state.target.lock().unwrap();
    let Ok(upstream) = TcpStream::connect(target) else {
        reset(&client);
        return;
    };
    let killed = Arc::new(AtomicBool::new(false));
    state.conns.lock().unwrap().push(Conn {
        sockets: vec![client.try_clone().unwrap(), upstream.try_clone().unwrap()],
        killed: killed.clone(),
    });
    match state.mode.clone() {
        Mode::Tcp if state.one_queue.load(Ordering::SeqCst) => {
            one_queue(state, killed, client, upstream)
        }
        Mode::Tcp => {
            let a = (client.try_clone().unwrap(), upstream.try_clone().unwrap());
            let (s1, k1) = (state.clone(), killed.clone());
            std::thread::spawn(move || pump(s1, k1, a.0, a.1));
            pump(state, killed, upstream, client);
        }
        Mode::Tls { front, back } => tls_relay(state, killed, client, upstream, front, back),
    }
}

/// When a chunk read now is due: after the latency and jitter, and never
/// before the chunk read before it.
fn due_after(state: &State, last_due: &mut Instant) -> Instant {
    let base = state.latency_ms.load(Ordering::SeqCst);
    let jitter = state.jitter_ms.load(Ordering::SeqCst);
    let extra = if jitter == 0 {
        0
    } else {
        rand::random::<u64>() % (jitter + 1)
    };
    let due = (Instant::now() + Duration::from_millis(base + extra)).max(*last_due);
    *last_due = due;
    due
}

/// Wait until `due` and not hung; false once the connection is killed.
fn wait_due(state: &State, killed: &AtomicBool, due: Instant) -> bool {
    loop {
        if killed.load(Ordering::SeqCst) {
            return false;
        }
        if !state.hang.load(Ordering::SeqCst) && Instant::now() >= due {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// One forwarding loop's pacing schedule (DC-303): when its next byte may
/// go out at the bandwidth cap. Each write is due when the bytes sent since
/// the schedule started would take that long at the cap, so a sleep that
/// overshoots is made up by the next write instead of slowing the link, and
/// the link carries its cap whatever the host's sleep granularity. After an
/// idle gap the schedule starts again, so an idle link earns no burst.
#[derive(Default)]
struct Pacer {
    next: Option<Instant>,
}

impl Pacer {
    /// The longest idle gap after which the schedule starts again.
    const RESTART: Duration = Duration::from_millis(100);

    /// When a write of `bytes` that starts now is due to end at `rate`.
    fn due(&mut self, bytes: usize, rate: u64) -> Instant {
        let now = Instant::now();
        let start = match self.next {
            Some(next) if next + Self::RESTART >= now => next,
            _ => now,
        };
        let next = start + Duration::from_secs_f64(bytes as f64 / rate as f64);
        self.next = Some(next);
        next
    }
}

/// Write `bytes` at the bandwidth cap, on `pacer`'s schedule.
fn write_shaped(
    state: &State,
    pacer: &mut Pacer,
    to: &mut TcpStream,
    bytes: &[u8],
) -> std::io::Result<()> {
    let rate = state.bandwidth.load(Ordering::SeqCst);
    if rate == 0 {
        return to.write_all(bytes);
    }
    let step = (rate / 100).max(1) as usize;
    for piece in bytes.chunks(step) {
        to.write_all(piece)?;
        let due = pacer.due(piece.len(), rate);
        if let Some(rest) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(rest);
        }
    }
    Ok(())
}

/// Read `from` into `tx` as `(direction, due, bytes)` until it ends.
fn read_into<D: Copy + Send + 'static>(
    state: Arc<State>,
    killed: Arc<AtomicBool>,
    mut from: TcpStream,
    tx: mpsc::Sender<(D, Instant, Vec<u8>)>,
    direction: D,
) {
    from.set_read_timeout(Some(Duration::from_millis(20)))
        .unwrap();
    let mut buf = vec![0u8; 64 * 1024];
    let mut last_due = Instant::now();
    while !killed.load(Ordering::SeqCst) {
        match from.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let due = due_after(&state, &mut last_due);
                if tx.send((direction, due, buf[..n].to_vec())).is_err() {
                    break;
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => break,
        }
    }
}

/// Both directions through one queue in arrival order (see `one_queue`).
fn one_queue(state: Arc<State>, killed: Arc<AtomicBool>, client: TcpStream, upstream: TcpStream) {
    let (tx, rx) = mpsc::channel::<(bool, Instant, Vec<u8>)>();
    for (from, toward_client) in [
        (client.try_clone().unwrap(), false),
        (upstream.try_clone().unwrap(), true),
    ] {
        let (s, k, tx) = (state.clone(), killed.clone(), tx.clone());
        std::thread::spawn(move || read_into(s, k, from, tx, toward_client));
    }
    drop(tx);
    let (mut to_client, mut to_upstream) = (client, upstream);
    let mut pacer = Pacer::default();
    while let Ok((toward_client, due, bytes)) = rx.recv() {
        if !wait_due(&state, &killed, due) {
            return;
        }
        let to = if toward_client {
            &mut to_client
        } else {
            &mut to_upstream
        };
        if write_shaped(&state, &mut pacer, to, &bytes).is_err() {
            return;
        }
    }
}

/// Forward `from` to `to`, delaying each chunk by the latency and holding it
/// while hung. Order is kept: a chunk never overtakes the one before it.
fn pump(state: Arc<State>, killed: Arc<AtomicBool>, from: TcpStream, mut to: TcpStream) {
    let (tx, rx) = mpsc::channel::<((), Instant, Vec<u8>)>();
    let (ws, wk) = (state.clone(), killed.clone());
    let mut writer_to = to.try_clone().unwrap();
    let writer = std::thread::spawn(move || {
        let mut pacer = Pacer::default();
        while let Ok((_, due, bytes)) = rx.recv() {
            if !wait_due(&ws, &wk, due) {
                return;
            }
            if write_shaped(&ws, &mut pacer, &mut writer_to, &bytes).is_err() {
                return;
            }
        }
        let _ = writer_to.shutdown(Shutdown::Write);
    });
    read_into(state, killed, from, tx, ());
    let _ = writer.join();
    let _ = to.flush();
}

/// End TLS from the client and open a new TLS connection to the target,
/// moving plaintext between them.
fn tls_relay(
    state: Arc<State>,
    killed: Arc<AtomicBool>,
    client: TcpStream,
    upstream: TcpStream,
    front: Arc<rustls::ServerConfig>,
    back: Arc<rustls::ClientConfig>,
) {
    client
        .set_read_timeout(Some(Duration::from_millis(5)))
        .unwrap();
    upstream
        .set_read_timeout(Some(Duration::from_millis(5)))
        .unwrap();
    let mut a = StreamOwned::new(ServerConnection::new(front).unwrap(), client);
    let name = ServerName::try_from("localhost").unwrap();
    let mut b = StreamOwned::new(ClientConnection::new(back, name).unwrap(), upstream);
    let mut buf = vec![0u8; 64 * 1024];
    let blocked = |e: &std::io::Error| {
        matches!(
            e.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        )
    };
    while !killed.load(Ordering::SeqCst) && !state.stop.load(Ordering::SeqCst) {
        let mut moved = false;
        for dir in 0..2 {
            let r = if dir == 0 {
                a.read(&mut buf)
            } else {
                b.read(&mut buf)
            };
            match r {
                Ok(0) => return,
                Ok(n) => {
                    moved = true;
                    let w = if dir == 0 {
                        write_retrying(&mut b, &buf[..n])
                    } else {
                        write_retrying(&mut a, &buf[..n])
                    };
                    if w.is_err() {
                        return;
                    }
                }
                Err(e) if blocked(&e) => {}
                Err(_) => return,
            }
        }
        if !moved {
            std::thread::yield_now();
        }
    }
}

/// Write everything, retrying while a TLS handshake still waits on a read.
fn write_retrying(s: &mut impl Write, mut bytes: &[u8]) -> std::io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let retry = |e: &std::io::Error| {
        Instant::now() < deadline
            && matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            )
    };
    while !bytes.is_empty() {
        match s.write(bytes) {
            Ok(n) => bytes = &bytes[n..],
            Err(e) if retry(&e) => std::thread::sleep(Duration::from_millis(1)),
            Err(e) => return Err(e),
        }
    }
    loop {
        match s.flush() {
            Ok(()) => return Ok(()),
            Err(e) if retry(&e) => std::thread::sleep(Duration::from_millis(1)),
            Err(e) => return Err(e),
        }
    }
}
