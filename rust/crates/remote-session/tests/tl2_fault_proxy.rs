//! TL-2: a real client and a real server over TCP and TLS, through an
//! in-process fault proxy: reset, silent hang, latency and jitter, a
//! reconnect from a new source port, a server restart, grace expiry, a proxy
//! that ends TLS and re-encrypts, and the message cap.

mod support;

use std::net::{SocketAddr, TcpListener};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use remote_session::{
    connect, split_body, ClientConfig, Error, ExpiryReason, LinkEvent, Listening, RejectCode,
    ServerConfig, Session, SessionEnd, SessionServer, StaticToken, Timing, MAX_BODY,
};
use support::{fast_timing, Cert, FaultProxy, Mode};

const TOKEN: &str = "test-token";

struct Server {
    server: SessionServer,
    listening: Listening,
}

impl Server {
    fn start(cert: &Cert, timing: Timing, grace: Duration) -> Self {
        Self::start_on("127.0.0.1:0".parse().unwrap(), cert, timing, grace)
    }

    fn start_on(addr: SocketAddr, cert: &Cert, timing: Timing, grace: Duration) -> Self {
        let mut config = ServerConfig::new(StaticToken::new(TOKEN, "tester"));
        config.tls = Some(cert.server_config());
        config.timing = timing;
        config.grace = grace;
        let server = SessionServer::new(config).unwrap();
        let listening = server.listen(TcpListener::bind(addr).unwrap()).unwrap();
        Self { server, listening }
    }

    fn addr(&self) -> SocketAddr {
        self.listening.local_addr()
    }

    fn url(&self) -> String {
        format!("wss://localhost:{}/relay", self.addr().port())
    }

    fn accept(&self) -> Session {
        self.server
            .accept_timeout(Duration::from_secs(10))
            .unwrap()
            .expect("a new session")
    }
}

fn client(url: String, trust: &Cert, timing: Timing) -> ClientConfig {
    let mut c = ClientConfig::new(url, TOKEN);
    c.tls = Some(trust.client_config());
    c.timing = timing;
    c.attributes = serde_json::json!({"role": "client"});
    c
}

fn events(rx: &Receiver<LinkEvent>) -> Vec<LinkEvent> {
    rx.try_iter().collect()
}

fn recv(s: &Session) -> Vec<u8> {
    s.recv_timeout(Duration::from_secs(20))
        .unwrap()
        .expect("a message within 20 s")
}

/// Echo every body back with a prefix until the session ends.
fn echo(session: Session) -> std::thread::JoinHandle<Vec<Vec<u8>>> {
    std::thread::spawn(move || {
        let mut got = Vec::new();
        while let Ok(body) = session.recv() {
            session.send([b"echo:".as_slice(), &body].concat()).unwrap();
            got.push(body);
        }
        got
    })
}

fn numbered(n: usize) -> Vec<u8> {
    format!("m{n}").into_bytes()
}

#[test]
fn tls_by_host_name_round_trips_in_order() {
    let cert = Cert::localhost();
    let server = Server::start(&cert, fast_timing(), Duration::from_secs(60));
    let session = connect(client(server.url(), &cert, fast_timing())).unwrap();
    let remote = server.accept();
    assert_eq!(remote.id(), session.id());
    assert_eq!(remote.principal(), Some("tester"));
    assert_eq!(remote.attributes()["role"], "client");
    let echoed = echo(remote);
    for i in 0..50 {
        session.send(numbered(i)).unwrap();
    }
    for i in 0..50 {
        assert_eq!(recv(&session), [b"echo:".as_slice(), &numbered(i)].concat());
    }
    session.flush(Duration::from_secs(5)).unwrap();
    session.close();
    assert_eq!(echoed.join().unwrap().len(), 50);
}

#[test]
fn resets_resume_from_new_source_ports_with_each_message_once() {
    let cert = Cert::localhost();
    let server = Server::start(&cert, fast_timing(), Duration::from_secs(60));
    let proxy = FaultProxy::start(server.addr(), Mode::Tcp);
    let session = connect(client(proxy.url("wss"), &cert, fast_timing())).unwrap();
    let client_events = session.take_events().unwrap();
    let remote = server.accept();
    let server_events = remote.take_events().unwrap();
    let id = remote.id().to_string();
    let echoed = echo(remote);

    const N: usize = 300;
    let sender = {
        let session = session.clone();
        std::thread::spawn(move || {
            for i in 0..N {
                session.send(numbered(i)).unwrap();
                std::thread::sleep(Duration::from_millis(3));
            }
        })
    };
    for _ in 0..5 {
        std::thread::sleep(Duration::from_millis(150));
        proxy.reset();
    }
    sender.join().unwrap();
    for i in 0..N {
        assert_eq!(recv(&session), [b"echo:".as_slice(), &numbered(i)].concat());
    }
    session.flush(Duration::from_secs(10)).unwrap();
    assert!(session
        .recv_timeout(Duration::from_millis(300))
        .unwrap()
        .is_none());

    // One session, many connections, each from a new source port (TC-4).
    assert!(server
        .server
        .accept_timeout(Duration::from_millis(10))
        .unwrap()
        .is_none());
    let ports: Vec<u16> = events(&server_events)
        .into_iter()
        .filter_map(|e| match e {
            LinkEvent::Connected { peer, .. } => Some(peer.port()),
            _ => None,
        })
        .collect();
    assert!(ports.len() >= 2, "{ports:?}");
    let mut unique = ports.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), ports.len(), "{ports:?}");
    let resumed = events(&client_events)
        .iter()
        .filter(|e| matches!(e, LinkEvent::Connected { resumed: true, .. }))
        .count();
    assert!(resumed >= 1);
    assert_eq!(session.id(), id);

    session.close();
    let got = echoed.join().unwrap();
    assert_eq!(got, (0..N).map(numbered).collect::<Vec<_>>());
}

#[test]
fn a_silent_hang_is_detected_on_both_ends_and_resumed() {
    let cert = Cert::localhost();
    let timing = fast_timing();
    let server = Server::start(&cert, timing.clone(), Duration::from_secs(60));
    let proxy = FaultProxy::start(server.addr(), Mode::Tcp);
    let session = connect(client(proxy.url("wss"), &cert, timing.clone())).unwrap();
    let client_events = session.take_events().unwrap();
    let remote = server.accept();
    let server_events = remote.take_events().unwrap();
    session.send(b"before".to_vec()).unwrap();
    assert_eq!(
        remote
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap(),
        b"before"
    );

    proxy.set_hang(true);
    let hung_at = Instant::now();
    remote.send(b"during".to_vec()).unwrap();
    let disconnected = |rx: &Receiver<LinkEvent>| {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(LinkEvent::Disconnected { .. }) => return Instant::now(),
                Ok(_) => {}
                Err(_) => panic!("no disconnect within 5 s"),
            }
        }
    };
    let bound = timing.dead_after + Duration::from_millis(700);
    let client_saw = disconnected(&client_events) - hung_at;
    let server_saw = disconnected(&server_events) - hung_at;
    assert!(client_saw <= bound, "client took {client_saw:?}");
    assert!(server_saw <= bound, "server took {server_saw:?}");

    std::thread::sleep(Duration::from_millis(300));
    proxy.set_hang(false);
    assert_eq!(recv(&session), b"during");
    assert!(session
        .recv_timeout(Duration::from_millis(500))
        .unwrap()
        .is_none());
    session.send(b"after".to_vec()).unwrap();
    assert_eq!(recv(&remote), b"after");
    assert!(remote
        .recv_timeout(Duration::from_millis(300))
        .unwrap()
        .is_none());
}

#[test]
fn latency_and_jitter_keep_order() {
    let cert = Cert::localhost();
    let timing = Timing {
        dead_after: Duration::from_secs(3),
        ..fast_timing()
    };
    let server = Server::start(&cert, timing.clone(), Duration::from_secs(60));
    let proxy = FaultProxy::start(server.addr(), Mode::Tcp);
    proxy.set_latency(Duration::from_millis(100), Duration::from_millis(150));
    let session = connect(client(proxy.url("wss"), &cert, timing)).unwrap();
    let echoed = echo(server.accept());
    const N: usize = 100;
    for i in 0..N {
        session.send(numbered(i)).unwrap();
    }
    for i in 0..N {
        assert_eq!(recv(&session), [b"echo:".as_slice(), &numbered(i)].concat());
    }
    session.close();
    assert_eq!(echoed.join().unwrap().len(), N);
}

#[test]
fn a_restarted_server_answers_unknown() {
    let cert = Cert::localhost();
    let first = Server::start(&cert, fast_timing(), Duration::from_secs(600));
    let addr = first.addr();
    let proxy = FaultProxy::start(addr, Mode::Tcp);
    let session = connect(client(proxy.url("wss"), &cert, fast_timing())).unwrap();
    let remote = first.accept();

    first.server.shutdown();
    assert_eq!(
        remote.wait_ended(Duration::from_secs(2)),
        Some(SessionEnd::Shutdown)
    );
    drop(first);
    let second = Server::start_on(addr, &cert, fast_timing(), Duration::from_secs(600));

    // The new server has no record of the session: the client learns it is
    // gone, and its payload layer starts a new session and queries.
    assert_eq!(
        session.wait_ended(Duration::from_secs(10)),
        Some(SessionEnd::Expired(ExpiryReason::Unknown))
    );
    assert!(matches!(
        session.send(b"x".to_vec()),
        Err(Error::Ended(SessionEnd::Expired(ExpiryReason::Unknown)))
    ));
    let fresh = connect(client(proxy.url("wss"), &cert, fast_timing())).unwrap();
    assert_ne!(fresh.id(), session.id());
    assert_eq!(second.accept().id(), fresh.id());
}

#[test]
fn resuming_after_the_grace_period_answers_expired() {
    let cert = Cert::localhost();
    // The client's own give-up point, grace + dead_after, lies well after the
    // reconnect, so the answer comes from the server.
    let timing = Timing {
        heartbeat: Duration::from_millis(500),
        dead_after: Duration::from_secs(6),
        ..fast_timing()
    };
    let server = Server::start(&cert, timing.clone(), Duration::from_secs(1));
    let proxy = FaultProxy::start(server.addr(), Mode::Tcp);
    let session = connect(client(proxy.url("wss"), &cert, timing)).unwrap();
    let remote = server.accept();
    session.send(b"in flight".to_vec()).unwrap();
    assert_eq!(recv(&remote), b"in flight");
    remote.send(b"result".to_vec()).unwrap();
    session.flush(Duration::from_secs(5)).unwrap();

    proxy.set_refuse(true);
    proxy.reset();
    std::thread::sleep(Duration::from_millis(1600));
    proxy.set_refuse(false);

    assert_eq!(
        session.wait_ended(Duration::from_secs(10)),
        Some(SessionEnd::Expired(ExpiryReason::Expired))
    );
    assert_eq!(
        remote.wait_ended(Duration::from_secs(2)),
        Some(SessionEnd::Expired(ExpiryReason::Expired))
    );
    // A body that arrived before the end is still handed over.
    assert_eq!(
        session
            .recv_timeout(Duration::from_millis(100))
            .unwrap()
            .unwrap(),
        b"result"
    );
    assert!(matches!(
        session.recv_timeout(Duration::ZERO),
        Err(Error::Ended(_))
    ));
}

#[test]
fn an_unreachable_server_ends_the_session_after_the_grace_period() {
    let cert = Cert::localhost();
    let timing = Timing {
        dead_after: Duration::from_millis(500),
        ..fast_timing()
    };
    let server = Server::start(&cert, timing.clone(), Duration::from_millis(300));
    let proxy = FaultProxy::start(server.addr(), Mode::Tcp);
    let session = connect(client(proxy.url("wss"), &cert, timing)).unwrap();
    let _remote = server.accept();
    proxy.set_refuse(true);
    let cut = Instant::now();
    proxy.reset();
    assert_eq!(
        session.wait_ended(Duration::from_secs(5)),
        Some(SessionEnd::Expired(ExpiryReason::GraceElapsed))
    );
    assert!(cut.elapsed() >= Duration::from_millis(800));
}

#[test]
fn a_proxy_that_ends_tls_and_reencrypts_carries_the_session() {
    let server_cert = Cert::localhost();
    let proxy_cert = Cert::localhost();
    let server = Server::start(&server_cert, fast_timing(), Duration::from_secs(60));
    let proxy = FaultProxy::start(
        server.addr(),
        Mode::Tls {
            front: proxy_cert.server_config(),
            back: server_cert.client_config(),
        },
    );
    // The client trusts only the proxy's certificate, and the credential is
    // inside the protocol, so nothing ties it to the server's TLS (TC-5).
    let session = connect(client(proxy.url("wss"), &proxy_cert, fast_timing())).unwrap();
    let echoed = echo(server.accept());
    for i in 0..10 {
        session.send(numbered(i)).unwrap();
        assert_eq!(recv(&session), [b"echo:".as_slice(), &numbered(i)].concat());
    }
    proxy.reset();
    // Sent after the reset, so they travel on the resumed connection.
    for i in 10..20 {
        session.send(numbered(i)).unwrap();
    }
    for i in 10..20 {
        assert_eq!(recv(&session), [b"echo:".as_slice(), &numbered(i)].concat());
    }
    session.flush(Duration::from_secs(5)).unwrap();
    session.close();
    assert_eq!(echoed.join().unwrap().len(), 20);
    assert!(proxy.accepted() >= 2);
}

#[test]
fn a_wrong_credential_or_path_is_refused() {
    let cert = Cert::localhost();
    let server = Server::start(&cert, fast_timing(), Duration::from_secs(60));
    let mut bad = client(server.url(), &cert, fast_timing());
    bad.credential = "wrong".into();
    assert!(matches!(
        connect(bad),
        Err(Error::Rejected {
            code: RejectCode::Unauthorized,
            ..
        })
    ));
    let wrong_path = client(
        server.url().replace("/relay", "/other"),
        &cert,
        fast_timing(),
    );
    assert!(matches!(connect(wrong_path), Err(Error::Connect(_))));
    let untrusted = client(server.url(), &Cert::localhost(), fast_timing());
    assert!(matches!(connect(untrusted), Err(Error::Connect(_))));
}

#[test]
fn messages_are_capped_at_one_mebibyte() {
    let cert = Cert::localhost();
    let server = Server::start(&cert, fast_timing(), Duration::from_secs(60));
    let session = connect(client(server.url(), &cert, fast_timing())).unwrap();
    let remote = server.accept();
    assert!(matches!(
        session.send(vec![0; MAX_BODY + 1]),
        Err(Error::TooLarge { .. })
    ));
    let payload: Vec<u8> = (0..3 * MAX_BODY + 17).map(|i| i as u8).collect();
    let parts: Vec<&[u8]> = split_body(&payload, MAX_BODY).collect();
    assert_eq!(parts.len(), 4);
    for part in &parts {
        session.send(part.to_vec()).unwrap();
    }
    let mut got = Vec::new();
    for _ in 0..parts.len() {
        got.extend(recv(&remote));
    }
    assert_eq!(got, payload);
}

#[test]
fn closing_ends_both_sides() {
    let cert = Cert::localhost();
    let server = Server::start(&cert, fast_timing(), Duration::from_secs(60));
    let session = connect(client(server.url(), &cert, fast_timing())).unwrap();
    let remote = server.accept();
    session.close();
    assert_eq!(
        remote.wait_ended(Duration::from_secs(5)),
        Some(SessionEnd::Closed { by_peer: true })
    );
    assert_eq!(
        session.wait_ended(Duration::from_secs(5)),
        Some(SessionEnd::Closed { by_peer: false })
    );

    let other = connect(client(server.url(), &cert, fast_timing())).unwrap();
    drop(server.accept());
    assert_eq!(
        other.wait_ended(Duration::from_secs(5)),
        Some(SessionEnd::Closed { by_peer: true })
    );
}

#[test]
fn plain_websocket_serves_loopback() {
    let mut config = ServerConfig::new(StaticToken::new(TOKEN, "tester"));
    config.timing = fast_timing();
    let server = SessionServer::new(config).unwrap();
    let listening = server
        .listen(TcpListener::bind("127.0.0.1:0").unwrap())
        .unwrap();
    let mut c = ClientConfig::new(
        format!("ws://127.0.0.1:{}/relay", listening.local_addr().port()),
        TOKEN,
    );
    c.timing = fast_timing();
    let session = connect(c).unwrap();
    let remote = server
        .accept_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    session.send(b"hi".to_vec()).unwrap();
    assert_eq!(recv(&remote), b"hi");
}
