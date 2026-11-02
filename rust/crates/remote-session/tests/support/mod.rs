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
        Mode::Tcp => {
            let a = (client.try_clone().unwrap(), upstream.try_clone().unwrap());
            let (s1, k1) = (state.clone(), killed.clone());
            std::thread::spawn(move || pump(s1, k1, a.0, a.1));
            pump(state, killed, upstream, client);
        }
        Mode::Tls { front, back } => tls_relay(state, killed, client, upstream, front, back),
    }
}

/// Forward `from` to `to`, delaying each chunk by the latency and holding it
/// while hung. Order is kept: a chunk never overtakes the one before it.
fn pump(state: Arc<State>, killed: Arc<AtomicBool>, mut from: TcpStream, mut to: TcpStream) {
    let (tx, rx) = mpsc::channel::<(Instant, Vec<u8>)>();
    let (ws, wk) = (state.clone(), killed.clone());
    let mut writer_to = to.try_clone().unwrap();
    let writer = std::thread::spawn(move || {
        while let Ok((due, bytes)) = rx.recv() {
            loop {
                if wk.load(Ordering::SeqCst) {
                    return;
                }
                if !ws.hang.load(Ordering::SeqCst) && Instant::now() >= due {
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            if writer_to.write_all(&bytes).is_err() {
                return;
            }
        }
        let _ = writer_to.shutdown(Shutdown::Write);
    });
    from.set_read_timeout(Some(Duration::from_millis(20)))
        .unwrap();
    let mut buf = vec![0u8; 64 * 1024];
    let mut last_due = Instant::now();
    while !killed.load(Ordering::SeqCst) {
        match from.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let base = state.latency_ms.load(Ordering::SeqCst);
                let jitter = state.jitter_ms.load(Ordering::SeqCst);
                let extra = if jitter == 0 {
                    0
                } else {
                    rand::random::<u64>() % (jitter + 1)
                };
                let due = (Instant::now() + Duration::from_millis(base + extra)).max(last_due);
                last_due = due;
                if tx.send((due, buf[..n].to_vec())).is_err() {
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
    drop(tx);
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
