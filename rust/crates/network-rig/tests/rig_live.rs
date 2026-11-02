//! The rig's self-tests (TL-7, M1): each fault, shown on a plain WebSocket
//! echo server through the real chain.
//!
//! They are `#[ignore]`d because they fetch the pinned Toxiproxy and
//! mitmproxy into the cache (about 60 MB on macOS, 110 MB on Linux, once)
//! and run them. Run them with
//!
//! ```sh
//! cargo test -p network-rig --test rig_live -- --ignored
//! ```
//!
//! `NETWORK_RIG_CACHE` moves the cache. Each rig's files stay in a temporary
//! run directory, which `NETWORK_RIG_KEEP=1` keeps for inspection.

use std::time::{Duration, Instant};

use network_rig::cell::{self, Cell, ClientPlatform, Scenario, StandIn, TargetKind, Verdict};
use network_rig::client::{Dial, Outcome, Session};
use network_rig::standin::EchoServer;
use network_rig::{tls, Fault, Phase, Profile, Rig, RigConfig, Stream};
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
      { "weight": 1, "fault": "address_change" }
    ]
  }
}"#;

/// What the probe saw at one fault: the event, its kind, and the outcomes.
type Seen = (usize, &'static str, Vec<Outcome>);

/// Play a seeded schedule and probe the chain at each fault.
///
/// At each fault the probe uses the connection that was open before it: an
/// address change is watched without writing, so the probe never races the
/// close; every other fault is met with an echo. While a reset is armed, the
/// probe also dials a new connection, which must be reset. (A connection
/// dialled under a data limit is cut in the middle of its TLS handshake, and
/// whether that cut is a FIN or a RST depends on whether the handshake's
/// last bytes had arrived, so it is not probed.)
fn failures_for(seed: u64) -> (Vec<Seen>, network_rig::Schedule) {
    let profile = Profile::parse(FAST_CHAOS).unwrap();
    let mut chain = chain_with(profile, |config| {
        config.seed = seed;
        config.horizon_ms = 20_000;
    });
    let schedule = chain.rig.schedule().clone();
    let payload = vec![9u8; 2048];
    let mut session = open(&chain);
    let notes = chain.rig.play();
    let mut seen = Vec::new();
    for note in notes.iter() {
        match note.phase {
            Phase::Applied => {
                let fault = note.event.fault;
                session.set_read_timeout(Duration::from_millis(1000));
                let mut outcomes = vec![match fault {
                    Fault::AddressChange => session.wait_for_end(Duration::from_millis(1000)),
                    _ => session.echo(&payload),
                }];
                if matches!(fault, Fault::Reset { .. }) {
                    let dialled = Session::open(&dial(&chain), Duration::from_millis(1000))
                        .map(|mut fresh| fresh.echo(&payload));
                    outcomes.push(dialled.unwrap_or_else(|outcome| outcome));
                }
                seen.push((note.event.index, fault.kind(), outcomes));
            }
            Phase::Reverted => session = open(&chain),
            Phase::Failed(why) => panic!("the rig failed event {}: {why}", note.event.index),
        }
    }
    (seen, schedule)
}

#[test]
#[ignore = "TL-7 rig: fetches and runs the pinned Toxiproxy and mitmproxy; see the module documentation"]
fn the_same_seed_replays_the_same_failures() {
    // Seed 7 draws every kind of fault from the fast menu within 20 s.
    let seed = 7;
    let (first, schedule) = failures_for(seed);
    eprintln!("failures for seed {seed}: {first:?}");
    let (again, replayed) = failures_for(seed);
    assert_eq!(schedule, replayed);
    assert_eq!(first.len(), schedule.events.len());
    for kind in ["reset", "blackhole", "limit_data", "address_change"] {
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
    assert_eq!(first, again, "the same seed must give the same failures");
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
        Scenario {
            id: "echo",
            profiles: &["N0", "N1", "N2", "N3"],
            run: |context| {
                let dial = Dial::through(&context.client).map_err(|e| e.to_string())?;
                let mut session = Session::open(&dial, IO).map_err(|e| format!("{e:?}"))?;
                match session.echo(b"cell") {
                    Outcome::Ok => Ok(()),
                    other => Err(format!("{other:?}")),
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
