//! Server answers to hellos that a well-behaved client never sends, driven
//! with raw hellos over plain WebSocket on loopback.

use std::net::TcpListener;
use std::time::Duration;

use crate::client::{dial, Reply, Target};
use crate::server::{AuthRequest, Rejection};
use crate::wire::{Hello, RejectCode, PROTOCOL_VERSION};
use crate::{tls, ServerConfig, SessionServer, Timing};

fn timing() -> Timing {
    Timing {
        heartbeat: Duration::from_millis(100),
        dead_after: Duration::from_secs(1),
        handshake_timeout: Duration::from_secs(5),
        poll_interval: Duration::from_millis(5),
    }
}

/// A server where credential `token-X` authenticates principal `X`.
fn server() -> (SessionServer, crate::Listening, Target) {
    let auth = |r: &AuthRequest<'_>| {
        r.credential
            .strip_prefix("token-")
            .map(str::to_string)
            .ok_or_else(|| Rejection::unauthorized("bad credential"))
    };
    let mut config = ServerConfig::new(auth);
    config.timing = timing();
    let server = SessionServer::new(config).unwrap();
    let listening = server
        .listen(TcpListener::bind("127.0.0.1:0").unwrap())
        .unwrap();
    let url = format!("ws://127.0.0.1:{}/relay", listening.local_addr().port());
    (server, listening, Target::parse(&url).unwrap())
}

fn hello(session: Option<&str>, received: u64, who: &str) -> Hello {
    Hello {
        version: PROTOCOL_VERSION,
        session: session.map(str::to_string),
        received,
        credential: format!("token-{who}"),
        attributes: serde_json::Value::Null,
    }
}

fn refused(reply: Reply) -> RejectCode {
    match reply {
        Reply::Refused(crate::Error::Rejected { code, .. }) => code,
        Reply::Welcome(w) => panic!("welcomed: {w:?}"),
        Reply::Expired(r) => panic!("expired: {r:?}"),
        Reply::Refused(e) => panic!("refused otherwise: {e:?}"),
    }
}

#[test]
fn a_session_resumes_only_for_its_principal() {
    let (server, _l, target) = server();
    let tls = tls::default_client_config();
    let (_ws, _, reply) = dial(&target, &tls, &timing(), hello(None, 0, "alice")).unwrap();
    let Reply::Welcome(w) = reply else {
        panic!("no welcome")
    };
    let session = server
        .accept_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    assert_eq!(session.principal(), Some("alice"));

    let (_, _, reply) = dial(
        &target,
        &tls,
        &timing(),
        hello(Some(&w.session), 0, "mallory"),
    )
    .unwrap();
    assert_eq!(refused(reply), RejectCode::Forbidden);
    let (_, _, reply) = dial(
        &target,
        &tls,
        &timing(),
        hello(Some(&w.session), 0, "alice"),
    )
    .unwrap();
    assert!(matches!(reply, Reply::Welcome(w2) if w2.resumed && w2.session == w.session));
}

#[test]
fn malformed_hellos_are_refused() {
    let (_server, _l, target) = server();
    let tls = tls::default_client_config();
    let mut old = hello(None, 0, "alice");
    old.version = 99;
    let (_, _, reply) = dial(&target, &tls, &timing(), old).unwrap();
    assert_eq!(refused(reply), RejectCode::UnsupportedVersion);
    let (_, _, reply) = dial(&target, &tls, &timing(), hello(None, 5, "alice")).unwrap();
    assert_eq!(refused(reply), RejectCode::Protocol);
    let mut nobody = hello(None, 0, "x");
    nobody.credential = "nothing".into();
    let (_, _, reply) = dial(&target, &tls, &timing(), nobody).unwrap();
    assert_eq!(refused(reply), RejectCode::Unauthorized);
}

#[test]
fn a_resume_claiming_more_than_was_sent_is_a_protocol_error() {
    let (_server, _l, target) = server();
    let tls = tls::default_client_config();
    let (_ws, _, reply) = dial(&target, &tls, &timing(), hello(None, 0, "alice")).unwrap();
    let Reply::Welcome(w) = reply else {
        panic!("no welcome")
    };
    let (_, _, reply) = dial(
        &target,
        &tls,
        &timing(),
        hello(Some(&w.session), 7, "alice"),
    )
    .unwrap();
    assert_eq!(refused(reply), RejectCode::Protocol);
}
