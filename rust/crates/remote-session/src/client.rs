//! The client side: dial by URL, say hello, and keep the session alive across
//! broken connections with backoff (RS-2, RS-4, TC-1, TC-4, TC-6).

use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustls::pki_types::ServerName;
use rustls::{ClientConnection, StreamOwned};
use tungstenite::handshake::HandshakeError;
use tungstenite::http::Uri;

use crate::link::{self, Exit, Stream, Ws};
use crate::session::{Session, SessionEnd, Shared, DEFAULT_MAX_BUFFERED};
use crate::timing::{Backoff, Timing};
use crate::wire::{Control, ExpiryReason, Hello, Welcome, PROTOCOL_VERSION};
use crate::{tls, Error};

/// Resolves a host name and port to the addresses to try, on every connection
/// attempt. The default is the system resolver.
pub type Resolver = Arc<dyn Fn(&str, u16) -> std::io::Result<Vec<SocketAddr>> + Send + Sync>;

/// A resolver that answers `address` for `host:port`, as `curl --resolve
/// host:port:address` does, and the system resolver for any other name. The
/// spec is `host:port:address`; an IPv6 address may be bracketed.
pub fn fixed_resolver(spec: &str) -> Result<Resolver, Error> {
    let bad = || Error::Config(format!("resolve {spec:?} is not host:port:address"));
    let (host, rest) = spec.split_once(':').ok_or_else(bad)?;
    let (port, address) = rest.split_once(':').ok_or_else(bad)?;
    let port: u16 = port.parse().map_err(|_| bad())?;
    let address: IpAddr = address
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .map_err(|_| bad())?;
    if host.is_empty() {
        return Err(bad());
    }
    let host = host.to_ascii_lowercase();
    Ok(Arc::new(move |name: &str, wanted: u16| {
        if name.eq_ignore_ascii_case(&host) && wanted == port {
            Ok(vec![SocketAddr::new(address, port)])
        } else {
            (name, wanted).to_socket_addrs().map(Iterator::collect)
        }
    }))
}

/// How a client reaches and authenticates to a session server.
#[derive(Clone)]
pub struct ClientConfig {
    /// `wss://host:port/path`, or `ws://` to a loopback host. The host is
    /// resolved again on every connection, so its address may change.
    pub url: String,
    /// The bearer credential sent in the hello.
    pub credential: String,
    /// Opaque attributes for the server's authenticator, such as a role.
    pub attributes: serde_json::Value,
    /// TLS trust; `None` trusts the Mozilla roots.
    pub tls: Option<Arc<rustls::ClientConfig>>,
    pub timing: Timing,
    pub backoff: Backoff,
    /// Bytes held unacknowledged before [`Session::send`] waits.
    pub max_buffered: usize,
    /// How the URL's host is resolved on each attempt; `None` is the system
    /// resolver. The TLS server name stays the URL's host either way.
    pub resolver: Option<Resolver>,
}

impl std::fmt::Debug for ClientConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientConfig")
            .field("url", &self.url)
            .field("credential", &"<redacted>")
            .field("attributes", &self.attributes)
            .field("timing", &self.timing)
            .field("backoff", &self.backoff)
            .finish_non_exhaustive()
    }
}

impl ClientConfig {
    pub fn new(url: impl Into<String>, credential: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            credential: credential.into(),
            attributes: serde_json::Value::Null,
            tls: None,
            timing: Timing::default(),
            backoff: Backoff::default(),
            max_buffered: DEFAULT_MAX_BUFFERED,
            resolver: None,
        }
    }
}

#[derive(Clone)]
pub(crate) struct Target {
    url: String,
    secure: bool,
    host: String,
    port: u16,
    resolver: Option<Resolver>,
}

impl Target {
    #[cfg(test)]
    pub fn parse(url: &str) -> Result<Self, Error> {
        Self::parse_with(url, None)
    }

    pub fn parse_with(url: &str, resolver: Option<Resolver>) -> Result<Self, Error> {
        let uri: Uri = url
            .parse()
            .map_err(|e| Error::Config(format!("bad URL {url:?}: {e}")))?;
        let secure = match uri.scheme_str() {
            Some("wss") => true,
            Some("ws") => false,
            _ => return Err(Error::Config(format!("URL {url:?} is not ws:// or wss://"))),
        };
        let host = uri
            .host()
            .ok_or_else(|| Error::Config(format!("URL {url:?} has no host")))?
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string();
        let port = uri.port_u16().unwrap_or(if secure { 443 } else { 80 });
        let target = Self {
            url: url.to_string(),
            secure,
            host,
            port,
            resolver,
        };
        if !secure {
            target.resolve()?;
        }
        Ok(target)
    }

    /// The addresses to try now. Plain WebSocket is only for loopback.
    fn resolve(&self) -> Result<Vec<SocketAddr>, Error> {
        let resolved = match &self.resolver {
            Some(resolve) => resolve(&self.host, self.port),
            None => (self.host.as_str(), self.port)
                .to_socket_addrs()
                .map(Iterator::collect),
        };
        let addrs: Vec<SocketAddr> = resolved
            .map_err(|e| Error::Connect(format!("resolve {}: {e}", self.host)))?
            .into_iter()
            .filter(|a| self.secure || a.ip().is_loopback())
            .collect();
        if addrs.is_empty() {
            return Err(if self.secure {
                Error::Connect(format!("{} has no address", self.host))
            } else {
                Error::Config(format!(
                    "plain ws:// is allowed only to a loopback host, not {}",
                    self.host
                ))
            });
        }
        Ok(addrs)
    }
}

pub(crate) enum Reply {
    Welcome(Welcome),
    Expired(ExpiryReason),
    Refused(Error),
}

/// Open one connection and exchange hello and welcome.
pub(crate) fn dial(
    target: &Target,
    tls: &Arc<rustls::ClientConfig>,
    timing: &Timing,
    hello: Hello,
) -> Result<(Ws, SocketAddr, Reply), Error> {
    let connect_err = |e: String| Error::Connect(e);
    let mut last = None;
    for addr in target.resolve()? {
        match open(target, tls, timing, addr) {
            Ok(mut ws) => {
                let deadline = Instant::now() + timing.handshake_timeout;
                link::send_control(&mut ws, &Control::Hello(hello.clone()))
                    .map_err(|e| connect_err(e.to_string()))?;
                let reply = match link::read_control(&mut ws, deadline).map_err(connect_err)? {
                    Control::Welcome(w) => Reply::Welcome(w),
                    Control::Expired { reason, .. } => Reply::Expired(reason),
                    Control::Error { code, message } => {
                        Reply::Refused(Error::Rejected { code, message })
                    }
                    other => {
                        return Err(Error::Protocol(format!(
                            "unexpected answer to hello: {other:?}"
                        )))
                    }
                };
                return Ok((ws, addr, reply));
            }
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| Error::Connect("no address".into())))
}

fn open(
    target: &Target,
    tls: &Arc<rustls::ClientConfig>,
    timing: &Timing,
    addr: SocketAddr,
) -> Result<Ws, Error> {
    let err = |e: String| Error::Connect(format!("{addr}: {e}"));
    let tcp = TcpStream::connect_timeout(&addr, timing.handshake_timeout)
        .map_err(|e| err(e.to_string()))?;
    tcp.set_nodelay(true).map_err(|e| err(e.to_string()))?;
    let stream = if target.secure {
        let name = ServerName::try_from(target.host.clone()).map_err(|e| err(e.to_string()))?;
        let conn = ClientConnection::new(tls.clone(), name).map_err(|e| err(e.to_string()))?;
        Stream::Client(Box::new(StreamOwned::new(conn, tcp)))
    } else {
        Stream::Plain(tcp)
    };
    stream
        .set_timeouts(timing.handshake_timeout)
        .map_err(|e| err(e.to_string()))?;
    let ws = match tungstenite::client::client_with_config(
        target.url.as_str(),
        stream,
        Some(link::ws_config()),
    ) {
        Ok((ws, _)) => ws,
        Err(HandshakeError::Interrupted(_)) => {
            return Err(err("WebSocket handshake timed out".into()))
        }
        Err(HandshakeError::Failure(e)) => return Err(err(e.to_string())),
    };
    ws.get_ref()
        .set_timeouts(timing.poll_interval)
        .map_err(|e| err(e.to_string()))?;
    Ok(ws)
}

/// Open a new session. The first connection is made before this returns, so
/// a wrong URL, trust or credential fails here; later breaks are resumed in
/// the background.
pub fn connect(config: ClientConfig) -> Result<Session, Error> {
    config.timing.validate()?;
    let target = Target::parse_with(&config.url, config.resolver.clone())?;
    let tls = config
        .tls
        .clone()
        .unwrap_or_else(tls::default_client_config);
    let hello = Hello {
        version: PROTOCOL_VERSION,
        session: None,
        received: 0,
        credential: config.credential.clone(),
        attributes: config.attributes.clone(),
    };
    let (ws, peer, reply) = dial(&target, &tls, &config.timing, hello)?;
    let welcome = match reply {
        Reply::Welcome(w) if !w.resumed => w,
        Reply::Welcome(_) => {
            return Err(Error::Protocol(
                "server resumed a session never asked for".into(),
            ))
        }
        Reply::Expired(_) => {
            return Err(Error::Protocol(
                "server expired a session never asked for".into(),
            ))
        }
        Reply::Refused(e) => return Err(e),
    };
    let (shared, events) = Shared::new(welcome.session.clone(), config.max_buffered);
    let generation = {
        let mut inner = shared.lock();
        inner
            .seq
            .resume(0)
            .map_err(|e| Error::Protocol(e.to_string()))?;
        inner.attach(false, peer)
    };
    let session = Session::new(shared.clone(), events, None, serde_json::Value::Null);
    let driver = Driver {
        shared,
        target,
        tls,
        config,
        grace: Duration::from_millis(welcome.grace_ms),
    };
    std::thread::Builder::new()
        .name("remote-session-client".into())
        .spawn(move || driver.run(ws, generation))
        .map_err(|e| Error::Connect(format!("spawn: {e}")))?;
    Ok(session)
}

struct Driver {
    shared: Arc<Shared>,
    target: Target,
    tls: Arc<rustls::ClientConfig>,
    config: ClientConfig,
    grace: Duration,
}

impl Driver {
    fn run(self, mut ws: Ws, mut generation: u64) {
        loop {
            let exit = link::run(&mut ws, &self.shared, generation, &self.config.timing);
            drop(ws);
            match exit {
                Exit::LocalClose => {
                    return self.shared.finish(SessionEnd::Closed { by_peer: false })
                }
                Exit::PeerBye => return self.shared.finish(SessionEnd::Closed { by_peer: true }),
                Exit::Ended | Exit::Superseded => return,
                lost => {
                    self.shared.lock().detach(generation, lost.reason());
                }
            }
            match self.reconnect() {
                Some((next, g)) => {
                    ws = next;
                    generation = g;
                }
                None => return,
            }
        }
    }

    /// Reconnect with backoff until resumed, or end the session and return
    /// `None`. The server may notice the loss up to `dead_after` later than
    /// this side, so after `grace + dead_after` it has surely dropped the
    /// session, and the client stops without waiting for a server to say so.
    fn reconnect(&self) -> Option<(Ws, u64)> {
        let lost_at = Instant::now();
        let give_up = lost_at + self.grace + self.config.timing.dead_after;
        let mut attempt = 0u32;
        loop {
            let pause = self.config.backoff.delay(attempt, rand::random::<f64>());
            attempt = attempt.saturating_add(1);
            let wake = (Instant::now() + pause).min(give_up);
            {
                let mut inner = self.shared.lock();
                loop {
                    if inner.end.is_some() {
                        return None;
                    }
                    if inner.close_requested {
                        inner.finish(SessionEnd::Closed { by_peer: false });
                        drop(inner);
                        self.shared.changed.notify_all();
                        return None;
                    }
                    let now = Instant::now();
                    if now >= give_up {
                        inner.finish(SessionEnd::Expired(ExpiryReason::GraceElapsed));
                        drop(inner);
                        self.shared.changed.notify_all();
                        return None;
                    }
                    if now >= wake {
                        break;
                    }
                    inner = self.shared.wait(inner, wake - now);
                }
            }
            let hello = Hello {
                version: PROTOCOL_VERSION,
                session: Some(self.shared.id.clone()),
                received: self.shared.lock().seq.received(),
                credential: self.config.credential.clone(),
                attributes: self.config.attributes.clone(),
            };
            let end = match dial(&self.target, &self.tls, &self.config.timing, hello) {
                Err(Error::Connect(_)) => continue,
                Err(e) => SessionEnd::Protocol(e.to_string()),
                Ok((ws, peer, Reply::Welcome(w))) if w.resumed && w.session == self.shared.id => {
                    let mut inner = self.shared.lock();
                    match inner.seq.resume(w.received) {
                        Ok(()) => {
                            let generation = inner.attach(true, peer);
                            return Some((ws, generation));
                        }
                        Err(e) => SessionEnd::Protocol(e.to_string()),
                    }
                }
                Ok((_, _, Reply::Welcome(w))) => {
                    SessionEnd::Protocol(format!("asked to resume {}, got {:?}", self.shared.id, w))
                }
                Ok((_, _, Reply::Expired(reason))) => SessionEnd::Expired(reason),
                Ok((_, _, Reply::Refused(Error::Rejected { code, message }))) => {
                    SessionEnd::Rejected { code, message }
                }
                Ok((_, _, Reply::Refused(e))) => SessionEnd::Protocol(e.to_string()),
            };
            self.shared.finish(end);
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_websocket_is_only_for_loopback() {
        assert!(Target::parse("ws://127.0.0.1:9/relay").is_ok());
        assert!(Target::parse("ws://[::1]:9/relay").is_ok());
        assert!(matches!(
            Target::parse("ws://192.0.2.1:9/relay"),
            Err(Error::Config(_))
        ));
        assert!(Target::parse("wss://broker.example.invalid/relay").is_ok());
        assert!(matches!(
            Target::parse("http://localhost/relay"),
            Err(Error::Config(_))
        ));
        let t = Target::parse("wss://broker.example.invalid/relay").unwrap();
        assert_eq!((t.host.as_str(), t.port), ("broker.example.invalid", 443));
    }

    #[test]
    fn a_fixed_resolver_answers_its_name_and_port_only() {
        let resolve = fixed_resolver("broker.relay-rig.test:8443:127.0.0.1").unwrap();
        assert_eq!(
            resolve("Broker.Relay-Rig.test", 8443).unwrap(),
            vec!["127.0.0.1:8443".parse().unwrap()]
        );
        // Any other name goes to the system resolver.
        assert!(resolve("127.0.0.1", 9)
            .unwrap()
            .iter()
            .all(|a| a.ip().is_loopback()));
        let six = fixed_resolver("h:1:[::1]").unwrap();
        assert_eq!(six("h", 1).unwrap(), vec!["[::1]:1".parse().unwrap()]);
        for bad in ["", "h", "h:1", "h:x:127.0.0.1", ":1:127.0.0.1", "h:1:nope"] {
            assert!(fixed_resolver(bad).is_err(), "{bad}");
        }
        let target = Target::parse_with(
            "wss://broker.relay-rig.test:8443/relay",
            Some(resolve.clone()),
        )
        .unwrap();
        assert_eq!(
            target.resolve().unwrap(),
            vec!["127.0.0.1:8443".parse().unwrap()]
        );
        // A resolver may make a name plain-WebSocket-eligible only by
        // answering loopback.
        assert!(Target::parse_with("ws://broker.relay-rig.test:8443/relay", Some(resolve)).is_ok());
    }
}
