//! Live: a client of a real broker over the network, as mcp-relay will be
//! (RP-1 to RP-7). Opt-in, `#[ignore]`d; it needs a lease on an instance
//! that serves the relay link:
//!
//! ```sh
//! RELAY_LIVE_URL=<relay.url> RELAY_LIVE_CREDENTIAL=<relay.credential> \
//! RELAY_LIVE_CA=<file holding relay.ca_pem> RELAY_LIVE_CA_SHA256=<relay.ca_sha256> \
//!   cargo test -p relay-broker --test live_client -- --ignored --nocapture
//! ```

mod common;

use std::time::{Duration, Instant};

use common::{client_config, exec, TestClient};
use relay_agent::protocol::{Op, Outcome};

fn var(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("set {name}"))
}

#[test]
#[ignore = "needs a live lease; see the module documentation"]
fn a_live_lease_answers_through_its_broker() {
    let url = var("RELAY_LIVE_URL");
    let pem = std::fs::read_to_string(var("RELAY_LIVE_CA")).unwrap();
    assert_eq!(
        relay_broker::tls::pem_sha256(&pem),
        Some(var("RELAY_LIVE_CA_SHA256")),
        "the authority matches its published fingerprint"
    );
    let mut config = client_config(&url, &var("RELAY_LIVE_CREDENTIAL"));
    config.tls = Some(remote_session::tls::client_config(&[pem.as_bytes()]).unwrap());
    config.timing = remote_session::Timing::default();
    let started = Instant::now();
    let client = TestClient::connect_with(config).unwrap();
    eprintln!(
        "connected to {url} in {:?}; lease {}, agent {:?}",
        started.elapsed(),
        client.ready.lease,
        client.ready.agent
    );
    let id = |name: &str| format!("live-{name}-{}", std::process::id());
    let wait = Duration::from_secs(120);

    let started = Instant::now();
    client.call(1, &id("exec"), exec("uname -sm; id -un"), b"");
    let answer = client.wait(1, wait);
    eprintln!(
        "exec in {:?}: {}",
        started.elapsed(),
        String::from_utf8_lossy(&answer.bytes).trim()
    );
    assert!(matches!(
        answer.result.unwrap().outcome,
        Outcome::Exit { rc: Some(0), .. }
    ));

    let bytes: Vec<u8> = (0..3 * 1024 * 1024u32)
        .map(|i| (i * 31 % 251) as u8)
        .collect();
    let started = Instant::now();
    client.call(
        2,
        &id("write"),
        Op::Write {
            path: ".relay-live-client.bin".into(),
            name: String::new(),
            mode: Some(0o600),
        },
        &bytes,
    );
    assert!(matches!(
        client.wait(2, wait).result.unwrap().outcome,
        Outcome::Written(_)
    ));
    client.call(
        3,
        &id("read"),
        Op::Read {
            path: ".relay-live-client.bin".into(),
        },
        b"",
    );
    let read = client.wait(3, wait);
    assert!(read.end_matches());
    assert_eq!(read.bytes, bytes);
    eprintln!("3 MiB written and read back in {:?}", started.elapsed());
    client.call(4, &id("clean"), exec("rm -f .relay-live-client.bin"), b"");
    client.wait(4, wait);
    // A repeated execution ID is answered from the record.
    client.call(5, &id("exec"), exec("never runs"), b"");
    assert!(client.wait(5, wait).result.unwrap().replayed);
}
