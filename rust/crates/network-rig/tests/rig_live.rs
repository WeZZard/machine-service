//! The rig's self-tests (TL-7): each fault, shown on a plain WebSocket echo
//! server through the real chain.
//!
//! They are `#[ignore]`d because they fetch the pinned Toxiproxy and
//! mitmproxy into the cache (about 60 MB on macOS, 110 MB on Linux, once)
//! and run them. Run them with
//!
//! ```sh
//! cargo test -p network-rig --test rig_live -- --ignored
//! ```
//!
//! The cache is `network-rig-cache` in the target folder; `NETWORK_RIG_CACHE`
//! moves it. Each rig's files stay in a temporary
//! run directory, which `NETWORK_RIG_KEEP=1` keeps for inspection.

use std::time::{Duration, Instant};

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};

use network_rig::cell::{self, Cell, ClientPlatform, Scenario, StandIn, TargetKind, Verdict};
use network_rig::client::{Dial, Outcome, Session};
use network_rig::stage::Hop;
use network_rig::standin::{EchoServer, Visit};
use network_rig::{
    tls, AddressChange, Fault, Phase, Profile, Rig, RigConfig, Select, Side, Stream,
};
use rustls::pki_types::{IpAddr as PkiIp, ServerName};

const IO: Duration = Duration::from_secs(10);
const MIB: usize = 1 << 20;

struct Chain {
    server: EchoServer,
    rig: Rig,
    _dir: Option<tempfile::TempDir>,
}

fn chain_with(profile: Profile, tweak: impl FnOnce(&mut RigConfig)) -> Chain {
    let server = EchoServer::start(network_rig::DEFAULT_BROKER_NAME).expect("start the stand-in");
    let dir = tempfile::Builder::new()
        .prefix("network-rig-")
        .tempdir()
        .expect("run directory");
    let mut config = RigConfig::new(profile, server.addr(), server.ca_pem(), dir.path());
    tweak(&mut config);
    let rig = Rig::start(config).expect("start the rig");
    let keep = std::env::var("NETWORK_RIG_KEEP").is_ok_and(|v| v == "1");
    let dir = if keep {
        eprintln!("rig run directory kept at {}", dir.keep().display());
        None
    } else {
        Some(dir)
    };
    Chain {
        server,
        rig,
        _dir: dir,
    }
}

fn chain(id: &str) -> Chain {
    chain_with(Profile::builtin(id).unwrap(), |_| {})
}

fn dial(chain: &Chain) -> Dial {
    Dial::through(chain.rig.client_side()).unwrap()
}

fn open(chain: &Chain) -> Session {
    Session::open(&dial(chain), IO).expect("open a session through the chain")
}

#[test]
#[ignore = "TL-7 rig: fetches and runs the pinned Toxiproxy and mitmproxy; see the module documentation"]
fn n0_carries_an_echo_to_the_broker_by_name() {
    let chain = chain("N0");
    let client = chain.rig.client_side();
    assert!(client.url.starts_with("wss://broker.relay-rig.test:"));
    assert_eq!(
        client.resolve(),
        format!("broker.relay-rig.test:{}:127.0.0.1", client.port)
    );
    let mut session = open(&chain);
    assert_eq!(session.echo(b"hello"), Outcome::Ok);
    let visits = chain.server.visits();
    let visit = visits.last().unwrap();
    assert!(visit.upgraded);
    assert_eq!(visit.path.as_deref(), Some("/relay"));
    assert_eq!(visit.sni.as_deref(), Some("broker.relay-rig.test"));
    let host = visit.host.as_deref().unwrap();
    assert!(
        host.starts_with("broker.relay-rig.test"),
        "the Host header names the broker: {host}"
    );
}

#[test]
#[ignore = "TL-7 rig: fetches and runs the pinned Toxiproxy and mitmproxy; see the module documentation"]
fn tls_ends_in_the_middle() {
    let chain = chain("N1");
    let session = open(&chain);
    // The client trusts the edge's authority only, so the handshake proves
    // that the certificate it saw is the edge's, not the broker's.
    let presented = session.peer_certificates();
    assert!(!presented.is_empty());
    drop(session);

    // A client that trusts only the broker's authority is refused: the
    // broker's own certificate never reaches the client.
    let mut broker_only = dial(&chain);
    broker_only.tls = tls::client_config(chain.server.ca_pem()).unwrap();
    match Session::open(&broker_only, IO) {
        Err(Outcome::Tls(_)) => {}
        other => panic!("a client trusting only the broker authority must fail TLS: {other:?}"),
    }

    // The broker saw a second TLS connection, opened by the edge, for its name.
    let visits = chain.server.visits();
    assert!(visits
        .iter()
        .any(|v| v.upgraded && v.sni.as_deref() == Some("broker.relay-rig.test")));
    assert!(visits
        .iter()
        .all(|v| v.peer.unwrap().port() != chain.rig.client_side().port));
}

#[test]
#[ignore = "TL-7 rig: fetches and runs the pinned Toxiproxy and mitmproxy; see the module documentation"]
fn the_edge_serves_the_broker_only_by_its_name() {
    let chain = chain("N1");
    let mut other = dial(&chain);
    other.server_name = ServerName::try_from("other.relay-rig.test").unwrap();
    other.url = format!(
        "wss://other.relay-rig.test:{}/relay",
        chain.rig.client_side().port
    );
    assert_eq!(
        Session::open(&other, IO).unwrap_err(),
        Outcome::Refused(421)
    );

    let mut by_address = dial(&chain);
    by_address.server_name = ServerName::IpAddress(PkiIp::from(std::net::Ipv4Addr::LOCALHOST));
    by_address.url = format!("wss://127.0.0.1:{}/relay", chain.rig.client_side().port);
    assert_eq!(
        Session::open(&by_address, IO).unwrap_err(),
        Outcome::Refused(421)
    );

    assert!(
        chain.server.visits().iter().all(|v| !v.upgraded),
        "nothing reached the broker"
    );
}

#[test]
#[ignore = "TL-7 rig: fetches and runs the pinned Toxiproxy and mitmproxy; see the module documentation"]
fn a_reset_resets_the_connection() {
    let chain = chain("N1");
    for stream in [Stream::Downstream, Stream::Upstream] {
        let mut session = open(&chain);
        assert_eq!(session.echo(b"before"), Outcome::Ok);
        chain.rig.control().reset_peer(stream).unwrap();
        // Toxiproxy cuts a connection that was open before the reset as soon
        // as it carries data in that direction ...
        assert_eq!(
            session.echo(b"during"),
            Outcome::Closed,
            "{stream:?}: an open connection"
        );
        // ... and resets with a TCP RST every connection dialled while the
        // reset is armed.
        match Session::open(&dial(&chain), IO) {
            Err(Outcome::Reset) => {}
            other => panic!("{stream:?}: a new connection must be reset: {other:?}"),
        }
        chain
            .rig
            .control()
            .revert(&Fault::Reset {
                stream,
                window_ms: 0,
            })
            .unwrap();
        assert_eq!(open(&chain).echo(b"after"), Outcome::Ok);
    }
}

#[test]
#[ignore = "TL-7 rig: fetches and runs the pinned Toxiproxy and mitmproxy; see the module documentation"]
fn a_blackhole_holds_data_without_closing_the_connection() {
    let chain = chain("N1");
    for stream in [Stream::Upstream, Stream::Downstream] {
        let mut session = open(&chain);
        assert_eq!(session.echo(b"before"), Outcome::Ok);
        chain.rig.control().blackhole(stream).unwrap();
        session.set_read_timeout(Duration::from_secs(3));
        assert_eq!(
            session.echo(b"lost"),
            Outcome::Stalled,
            "{stream:?}: nothing comes back"
        );
        // The connection stays open and silent for as long as the blackhole
        // lasts: no data, no FIN, no RST.
        assert_eq!(session.ping(), Outcome::Ok);
        assert_eq!(
            session.wait_for_end(Duration::from_secs(3)),
            Outcome::Stalled,
            "{stream:?}"
        );
        // Toxiproxy drops the held data and closes the connection when the
        // blackhole ends, as a dead path does; a new connection works.
        let fault = Fault::Blackhole {
            stream,
            duration_ms: 0,
        };
        chain.rig.control().revert(&fault).unwrap();
        let end = session.wait_for_end(IO);
        assert!(
            matches!(end, Outcome::Closed | Outcome::Reset),
            "{stream:?}: {end:?}"
        );
        assert_eq!(open(&chain).echo(b"after"), Outcome::Ok);
    }
}

#[test]
#[ignore = "TL-7 rig: fetches and runs the pinned Toxiproxy and mitmproxy; see the module documentation"]
fn n2_latency_stays_within_its_bounds() {
    let chain = chain("N2");
    let mut session = open(&chain);
    let mut rtts = Vec::new();
    for round in 0..20 {
        let start = Instant::now();
        assert_eq!(
            session.echo(format!("round {round}").as_bytes()),
            Outcome::Ok
        );
        rtts.push(start.elapsed());
    }
    let ms: Vec<u128> = rtts.iter().map(Duration::as_millis).collect();
    eprintln!("N2 round trips (ms): {ms:?}");
    // 300 ms to 1 s of added round trip (TC-7), plus the stand-in's and the
    // proxies' own time.
    assert!(
        rtts.iter().all(|rtt| *rtt >= Duration::from_millis(300)),
        "{ms:?}"
    );
    assert!(
        rtts.iter().all(|rtt| *rtt <= Duration::from_millis(1150)),
        "{ms:?}"
    );
    let spread = ms.iter().max().unwrap() - ms.iter().min().unwrap();
    assert!(
        spread >= 100,
        "jitter should spread the round trips: {ms:?}"
    );
}

#[test]
#[ignore = "TL-7 rig: fetches and runs the pinned Toxiproxy and mitmproxy; see the module documentation"]
fn an_idle_connection_closes_and_pings_keep_it_open() {
    let idle = 3;
    let chain = chain_with(Profile::builtin("N1").unwrap(), |config| {
        config.idle_close_s = Some(idle)
    });
    let mut session = open(&chain);
    // Pings more often than the idle limit keep the connection open.
    for _ in 0..6 {
        std::thread::sleep(Duration::from_secs(1));
        assert_eq!(session.ping(), Outcome::Ok);
    }
    // The edge counts from the last byte it carried, which comes after this
    // instant, so the close must come at least the idle limit later.
    let quiet = Instant::now();
    assert_eq!(session.echo(b"still open"), Outcome::Ok);
    let end = session.wait_for_end(Duration::from_secs(idle * 4));
    let after = quiet.elapsed();
    assert!(
        matches!(end, Outcome::Closed | Outcome::Reset),
        "the edge closes it: {end:?}"
    );
    assert!(
        after >= Duration::from_secs(idle),
        "closed too early: {after:?}"
    );
    assert!(
        after <= Duration::from_secs(idle) + Duration::from_millis(1500),
        "closed late: {after:?}"
    );
}

#[test]
#[ignore = "TL-7 rig: runs for about 105 s; fetches and runs the pinned Toxiproxy and mitmproxy; see the module documentation"]
fn an_idle_connection_closes_at_the_default_100_s() {
    let chain = chain("N1");
    assert_eq!(chain.rig.profile().edge.idle_close_s, 100);
    let mut quiet = open(&chain);
    let mut beating = open(&chain);
    assert_eq!(beating.echo(b"start"), Outcome::Ok);
    // The edge counts from the last byte it carried, which comes after this
    // instant, so the close must come at least 100 s later.
    let start = Instant::now();
    assert_eq!(quiet.echo(b"start"), Outcome::Ok);
    let heartbeats = std::thread::spawn(move || {
        // A heartbeat every 30 s (TC-3) keeps a connection past 100 s.
        for _ in 0..3 {
            std::thread::sleep(Duration::from_secs(30));
            assert_eq!(beating.ping(), Outcome::Ok);
        }
        std::thread::sleep(Duration::from_secs(15));
        beating.echo(b"after 105 s")
    });
    let end = quiet.wait_for_end(Duration::from_secs(130));
    let after = start.elapsed();
    assert!(matches!(end, Outcome::Closed | Outcome::Reset), "{end:?}");
    assert!(
        after >= Duration::from_secs(100) && after <= Duration::from_secs(102),
        "{after:?}"
    );
    assert_eq!(heartbeats.join().unwrap(), Outcome::Ok);
}

#[test]
#[ignore = "TL-7 rig: fetches and runs the pinned Toxiproxy and mitmproxy; see the module documentation"]
fn a_message_over_1_mib_is_refused() {
    let chain = chain("N1");
    let mut session = open(&chain);
    let limit = vec![7u8; MIB];
    assert_eq!(
        session.echo(&limit),
        Outcome::Ok,
        "a message of exactly 1 MiB passes"
    );
    let over = vec![7u8; MIB + 1];
    let refused = session.echo(&over);
    assert!(
        matches!(
            refused,
            Outcome::CloseFrame(1009) | Outcome::Closed | Outcome::Reset
        ),
        "the edge closes on a message over 1 MiB: {refused:?}"
    );
    let largest = chain
        .server
        .visits()
        .iter()
        .map(|v| v.largest_message)
        .max()
        .unwrap();
    assert_eq!(
        largest, MIB,
        "the oversized message never reached the broker"
    );

    // A message of 1 MiB + 1 sent in fragments is refused as soon as the
    // edge holds more than 1 MiB of it, before it is whole.
    let mut session = open(&chain);
    let frames = [vec![1u8; MIB / 2], vec![2u8; MIB / 2], vec![3u8; 1]];
    use tungstenite::protocol::frame::{coding::Data, coding::OpCode, Frame};
    for (index, part) in frames.iter().enumerate() {
        let opcode = if index == 0 {
            OpCode::Data(Data::Binary)
        } else {
            OpCode::Data(Data::Continue)
        };
        let frame = Frame::message(part.clone(), opcode, index == frames.len() - 1);
        if session
            .socket
            .send(tungstenite::Message::Frame(frame))
            .is_err()
        {
            break;
        }
    }
    let end = session.wait_for_end(IO);
    assert!(
        matches!(
            end,
            Outcome::CloseFrame(1009) | Outcome::Closed | Outcome::Reset
        ),
        "{end:?}"
    );
    let largest = chain
        .server
        .visits()
        .iter()
        .map(|v| v.largest_message)
        .max()
        .unwrap();
    assert_eq!(largest, MIB);
}

#[test]
#[ignore = "TL-7 rig: fetches and runs the pinned Toxiproxy and mitmproxy; see the module documentation"]
fn n3_starts_with_its_toxics_and_its_seeded_schedule() {
    let chain = chain_with(Profile::builtin("N3").unwrap(), |config| config.seed = 42);
    let mut toxics = chain.rig.control().toxics().unwrap();
    toxics.sort();
    let mut want: Vec<String> = chain
        .rig
        .profile()
        .toxics
        .iter()
        .map(|t| t.name.clone())
        .collect();
    want.sort();
    assert_eq!(toxics, want);
    let schedule = chain.rig.schedule();
    assert_eq!(schedule.seed, 42);
    assert!(!schedule.events.is_empty());
    let record: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(chain.rig.run_dir().join("rig.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(record["seed"], 42);
    assert_eq!(
        record["schedule"]["events"][0],
        serde_json::to_value(schedule.events[0]).unwrap()
    );
    assert_eq!(open(&chain).echo(b"through N3"), Outcome::Ok);
}

/// A chaos menu fast enough for a test: a fault about every 2 seconds.
const FAST_CHAOS: &str = r#"{
  "id": "T-chaos",
  "name": "Fast chaos",
  "edge": { "idle_close_s": 100, "max_message_bytes": 1048576 },
  "chaos": {
    "first_after_ms": 1500,
    "gap_ms": [1500, 2500],
    "faults": [
      { "weight": 2, "fault": "reset", "window_ms": [1500, 2000] },
      { "weight": 2, "fault": "blackhole", "duration_ms": [1500, 2000] },
      { "weight": 1, "fault": "limit_data", "bytes": [64, 512], "window_ms": [1500, 2000] },
      { "weight": 1, "fault": "address_change" },
      { "weight": 2, "fault": "cut" }
    ]
  }
}"#;

/// What the probe saw at one fault: the event, its kind, and the outcomes.
type Seen = (usize, &'static str, Vec<Outcome>);

/// The stand-in's record of the broker leg of `session`.
fn broker_visit(chain: &Chain, session: &Session) -> Option<Visit> {
    let local = session.local_addr()?;
    let source = chain
        .rig
        .control()
        .connections()
        .into_iter()
        .find(|connection| connection.client == local)?
        .broker_source?;
    chain
        .server
        .visits()
        .into_iter()
        .find(|visit| visit.peer == Some(source))
}

/// How the broker leg whose source was `source` ended, once it has.
fn broker_end(chain: &Chain, source: SocketAddr, within: Duration) -> Option<Outcome> {
    let deadline = Instant::now() + within;
    loop {
        let end = chain
            .server
            .visits()
            .into_iter()
            .find(|visit| visit.peer == Some(source))
            .and_then(|visit| visit.end);
        if end.is_some() || Instant::now() >= deadline {
            return end;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Play a seeded schedule and probe the chain at each fault.
///
/// At each fault the probe uses the connection that was open before it: an
/// address change and a cut are watched without writing, so the probe never
/// races the close, and a cut also records how the broker saw its leg end;
/// every other fault is met with an echo. While a reset is armed, the probe
/// also dials a new connection, which must be reset. (A connection
/// dialled under a data limit is cut in the middle of its TLS handshake, and
/// whether that cut is a FIN or a RST depends on whether the handshake's
/// last bytes had arrived, so it is not probed.)
fn failures_for(seed: u64) -> (Vec<Seen>, network_rig::Schedule) {
    let profile = Profile::parse(FAST_CHAOS).unwrap();
    let mut chain = chain_with(profile, |config| {
        config.seed = seed;
        config.horizon_ms = 24_000;
    });
    let schedule = chain.rig.schedule().clone();
    let payload = vec![9u8; 2048];
    let mut session = open(&chain);
    let mut leg = broker_visit(&chain, &session).and_then(|visit| visit.peer);
    let notes = chain.rig.play();
    let mut seen = Vec::new();
    for note in notes.iter() {
        match note.phase {
            Phase::Applied => {
                let fault = note.event.fault;
                session.set_read_timeout(Duration::from_millis(1000));
                let mut outcomes = vec![match fault {
                    Fault::AddressChange | Fault::Cut { .. } => {
                        session.wait_for_end(Duration::from_millis(1000))
                    }
                    _ => session.echo(&payload),
                }];
                if let Fault::Cut { .. } = fault {
                    let source = leg.expect("the probe's broker leg");
                    let end = broker_end(&chain, source, Duration::from_secs(2));
                    outcomes.push(end.unwrap_or(Outcome::Stalled));
                }
                if matches!(fault, Fault::Reset { .. }) {
                    let dialled = Session::open(&dial(&chain), Duration::from_millis(1000))
                        .map(|mut fresh| fresh.echo(&payload));
                    outcomes.push(dialled.unwrap_or_else(|outcome| outcome));
                }
                seen.push((note.event.index, fault.kind(), outcomes));
            }
            Phase::Reverted => {
                session = open(&chain);
                leg = broker_visit(&chain, &session).and_then(|visit| visit.peer);
            }
            Phase::Failed(why) => panic!("the rig failed event {}: {why}", note.event.index),
        }
    }
    (seen, schedule)
}

#[test]
#[ignore = "TL-7 rig: fetches and runs the pinned Toxiproxy and mitmproxy; see the module documentation"]
fn the_same_seed_replays_the_same_failures() {
    // Seed 14 draws every kind of fault from the fast menu within 24 s, and
    // a cut on each side.
    let seed = 14;
    let (first, schedule) = failures_for(seed);
    eprintln!("failures for seed {seed}: {first:?}");
    let (again, replayed) = failures_for(seed);
    assert_eq!(schedule, replayed);
    assert_eq!(first.len(), schedule.events.len());
    for kind in ["reset", "blackhole", "limit_data", "address_change", "cut"] {
        assert!(
            first.iter().any(|(_, k, _)| *k == kind),
            "no {kind} in {first:?}"
        );
    }
    assert!(
        first
            .iter()
            .all(|(_, _, outcomes)| outcomes[0] != Outcome::Ok),
        "every fault shows: {first:?}"
    );
    // A cut resets the probe's connection on the side the seed chose.
    for (event, (_, _, outcomes)) in schedule.events.iter().zip(&first) {
        if let Fault::Cut { side, .. } = event.fault {
            let (client, broker) = (&outcomes[0], &outcomes[1]);
            if matches!(side, Side::Client | Side::Both) {
                assert_eq!(*client, Outcome::Reset, "event {}: {side:?}", event.index);
            }
            if matches!(side, Side::Broker | Side::Both) {
                assert_eq!(*broker, Outcome::Reset, "event {}: {side:?}", event.index);
            }
        }
    }
    assert_eq!(first, again, "the same seed must give the same failures");
}

#[test]
#[ignore = "TL-7 rig: fetches and runs the pinned Toxiproxy and mitmproxy; see the module documentation"]
fn a_cut_resets_an_open_websocket_mid_transfer_on_the_chosen_side() {
    let chain = chain("N1");
    let control = chain.rig.control();
    for side in [Side::Client, Side::Broker, Side::Both] {
        let session = open(&chain);
        let local = session.local_addr().unwrap();
        let connection = control
            .connections()
            .into_iter()
            .find(|connection| connection.client == local)
            .expect("the stage lists the open connection");
        let source = connection
            .broker_source
            .expect("the stage knows the connection's broker leg");
        // Keep 256 KiB messages flowing both ways until the connection ends.
        let transfer = std::thread::spawn(move || {
            let mut session = session;
            let payload = vec![3u8; 256 * 1024];
            loop {
                match session.echo(&payload) {
                    Outcome::Ok => continue,
                    other => return other,
                }
            }
        });
        let deadline = Instant::now() + IO;
        while chain
            .server
            .visits()
            .iter()
            .find(|visit| visit.peer == Some(source))
            .is_none_or(|visit| visit.messages < 3)
        {
            assert!(
                Instant::now() < deadline,
                "{side:?}: the transfer never started"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            control.cut(Select::Id(connection.id), side),
            [connection.id]
        );
        let client = transfer.join().unwrap();
        let broker = broker_end(&chain, source, IO);
        eprintln!("cut {side:?}: the client saw {client:?}, the broker saw {broker:?}");
        // The chosen side sees ECONNRESET, not a clean close. The other side
        // sees the chain close; a proxy that closes with data unread may
        // turn that close into a reset too, so it is not checked.
        if matches!(side, Side::Client | Side::Both) {
            assert_eq!(client, Outcome::Reset, "{side:?}: the client");
        }
        if matches!(side, Side::Broker | Side::Both) {
            assert_eq!(broker, Some(Outcome::Reset), "{side:?}: the broker");
        }
        // The cut waits for the hop it reset; the rest of the chain closes
        // the connection's other hop soon after.
        let deadline = Instant::now() + IO;
        while control.connections().iter().any(|c| c.id == connection.id) {
            assert!(
                Instant::now() < deadline,
                "{side:?}: the connection stays open"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    // A cut of every connection resets each of them, and the chain still
    // carries a new one.
    let mut sessions: Vec<Session> = (0..3).map(|_| open(&chain)).collect();
    let ids: Vec<u64> = control.connections().iter().map(|c| c.id).collect();
    assert_eq!(control.cut(Select::All, Side::Client), ids);
    for session in &mut sessions {
        assert_eq!(session.wait_for_end(IO), Outcome::Reset);
    }
    assert_eq!(open(&chain).echo(b"after"), Outcome::Ok);
}

#[test]
#[ignore = "TL-7 rig: fetches and runs the pinned Toxiproxy and mitmproxy; see the module documentation"]
fn an_address_change_reaches_the_broker_from_a_new_source() {
    let chain = chain("N1");
    let control = chain.rig.control();
    if cfg!(target_os = "linux") {
        assert_eq!(control.address_change(), AddressChange::SourceAddress);
    }
    let mut sources = Vec::new();
    for round in 0..3 {
        let mut session = open(&chain);
        assert_eq!(session.echo(b"hello"), Outcome::Ok);
        sources.push(broker_visit(&chain, &session).unwrap().peer.unwrap());
        if round < 2 {
            control.change_address().unwrap();
            let end = session.wait_for_end(IO);
            assert!(matches!(end, Outcome::Closed | Outcome::Reset), "{end:?}");
        }
    }
    eprintln!(
        "address change ({:?}): the broker saw {sources:?}",
        control.address_change()
    );
    for pair in sources.windows(2) {
        assert_ne!(pair[0], pair[1]);
        match control.address_change() {
            AddressChange::SourceAddress => assert_ne!(pair[0].ip(), pair[1].ip()),
            AddressChange::SourcePort => assert_eq!(pair[0].ip(), pair[1].ip()),
        }
    }
}

/// A plain TCP echo server for the latency measurement.
fn tcp_echo() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let _ = stream.set_nodelay(true);
            std::thread::spawn(move || {
                let mut buffer = [0u8; 4096];
                while let Ok(read @ 1..) = stream.read(&mut buffer) {
                    if stream.write_all(&buffer[..read]).is_err() {
                        return;
                    }
                }
            });
        }
    });
    addr
}

/// The median and 99th percentile of `rounds` round trips of 64 bytes.
fn round_trips(addr: SocketAddr, rounds: usize) -> (Duration, Duration) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_nodelay(true).unwrap();
    let (message, mut back) = ([7u8; 64], [0u8; 64]);
    let mut times: Vec<Duration> = (0..rounds + 100)
        .map(|_| {
            let start = Instant::now();
            stream.write_all(&message).unwrap();
            stream.read_exact(&mut back).unwrap();
            start.elapsed()
        })
        .skip(100)
        .collect();
    times.sort();
    (times[rounds / 2], times[rounds * 99 / 100])
}

#[test]
#[ignore = "TL-7 rig: a timing measurement of the reset stage; see the module documentation"]
fn the_reset_stage_adds_negligible_latency() {
    // Both of the stage's hops in a row, as they stand in the chain.
    let server = tcp_echo();
    let back = Hop::start(server).unwrap();
    let front = Hop::start(back.addr()).unwrap();
    let rounds = 2000;
    let (direct, direct_p99) = round_trips(server, rounds);
    let (staged, staged_p99) = round_trips(front.addr(), rounds);
    eprintln!(
        "round trip of 64 bytes over loopback, {rounds} rounds: \
         direct median {direct:?} p99 {direct_p99:?}; \
         through both hops median {staged:?} p99 {staged_p99:?}; \
         the stage adds {:?} at the median",
        staged.saturating_sub(direct)
    );
    // N2 alone adds 300 ms to 1 s; the stage must stay far below a millisecond.
    assert!(staged.saturating_sub(direct) < Duration::from_millis(1));
}

#[test]
#[ignore = "TL-7 rig: fetches and runs the pinned Toxiproxy and mitmproxy; see the module documentation"]
fn the_cell_runner_runs_a_stand_in_cell() {
    let client = ClientPlatform::this_machine().expect("a macOS or Linux client");
    let cell = Cell {
        client,
        target: TargetKind::UbuntuVm,
    };
    let evidence = tempfile::tempdir().unwrap();
    let scenarios = [
        Scenario::<StandIn> {
            id: "echo",
            profiles: &["N0", "N1", "N2", "N3"],
            run: |context| {
                let dial = Dial::through(&context.client).map_err(|e| e.to_string())?;
                let mut session = Session::open(&dial, IO).map_err(|e| format!("{e:?}"))?;
                match session.echo(b"cell") {
                    Outcome::Ok => Ok(()),
                    other => Err(format!("{other:?}").into()),
                }
            },
        },
        Scenario {
            id: "edge-only",
            profiles: &["N1", "N2", "N3"],
            run: |_| Ok(()),
        },
    ];
    let profile = Profile::builtin("N0").unwrap();
    let mut target = StandIn::default();
    let report =
        cell::run_cell(cell, &profile, 1, &mut target, &scenarios, evidence.path()).unwrap();
    assert_eq!(report.results[0].verdict, Verdict::Passed, "{report:?}");
    assert!(matches!(report.results[1].verdict, Verdict::Skipped(_)));
    assert!(report.run_dir.join("report.json").is_file());
    assert!(report.run_dir.join("rig.json").is_file());
    assert!(target.server().unwrap().visits().iter().any(|v| v.upgraded));
}
