//! TL-7, opt-in: a remote session carrying a bulk transfer through the real
//! network rig under a chosen profile, with no scheduled fault. It counts the
//! reconnects and measures the throughput of a healthy slow link.
//!
//! ```sh
//! FLOW_RIG_PROFILE=N2 FLOW_RIG_MIB=256 \
//!   cargo test -p relay-acceptance --test flow_rig -- --ignored --nocapture
//! ```

use std::net::TcpListener;
use std::sync::Arc;
use std::time::{Duration, Instant};

use network_rig::{tls::Authority, Profile, Rig, RigConfig, DEFAULT_BROKER_NAME};
use remote_session::{
    connect, ClientConfig, LinkEvent, Resolver, ServerConfig, SessionServer, StaticToken, MAX_BODY,
};

fn var(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.into())
}

#[test]
#[ignore = "TL-7 rig: fetches and runs the pinned Toxiproxy and mitmproxy, and moves hundreds of MiB through a shaped link"]
fn a_bulk_transfer_on_a_healthy_slow_link_never_reconnects() {
    let profile = Profile::builtin(&var("FLOW_RIG_PROFILE", "N2")).unwrap();
    let mib: usize = var("FLOW_RIG_MIB", "256").parse().unwrap();
    // `FLOW_RIG_BYTES` sets the size exactly, for a 2 GB (2e9 bytes) pull.
    let total: usize = var("FLOW_RIG_BYTES", &(mib << 20).to_string())
        .parse()
        .unwrap();
    let authority = Authority::new("flow-rig broker authority").unwrap();
    let leaf = authority.issue(&[DEFAULT_BROKER_NAME]).unwrap();
    let mut server_config = ServerConfig::new(StaticToken::new("t", "tester"));
    if let Ok(w) = std::env::var("FLOW_RIG_BUFFER_MIB") {
        server_config.max_buffered = w.parse::<usize>().unwrap() << 20;
    }
    server_config.tls = Some(
        remote_session::tls::server_config(leaf.cert_pem.as_bytes(), leaf.key_pem.as_bytes())
            .unwrap(),
    );
    let server = SessionServer::new(server_config).unwrap();
    let listening = server
        .listen(TcpListener::bind("127.0.0.1:0").unwrap())
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut config = RigConfig::new(
        profile.clone(),
        listening.local_addr(),
        authority.cert_pem(),
        dir.path(),
    );
    config.seed = 1;
    let rig = Rig::start(config).unwrap();
    let side = rig.client_side().clone();
    let ca = std::fs::read_to_string(&side.ca_bundle).unwrap();

    let mut client = ClientConfig::new(&side.url, "t");
    client.tls = Some(remote_session::tls::client_config(&[ca.as_bytes()]).unwrap());
    client.attributes = serde_json::json!({"role": "client"});
    let chain = side.connect;
    let resolver: Resolver = Arc::new(move |_: &str, _: u16| Ok(vec![chain]));
    client.resolver = Some(resolver);
    let session = connect(client).unwrap();
    let events = session.take_events().unwrap();
    let remote = server
        .accept_timeout(Duration::from_secs(30))
        .unwrap()
        .expect("a session");

    let started = Instant::now();
    let server_events = remote.take_events().unwrap();
    let watch = std::thread::spawn(move || {
        let t0 = Instant::now();
        let mut seen = Vec::new();
        while let Ok(e) = server_events.recv() {
            eprintln!("server {:>6.1} s: {e:?}", t0.elapsed().as_secs_f64());
            let end = matches!(e, LinkEvent::Ended(_));
            seen.push(e);
            if end {
                break;
            }
        }
        seen
    });
    let sender = std::thread::spawn(move || {
        let body = vec![7u8; MAX_BODY];
        let mut sent = 0;
        while sent < total {
            let n = body.len().min(total - sent);
            remote.send(body[..n].to_vec()).unwrap();
            sent += n;
        }
        remote
    });
    let mut got = 0;
    let mut reasons: Vec<String> = Vec::new();
    let mut last_report = Instant::now();
    while got < total {
        let body = session
            .recv_timeout(Duration::from_secs(120))
            .unwrap()
            .expect("data within 120 s");
        got += body.len();
        for e in events.try_iter() {
            eprintln!("client {:>6.1} s: {e:?}", started.elapsed().as_secs_f64());
            if let LinkEvent::Disconnected { reason } = e {
                reasons.push(reason);
            }
        }
        if last_report.elapsed() >= Duration::from_secs(10) {
            last_report = Instant::now();
            eprintln!(
                "{:>5.0} s: {} MiB",
                started.elapsed().as_secs_f64(),
                got >> 20
            );
        }
    }
    let elapsed = started.elapsed();
    let _remote = sender.join().unwrap();
    reasons.extend(events.try_iter().filter_map(|e| match e {
        LinkEvent::Disconnected { reason } => Some(reason),
        _ => None,
    }));
    let rate = total as f64 / elapsed.as_secs_f64() / 1e6;
    eprintln!(
        "{}: {total} bytes in {:.1} s = {rate:.2} MB/s, {} reconnects: {reasons:?}",
        profile.id,
        elapsed.as_secs_f64(),
        reasons.len()
    );
    eprintln!(
        "{}: 2 GB at this rate takes {:.1} min",
        profile.id,
        2e9 / (rate * 1e6) / 60.0
    );
    session.close();
    drop(_remote);
    let _ = watch.join();
    assert!(
        reasons.is_empty(),
        "a healthy slow link reconnected: {reasons:?}"
    );
}
