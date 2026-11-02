//! The guest network over the real daemon (DC-13; `docs/acquisition-contract.md`,
//! "Guest network").
//!
//! The fixture's `bin` comes first on the daemon's `PATH`, so a `softnet`
//! placed there is the one the daemon checks, as Tart would. A symlink to
//! `/usr/bin/sudo` stands in for a setuid-root Softnet; a plain file stands in
//! for one that Tart cannot run unattended.
//!
//! The guest firewall's anchor file is read from the fixture through
//! `MACHINE_SERVICE_GUEST_FIREWALL_ANCHOR`, and the fake `ssh` answers the
//! isolation probe with `FAKE_PROBE_RESULT` (DC-14).

mod common;

use common::{Daemon, LegacyFixture, Response};
use serde_json::{json, Value};

struct Live {
    fixture: LegacyFixture,
    _daemon: Daemon,
}

enum Softnet {
    SetuidRoot,
    NotSetuid,
}

/// The guest firewall as the fixture presents it.
enum Firewall {
    /// Installed, and the guest's probe of the gateway times out.
    InForce,
    /// No anchor file.
    Missing,
    /// Installed, but the guest's probe of the gateway connects.
    NotInForce,
}

impl Live {
    fn new(softnet: Softnet) -> Live {
        Self::with_firewall(softnet, Firewall::InForce)
    }

    fn with_firewall(softnet: Softnet, firewall: Firewall) -> Live {
        Self::with_env(softnet, firewall, &[])
    }

    fn with_env(softnet: Softnet, firewall: Firewall, extra: &[(&str, &str)]) -> Live {
        let fixture = LegacyFixture::new();
        let anchor = fixture.root.join("guest-firewall.anchor");
        let table = fixture.root.join("internet-guests");
        std::fs::write(&table, "").expect("table file");
        if !matches!(firewall, Firewall::Missing) {
            std::fs::write(
                &anchor,
                machine_service_core::guest_firewall::anchor_rules(&table, Some(6241)),
            )
            .expect("anchor file");
        }
        let probe = match firewall {
            Firewall::NotInForce => "open",
            _ => "timeout",
        };
        let path = fixture.bin.join("softnet");
        match softnet {
            Softnet::SetuidRoot => {
                std::os::unix::fs::symlink("/usr/bin/sudo", &path).expect("softnet symlink")
            }
            Softnet::NotSetuid => std::fs::write(&path, "#!/bin/sh\n").expect("softnet file"),
        }
        let mut env = vec![
            (
                "MACHINE_SERVICE_GUEST_FIREWALL_ANCHOR".to_string(),
                anchor.display().to_string(),
            ),
            (
                "MACHINE_SERVICE_GUEST_FIREWALL_TABLE".to_string(),
                table.display().to_string(),
            ),
            ("FAKE_PROBE_RESULT".to_string(), probe.to_string()),
        ];
        env.extend(
            extra
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string())),
        );
        let daemon = fixture.start_daemon_with(&env);
        Live {
            fixture,
            _daemon: daemon,
        }
    }

    fn request(&self, method: &str, path: &str, body: Option<&Value>) -> Response {
        self.fixture.request(method, path, body)
    }

    fn acquire(&self, purpose: &str, network: Option<&str>) -> Response {
        let mut body = json!({"purpose": purpose, "image": "ubuntu2404", "env": "none"});
        if let Some(network) = network {
            body["network"] = json!(network);
        }
        self.request("POST", "/acquire", Some(&body))
    }

    /// The `tart run` line the fake Tart recorded for `vm`.
    fn run_line(&self, vm: &str) -> String {
        self.fixture
            .tart_calls()
            .lines()
            .find(|line| line.starts_with(&format!("run {vm}")))
            .unwrap_or_else(|| panic!("no tart run for {vm}: {}", self.fixture.tart_calls()))
            .to_string()
    }

    fn release(&self, vm: &str) {
        let response = self.request("POST", &format!("/machines/{vm}/release"), Some(&json!({})));
        assert_eq!(response.status, 200, "{}", response.body);
    }
}

/// The guest addresses in a fixture's table file.
fn table_addresses(live: &Live) -> Vec<String> {
    let text =
        std::fs::read_to_string(live.fixture.root.join("internet-guests")).unwrap_or_default();
    machine_service_core::guest_firewall::parse_table(&text)
        .into_iter()
        .filter_map(|line| match line {
            machine_service_core::guest_firewall::Line::Guest(guest) => {
                Some(guest.address.to_string())
            }
            machine_service_core::guest_firewall::Line::Other(_) => None,
        })
        .collect()
}

#[test]
fn internet_boots_on_softnet_and_is_reported() {
    let live = Live::new(Softnet::SetuidRoot);
    let response = live.acquire("web-only", Some("internet"));
    assert_eq!(response.status, 200, "{}", response.body);
    let vm = response.body["vm"].as_str().expect("vm").to_string();
    assert_eq!(response.body["network"], "internet");
    assert_eq!(
        response.body["configuration"]["requested"]["network"],
        "internet"
    );
    assert_eq!(
        response.body["configuration"]["effective"]["network"],
        "internet"
    );
    assert_eq!(
        live.run_line(&vm),
        format!("run {vm} --no-graphics --net-softnet")
    );

    assert_eq!(
        response.body["guest_dns"],
        json!(["1.1.1.1#cloudflare-dns.com", "8.8.8.8#dns.google"])
    );
    assert_eq!(response.body["isolation"]["status"], "verified");
    assert_eq!(response.body["isolation"]["gateway"], "192.168.2.1");
    assert_eq!(
        response.body["isolation"]["egress"]["address"],
        "104.20.23.154"
    );
    assert!(
        live.fixture.ssh_calls().contains("DNSOverTLS=yes"),
        "the guest resolvers were not set: {}",
        live.fixture.ssh_calls()
    );
    assert_eq!(table_addresses(&live), vec!["192.168.64.99".to_string()]);

    let one = live.request("GET", &format!("/machines/{vm}"), None);
    assert_eq!(one.body["network"], "internet");
    let all = live.request("GET", "/machines", None);
    assert_eq!(all.body["machines"][&vm]["network"], "internet");

    // The host still reaches the guest over the lease's SSH key.
    let exec = live.request(
        "POST",
        &format!("/machines/{vm}/exec"),
        Some(&json!({"argv": ["uname", "-a"]})),
    );
    assert_eq!(exec.status, 200, "{}", exec.body);
    live.release(&vm);
    assert!(
        table_addresses(&live).is_empty(),
        "release left the address in the table"
    );
}

#[test]
fn nat_and_absent_network_keep_today_s_command() {
    let live = Live::new(Softnet::SetuidRoot);
    for (purpose, network) in [("absent", None), ("explicit", Some("nat"))] {
        let response = live.acquire(purpose, network);
        assert_eq!(response.status, 200, "{}", response.body);
        let vm = response.body["vm"].as_str().expect("vm").to_string();
        assert_eq!(response.body["network"], "nat");
        assert_eq!(
            response.body["configuration"]["effective"]["network"],
            "nat"
        );
        assert_eq!(live.run_line(&vm), format!("run {vm} --no-graphics"));
        assert!(
            table_addresses(&live).is_empty(),
            "a NAT guest entered the table"
        );
        live.release(&vm);
    }
}

#[test]
fn internet_without_usable_softnet_fails_before_the_clone() {
    let live = Live::new(Softnet::NotSetuid);
    let response = live.acquire("web-only", Some("internet"));
    assert_eq!(response.status, 409, "{}", response.body);
    let error = response.body["error"].as_str().expect("error");
    assert!(error.starts_with("Softnet unavailable: "), "{error}");
    assert!(error.contains("no NAT fallback"), "{error}");
    assert!(
        !live.fixture.tart_calls().contains("clone"),
        "Tart cloned: {}",
        live.fixture.tart_calls()
    );
    assert!(live.fixture.vms_empty(), "a refused lease left a record");

    // NAT is unaffected by a missing Softnet.
    let response = live.acquire("nat-still", None);
    assert_eq!(response.status, 200, "{}", response.body);
    live.release(response.body["vm"].as_str().expect("vm"));
}

#[test]
fn other_networks_are_rejected_before_reservation() {
    let live = Live::new(Softnet::SetuidRoot);
    for network in ["bridged", "host", "softnet", ""] {
        let response = live.acquire("bad-net", Some(network));
        assert_eq!(response.status, 409, "{}", response.body);
        assert_eq!(response.body["error"], "network must be nat or internet");
    }
    // The control-only contract refuses as before.
    let response = live.request(
        "POST",
        "/acquire",
        Some(&json!({"purpose": "ctl", "image": "ubuntu2404", "env": "none", "network": "control-only"})),
    );
    assert_eq!(response.status, 409, "{}", response.body);
    assert!(response.body["error"]
        .as_str()
        .expect("error")
        .starts_with("control-only unavailable"));
    assert!(
        !live.fixture.tart_calls().contains("clone"),
        "Tart cloned: {}",
        live.fixture.tart_calls()
    );
}

#[test]
fn discovery_reports_whether_internet_is_available() {
    let available = Live::new(Softnet::SetuidRoot);
    let response = available.request("GET", "/acquisition-capabilities", None);
    assert_eq!(response.status, 200);
    let network = &response.body["options"]["network"];
    assert_eq!(network["default"], "nat");
    assert_eq!(network["values"], json!(["nat", "internet"]));
    assert_eq!(network["internet"]["available"], true);

    let unavailable = Live::new(Softnet::NotSetuid);
    let response = unavailable.request("GET", "/acquisition-capabilities", None);
    let internet = &response.body["options"]["network"]["internet"];
    assert_eq!(internet["available"], false);
    assert!(internet["reason"]
        .as_str()
        .expect("reason")
        .contains("is not a setuid-root file"));
    let no_firewall = Live::with_firewall(Softnet::SetuidRoot, Firewall::Missing);
    let response = no_firewall.request("GET", "/acquisition-capabilities", None);
    let internet = &response.body["options"]["network"]["internet"];
    assert_eq!(internet["available"], false);
    assert!(internet["reason"]
        .as_str()
        .expect("reason")
        .starts_with("guest firewall not installed"));
    assert!(
        unavailable.fixture.tart_calls().is_empty(),
        "discovery invoked Tart: {}",
        unavailable.fixture.tart_calls()
    );
}

#[test]
fn internet_without_the_guest_firewall_fails_before_the_clone() {
    let live = Live::with_firewall(Softnet::SetuidRoot, Firewall::Missing);
    let response = live.acquire("web-only", Some("internet"));
    assert_eq!(response.status, 409, "{}", response.body);
    let error = response.body["error"].as_str().expect("error");
    assert!(error.starts_with("guest firewall not installed"), "{error}");
    assert!(
        error.contains("vm-service-guest-firewall install"),
        "{error}"
    );
    assert!(
        !live.fixture.tart_calls().contains("clone"),
        "Tart cloned: {}",
        live.fixture.tart_calls()
    );
    // NAT needs no guest firewall.
    let response = live.acquire("nat-still", None);
    assert_eq!(response.status, 200, "{}", response.body);
    live.release(response.body["vm"].as_str().expect("vm"));
}

#[test]
fn a_guest_that_reaches_the_host_is_released() {
    let live = Live::with_firewall(Softnet::SetuidRoot, Firewall::NotInForce);
    let response = live.acquire("web-only", Some("internet"));
    assert_eq!(response.status, 409, "{}", response.body);
    let error = response.body["error"].as_str().expect("error");
    assert!(error.starts_with("guest isolation check failed"), "{error}");
    assert!(
        error.contains("192.168.64.99 connected to this host at 192.168.2.1:"),
        "{error}"
    );
    assert!(
        table_addresses(&live).is_empty(),
        "the failed lease left its address"
    );
    assert!(error.contains("com.apple/250.vm-service"), "{error}");
    let state = live.fixture.read_state();
    assert!(
        state.values().all(|record| record["state"] != "running"),
        "the lease was not released: {state:?}"
    );
    assert!(
        live.fixture.tart_calls().contains("delete pilot-web-only-"),
        "the clone was not deleted: {}",
        live.fixture.tart_calls()
    );
}

#[test]
fn a_guest_whose_names_resolve_to_private_addresses_is_released() {
    let live = Live::with_env(
        Softnet::SetuidRoot,
        Firewall::InForce,
        &[("FAKE_EGRESS_ADDRESS", "198.18.35.15")],
    );
    let response = live.acquire("web-only", Some("internet"));
    assert_eq!(response.status, 409, "{}", response.body);
    let error = response.body["error"].as_str().expect("error");
    assert!(error.starts_with("guest egress check failed"), "{error}");
    assert!(error.contains("198.18.35.15"), "{error}");
    assert!(
        table_addresses(&live).is_empty(),
        "the failed lease left its address"
    );
}
