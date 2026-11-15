//! BK-1 at a restart: the broker accepts its first connection only once the
//! credentials of the `relay` leases that survived are admitted again, so a
//! client or relay agent that dials the moment the port takes connections
//! is never refused `unauthorized` with a credential that is good.

mod common;

use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

use machine_service_core::config::{unix_now, GuestLink};
use machine_service_core::relay::{Relay, RelaySettings};
use machine_service_core::relay_broker::{credential_sha256, new_credential};
use serde_json::json;

/// How long the test holds the restart between binding the listener and
/// admitting the surviving credentials, standing for the time a restart
/// takes to read its leases' records.
const ADMISSION: Duration = Duration::from_millis(500);

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .map(|addr| addr.port())
        .expect("a free loopback port")
}

#[test]
fn a_hello_at_the_first_connection_after_a_restart_is_never_refused_a_surviving_credential() {
    let fixture = common::Fixture::with_guest_link(GuestLink::Relay);
    let (agent, client) = (new_credential(), new_credential());
    fixture.write_state(json!({
        "vm-a": {
            "link": "relay",
            "relay_agent_sha256": credential_sha256(&agent),
            "relay_client_sha256": credential_sha256(&client),
            "ttl_expires_at": unix_now() + 3600.0,
        }
    }));
    let port = free_port();
    let url = format!("wss://127.0.0.1:{port}/relay");
    let settings = RelaySettings {
        session_port: port,
        session_host: "127.0.0.1".into(),
        relay_url: url.clone(),
        guest_url: None,
        guest_port: None,
        durable_agent_port: None,
        register_timeout: Duration::from_secs(5),
        agent_wait: Duration::from_secs(5),
    };
    let log_file = fixture.service.config.log_file.clone();
    let log = Arc::new(move |message: &str| {
        machine_service_core::config::log_to_file(&log_file, message)
    });
    let relay = Relay::start(settings, &fixture.service.config.state_dir, log).expect("relay");
    let tls = remote_session::tls::client_config(&[relay.authority.cert_pem().as_bytes()])
        .expect("trust the broker's authority");

    // Both sides dial as soon as the port takes connections, before the
    // restart has admitted anything.
    let hellos: Vec<_> = [("client", client), ("agent", agent)]
        .into_iter()
        .map(|(role, credential)| {
            let mut config = remote_session::ClientConfig::new(&url, &credential);
            config.attributes = json!({"role": role});
            config.tls = Some(Arc::clone(&tls));
            std::thread::spawn(move || (role, remote_session::connect(config)))
        })
        .collect();
    std::thread::sleep(ADMISSION);
    fixture
        .service
        .install_relay(Arc::clone(&relay))
        .expect("install the relay link");
    relay.open().expect("open the broker");

    for hello in hellos {
        let (role, verdict) = hello.join().expect("the hello's thread");
        assert!(
            verdict.is_ok(),
            "the {role}'s first hello after the restart: {:?}",
            verdict.err()
        );
    }
    relay.stop();
}
