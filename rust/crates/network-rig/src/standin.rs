//! The stand-in target: a WebSocket echo server under TLS.
//!
//! It stands where the broker will stand until the broker exists (M3). It
//! listens on loopback only, presents a certificate for the broker's name
//! signed by the broker's authority, echoes every text and binary message,
//! and records what each connection showed it, so a test can see what reached
//! the far side of the chain.

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use rustls::{ServerConfig, ServerConnection, StreamOwned};
use tungstenite::handshake::server::{Request, Response};
use tungstenite::Message;

use crate::tls::{self, Authority};
use crate::RigError;

/// What one connection showed the stand-in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Visit {
    /// The connection's source address.
    pub peer: Option<SocketAddr>,
    /// The TLS server name the peer sent.
    pub sni: Option<String>,
    /// The `Host` header of the upgrade request.
    pub host: Option<String>,
    /// The path of the upgrade request.
    pub path: Option<String>,
    /// Text and binary messages received.
    pub messages: usize,
    /// The size of the largest message received.
    pub largest_message: usize,
    /// Whether the WebSocket upgrade completed.
    pub upgraded: bool,
}

/// A running stand-in.
pub struct EchoServer {
    addr: SocketAddr,
    authority: Authority,
    visits: Arc<Mutex<Vec<Visit>>>,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for EchoServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EchoServer")
            .field("addr", &self.addr)
            .finish_non_exhaustive()
    }
}

impl EchoServer {
    /// Start a stand-in that serves `name` on a free loopback port.
    pub fn start(name: &str) -> Result<EchoServer, RigError> {
        let authority = Authority::new("network-rig broker authority")?;
        let config = tls::server_config(&authority.issue(&[name])?)?;
        let listener =
            TcpListener::bind("127.0.0.1:0").map_err(RigError::io("bind the stand-in"))?;
        listener
            .set_nonblocking(true)
            .map_err(RigError::io("configure the stand-in"))?;
        let addr = listener
            .local_addr()
            .map_err(RigError::io("read the stand-in address"))?;
        let visits = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let accept = {
            let (visits, stop) = (visits.clone(), stop.clone());
            std::thread::spawn(move || accept_loop(listener, config, visits, stop))
        };
        Ok(EchoServer {
            addr,
            authority,
            visits,
            stop,
            accept: Some(accept),
        })
    }

    /// Where it listens. Only the rig is told.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The broker authority's certificate, PEM, for the edge to trust.
    pub fn ca_pem(&self) -> &str {
        self.authority.cert_pem()
    }

    /// Every connection so far, in the order they arrived.
    pub fn visits(&self) -> Vec<Visit> {
        self.visits
            .lock()
            .map(|visits| visits.clone())
            .unwrap_or_default()
    }
}

impl Drop for EchoServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
    }
}

fn accept_loop(
    listener: TcpListener,
    config: Arc<ServerConfig>,
    visits: Arc<Mutex<Vec<Visit>>>,
    stop: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, peer)) => {
                let index = match visits.lock() {
                    Ok(mut visits) => {
                        visits.push(Visit {
                            peer: Some(peer),
                            ..Visit::default()
                        });
                        visits.len() - 1
                    }
                    Err(_) => return,
                };
                let (config, visits, stop) = (config.clone(), visits.clone(), stop.clone());
                std::thread::spawn(move || serve(stream, config, &visits, index, &stop));
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

fn record(visits: &Mutex<Vec<Visit>>, index: usize, change: impl FnOnce(&mut Visit)) {
    if let Ok(mut visits) = visits.lock() {
        if let Some(visit) = visits.get_mut(index) {
            change(visit);
        }
    }
}

// The upgrade callback must return tungstenite's own `ErrorResponse`, which is large.
#[allow(clippy::result_large_err)]
fn serve(
    stream: TcpStream,
    config: Arc<ServerConfig>,
    visits: &Mutex<Vec<Visit>>,
    index: usize,
    stop: &AtomicBool,
) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let Ok(connection) = ServerConnection::new(config) else {
        return;
    };
    let mut tls = StreamOwned::new(connection, stream);
    // Finish the handshake before the upgrade, so the server name is known.
    while tls.conn.is_handshaking() {
        match tls.conn.complete_io(&mut tls.sock) {
            Ok(_) => {}
            Err(error) if is_timeout(&error) && !stop.load(Ordering::SeqCst) => continue,
            Err(_) => return,
        }
    }
    let sni = tls.conn.server_name().map(str::to_string);
    record(visits, index, |visit| visit.sni = sni);
    let callback = |request: &Request, response: Response| {
        let host = request
            .headers()
            .get("host")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let path = request.uri().path().to_string();
        record(visits, index, |visit| {
            visit.host = host;
            visit.path = Some(path);
        });
        Ok(response)
    };
    let mut handshake = tungstenite::accept_hdr(tls, callback);
    let mut socket = loop {
        match handshake {
            Ok(socket) => break socket,
            Err(tungstenite::HandshakeError::Interrupted(mid)) if !stop.load(Ordering::SeqCst) => {
                handshake = mid.handshake();
            }
            Err(_) => return,
        }
    };
    record(visits, index, |visit| visit.upgraded = true);
    while !stop.load(Ordering::SeqCst) {
        match socket.read() {
            Ok(message @ (Message::Text(_) | Message::Binary(_))) => {
                let size = message.len();
                record(visits, index, |visit| {
                    visit.messages += 1;
                    visit.largest_message = visit.largest_message.max(size);
                });
                if socket.send(message).is_err() {
                    return;
                }
            }
            Ok(Message::Close(_)) => {
                let _ = socket.flush();
                return;
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(error)) if is_timeout(&error) => {
                let _ = socket.flush();
            }
            Err(_) => return,
        }
    }
}

fn is_timeout(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}
