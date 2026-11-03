//! MI-4/MI-5 and AC-52 to AC-54 at TL-2: real daemons and the real CLI.
//! DNS-SD is the test build's folder, which the daemon on loopback and the
//! CLI share, so nothing is advertised or browsed beyond this test and no
//! daemon binds outside loopback (DC-252); the Consul fixture
//! publishes installer output.

mod common;

use std::process::Output;
use std::time::{Duration, Instant};

use common::LegacyFixture;
use serde_json::{json, Value};

struct Instance {
    fixture: LegacyFixture,
    _daemon: common::Daemon,
    name: std::path::PathBuf,
}

impl Instance {
    fn start(name: &str, durable: bool) -> Self {
        let fixture = LegacyFixture::new();
        let name_file = fixture.state_dir.join("test-name");
        std::fs::write(&name_file, name).unwrap();
        std::fs::write(
            fixture.home.join(".zprofile"),
            "export PATH=/usr/bin:/bin:/usr/sbin:/sbin\n",
        )
        .unwrap();
        let daemon = fixture.start_daemon_with(&[
            ("MACHINE_SERVICE_HOST".into(), "127.0.0.1".into()),
            ("MACHINE_SERVICE_DISCOVERY".into(), "consul".into()),
            (
                "MACHINE_SERVICE_NAME_FILE".into(),
                name_file.display().to_string(),
            ),
            ("MACHINE_SERVICE_NAME_POLL_MS".into(), "50".into()),
            (
                "MACHINE_SERVICE_EXPOSE_DURABLE".into(),
                if durable { "on" } else { "off" }.into(),
            ),
            (
                "MACHINE_SERVICE_DURABLE_LOCK_DIR".into(),
                fixture.state_dir.join("lock").display().to_string(),
            ),
            (
                "MACHINE_SERVICE_CONFIG_DIR".into(),
                fixture.state_dir.join("config").display().to_string(),
            ),
        ]);
        Self {
            fixture,
            _daemon: daemon,
            name: name_file,
        }
    }

    fn health(&self) -> Value {
        self.fixture.request("GET", "/health", None).body
    }

    fn rename(&self, name: &str) {
        std::fs::write(&self.name, name).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.health()["instance"] != name {
            assert!(Instant::now() < deadline, "rename did not become visible");
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn bonjour(&self) -> discovery::dnssd::Scope {
        discovery::dnssd::Scope::Folder(self.fixture.root.join("bonjour"))
    }

    fn cli(&self) -> common::Isolated {
        let mut command = common::machctl_command();
        command
            .env(
                "MACHINE_SERVICE_TEST_BONJOUR",
                self.fixture.root.join("bonjour"),
            )
            .env("HOME", &self.fixture.home)
            .env(
                "MACHINE_SERVICE_STATE",
                self.fixture.state_dir.join("client"),
            )
            .env("MACHINE_SERVICE_PORT", common::free_port().to_string())
            .env(
                "CONSUL_HTTP_ADDR",
                format!("http://127.0.0.1:{}", common::free_port()),
            );
        command
    }

    fn advertise(&self, name: &str, machine_id: &str) -> discovery::dnssd::Advertisement {
        discovery::dnssd::register(
            name,
            discovery::SERVICE_TYPE,
            self.fixture.port(),
            &discovery::txt_entries(name, machine_id, &["durable", "virtual"]),
            self.bonjour(),
            Duration::from_secs(5),
        )
        .unwrap()
    }
}

fn listed(output: &Output, port: u16) -> Value {
    assert!(
        output.status.success() || output.status.code() == Some(2),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let list: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
    let matches: Vec<Value> = list
        .into_iter()
        .filter(|row| {
            row["address"]
                .as_str()
                .is_some_and(|address| address.ends_with(&format!(":{port}")))
        })
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "the test instance must be listed exactly once"
    );
    matches.into_iter().next().unwrap()
}

/// AC-52 / DC-90: stale advertisement labels cannot hide a real daemon
/// with the same hardware identity; a different identity stays unreachable.
#[test]
fn ac52_real_daemon_confirmed_by_machine_id() {
    let instance = Instance::start("Mac-B", false);
    let health = instance.health();
    let id = health["machine_id"].as_str().expect("hardware identity");
    let advertisement = instance.advertise("Mac-A", id);
    let output = instance.cli().args(["list", "--json"]).output().unwrap();
    let row = listed(&output, instance.fixture.port());
    assert_eq!(row["reachable"], true, "{row}");
    assert_eq!(row["instance"], "Mac-B");
    assert_eq!(row["machine_id"], id);
    drop(advertisement);
    let wrong = "00000000-0000-0000-0000-000000000000";
    let _advertisement = instance.advertise("Mac-A", wrong);
    let output = instance.cli().args(["list", "--json"]).output().unwrap();
    let row = listed(&output, instance.fixture.port());
    assert_eq!(row["reachable"], false, "{row}");
    let error = row["error"].as_str().unwrap();
    assert!(error.contains(id) && error.contains(wrong), "{error}");
}

/// AC-54 / MI-4: the exact installer description is unchanged while the
/// real daemon changes its label, and Consul confirmation still succeeds.
#[test]
fn ac54_consul_description_survives_real_daemon_rename() {
    use machine_service_install::catalog_description::{description, Offers};
    let instance = Instance::start("Mac-A", false);
    let health = instance.health();
    let id = health["machine_id"].as_str().expect("hardware identity");
    let description = serde_json::to_value(description(
        &Offers {
            durable: false,
            virtual_machines: true,
        },
        instance.fixture.port(),
        id,
    ))
    .unwrap();
    assert_eq!(description["meta"]["machine_id"], id);
    assert!(description["meta"].get("instance").is_none());
    let consul = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let address = consul.server_addr().to_ip().unwrap();
    let registration = json!([{
        "Node": {"Node": "catalog-node", "Address": "127.0.0.1"},
        "Service": {"ID": "fixture", "Service": "machine-service", "Address": "127.0.0.1",
                    "Port": description["port"], "Meta": description["meta"], "Tags": description["tags"]},
        "Checks": [{"ServiceID": "fixture", "Status": "passing"}]
    }]);
    let responder = std::thread::spawn(move || {
        // Exactly one lookup before and one after the rename; the registration
        // bytes stay fixed, so no reinstall or catalog refresh can mask a bug.
        for _ in 0..2 {
            let request = consul
                .recv_timeout(Duration::from_secs(20))
                .unwrap()
                .expect("Consul lookup");
            assert_eq!(request.url(), "/v1/health/service/machine-service");
            request
                .respond(tiny_http::Response::from_string(registration.to_string()))
                .unwrap();
        }
    });
    for name in ["Mac-A", "Mac-B"] {
        if name == "Mac-B" {
            instance.rename(name);
        }
        let output = instance
            .cli()
            .env("MACHINE_SERVICE_DISCOVERY", "consul")
            .env("CONSUL_HTTP_ADDR", format!("http://{address}"))
            .args(["list", "--json"])
            .output()
            .unwrap();
        let row = listed(&output, instance.fixture.port());
        assert_eq!(row["reachable"], true, "{row}");
        assert_eq!(row["instance"], name);
        assert_eq!(row["machine_id"], id);
    }
    responder.join().unwrap();
}

/// AC-53 / MI-5 / DC-90: a remembered durable lease still executes while
/// the replacement advertisement is delayed, then releases by granted name.
#[test]
fn ac53_remembered_lease_executes_across_advertisement_gap() {
    let old = "Mac-A-AC53";
    let final_name = "Mac-C-AC53";
    let instance = Instance::start(old, true);
    let health = instance.health();
    assert_eq!(health["instance"], old);
    let id = health["machine_id"].as_str().unwrap().to_string();
    let mut advertisement = Some(instance.advertise(old, &id));
    let acquired = instance
        .cli()
        .env("MACHINE_SERVICE_PORT", instance.fixture.port().to_string())
        .args([
            "acquire",
            "--machine",
            old,
            "--purpose",
            "identity-gap",
            "--env",
            "none",
        ])
        .output()
        .unwrap();
    assert!(
        acquired.status.success(),
        "{}",
        String::from_utf8_lossy(&acquired.stderr)
    );
    let lease: Value = serde_json::from_slice(&acquired.stdout).unwrap();
    assert_eq!(lease["granted_name"], old);
    let saved: Value = serde_json::from_slice(
        &std::fs::read(instance.fixture.state_dir.join("client/cli-leases.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(saved["leases"][0]["machine_id"], id);
    // The specified one-second gap may fit inside the initial two-second
    // browse. The longer gap forces that first browse to miss, proving the
    // second look against the real daemon and DNS-SD too (AC-53, MI-5).
    for (new, gap) in [("Mac-B-AC53", 1), (final_name, 3)] {
        drop(advertisement.take());
        instance.rename(new);
        let port = instance.fixture.port();
        let machine_id = id.clone();
        let scope = instance.bonjour();
        let ready = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(gap));
            discovery::dnssd::register(
                new,
                discovery::SERVICE_TYPE,
                port,
                &discovery::txt_entries(new, &machine_id, &["durable"]),
                scope,
                Duration::from_secs(5),
            )
            .unwrap()
        });
        let started = Instant::now();
        let output = instance
            .cli()
            .args(["exec", old, "--", "/usr/bin/true"])
            .output()
            .unwrap();
        advertisement = Some(ready.join().unwrap());
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        if gap == 3 {
            assert!(
                started.elapsed() >= Duration::from_secs(4),
                "the second browse must follow its two-second delay"
            );
        }
        let note = String::from_utf8_lossy(&output.stderr);
        assert!(note.contains(old) && note.contains(new), "{note}");
    }
    let released = instance.cli().args(["release", old]).output().unwrap();
    assert!(
        released.status.success(),
        "{}",
        String::from_utf8_lossy(&released.stderr)
    );
    let after = instance.fixture.request(
        "POST",
        &format!("/machines/{old}/heartbeat"),
        Some(&json!({"lease_id": lease["lease_id"]})),
    );
    assert_eq!(after.status, 404);
    assert!(after.body["error"].as_str().unwrap().contains(final_name));
}
