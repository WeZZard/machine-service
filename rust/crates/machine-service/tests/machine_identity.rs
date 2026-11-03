//! MI-1 to MI-3 / DC-87 to DC-89 at TL-2: isolated real daemons, a file
//! name source, and the test build's DNS-SD folder, so every daemon binds
//! loopback and nothing is advertised beyond this test (DC-252). No
//! test changes the Mac's name.

mod common;

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    name: PathBuf,
}

struct Daemon {
    child: common::Owned,
    port: u16,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = common::canonical_dir(dir.path());
        let name = root.join("name");
        std::fs::write(&name, "Mac-A\n").unwrap();
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::write(
            root.join("home/.zprofile"),
            "export PATH=/usr/bin:/bin:/usr/sbin:/sbin\n",
        )
        .unwrap();
        Self {
            _dir: dir,
            root,
            name,
        }
    }

    fn start(&self, exposed: bool, advertised: bool) -> Daemon {
        let port = common::free_port();
        let mut command = common::daemon_command(&self.root);
        command
            .env("HOME", self.root.join("home"))
            .env("MACHINE_SERVICE_STATE", self.root.join("state"))
            .env("MACHINE_SERVICE_CONFIG_DIR", self.root.join("config"))
            .env("MACHINE_SERVICE_DURABLE_LOCK_DIR", self.root.join("lock"))
            .env("MACHINE_SERVICE_PORT", port.to_string())
            .env("MACHINE_SERVICE_HOST", "127.0.0.1")
            .env(
                "MACHINE_SERVICE_DISCOVERY",
                if advertised { "bonjour" } else { "consul" },
            )
            .env(
                "MACHINE_SERVICE_EXPOSE_DURABLE",
                if exposed { "on" } else { "off" },
            )
            .env("MACHINE_SERVICE_NAME_FILE", &self.name)
            .env("MACHINE_SERVICE_NAME_POLL_MS", "50")
            .env("PILOT_REPO", self.root.join("pilot"))
            .env("TART_HOME", self.root.join("tart"))
            .stdout(Stdio::null())
            .stderr(Stdio::from(
                std::fs::File::create(self.root.join("daemon.err")).unwrap(),
            ));
        let child = command.spawn().unwrap();
        let daemon = Daemon { child, port };
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if common::try_http(port, "GET", "/health", None, None).is_ok_and(|r| r.status == 200) {
                return daemon;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!(
            "daemon startup: {}",
            std::fs::read_to_string(self.root.join("daemon.err")).unwrap()
        );
    }
}

impl Daemon {
    fn request(&self, method: &str, path: &str, body: Option<&Value>) -> common::Response {
        common::http_timeout(
            self.port,
            method,
            path,
            body,
            None,
            Duration::from_secs(180),
        )
    }

    fn health(&self) -> Value {
        self.request("GET", "/health", None).body
    }
}

/// MI-1 / DC-87: hardware identity is available without durable exposure.
#[test]
fn mi1_virtual_only_instance_reports_hardware_identity() {
    let fixture = Fixture::new();
    let daemon = fixture.start(false, false);
    let health = daemon.health();
    let id = health["machine_id"]
        .as_str()
        .expect("machine_id must be a string");
    assert_eq!(id.len(), 36);
    assert_eq!(health["former_names"], json!([]));
}

fn advertised(root: &std::path::Path, port: u16) -> Vec<discovery::Found> {
    discovery::dnssd::browse(
        discovery::SERVICE_TYPE,
        discovery::dnssd::Scope::Folder(root.join("bonjour")),
        Duration::from_millis(150),
        Duration::from_millis(300),
    )
    .unwrap()
    .into_iter()
    .filter(|found| found.port == port)
    .collect()
}

/// AC-50 / MI-2 / DC-88: one real process withdraws its old advertisement
/// and publishes the same hardware identity under its new label.
#[test]
fn ac50_live_rename_replaces_advertisement_without_restart() {
    let fixture = Fixture::new();
    let mut daemon = fixture.start(false, true);
    let initial = daemon.health();
    assert_eq!(initial["instance"], "Mac-A");
    let pid = daemon.child.id();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if advertised(&fixture.root, daemon.port)
            .iter()
            .any(|found| found.instance() == "Mac-A")
        {
            break;
        }
        assert!(Instant::now() < deadline, "initial advertisement missing");
    }
    std::fs::write(&fixture.name, "Mac-B\n").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let found = advertised(&fixture.root, daemon.port);
        let health = daemon.health();
        if health["instance"] == "Mac-B"
            && found.iter().any(|f| {
                f.instance() == "Mac-B" && f.machine_id() == initial["machine_id"].as_str()
            })
            && !found.iter().any(|f| f.instance() == "Mac-A")
        {
            assert_eq!(health["machine_id"], initial["machine_id"]);
            assert_eq!(health["former_names"][0]["name"], "Mac-A");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "rename did not reach health and DNS-SD within 5s"
        );
    }
    assert_eq!(daemon.child.id(), pid);
    assert!(daemon.child.try_wait().unwrap().is_none());
}

fn wait_name(daemon: &Daemon, name: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while daemon.health()["instance"] != name {
        assert!(Instant::now() < deadline, "name did not update");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// AC-51 / MI-3 / DC-89: real exec and transfers keep the granted label,
/// including after a daemon restart, then old-name requests explain the rename.
#[test]
fn ac51_granted_name_survives_rename_and_restart() {
    let fixture = Fixture::new();
    let daemon = fixture.start(true, false);
    let acquired = daemon.request(
        "POST",
        "/acquire",
        Some(&json!({"machine": "Mac-A", "purpose": "identity-test", "env": "none"})),
    );
    assert_eq!(acquired.status, 200, "{}", acquired.body);
    let lease = acquired.body["lease_id"].as_str().unwrap().to_string();
    let record_path = fixture
        .root
        .join("state/leases")
        .join(&lease)
        .join("lease.json");
    let record: Value = serde_json::from_slice(&std::fs::read(&record_path).unwrap()).unwrap();
    assert_eq!(record["granted_name"], "Mac-A");
    assert_eq!(acquired.body["granted_name"], "Mac-A");
    assert_eq!(acquired.body["machine_id"], daemon.health()["machine_id"]);
    std::fs::write(&fixture.name, "Mac-B\n").unwrap();
    wait_name(&daemon, "Mac-B");
    let exec = |daemon: &Daemon| {
        let result = daemon.request(
            "POST",
            "/machines/Mac-A/exec",
            Some(&json!({"lease_id": lease, "argv": ["/usr/bin/printf", "identity-ok"]})),
        );
        assert_eq!(result.status, 200, "{}", result.body);
        assert_eq!(result.body["output"], "identity-ok");
    };
    exec(&daemon);
    let input = fixture.root.join("input");
    let output = fixture.root.join("output");
    std::fs::write(&input, b"identity-transfer").unwrap();
    for (action, body) in [
        (
            "push",
            json!({"lease_id": lease, "local_path": input, "remote_path": "transfer"}),
        ),
        (
            "pull",
            json!({"lease_id": lease, "local_path": output, "remote_path": "transfer"}),
        ),
        ("heartbeat", json!({"lease_id": lease, "ttl_hours": 1})),
        ("refresh", json!({"lease_id": lease})),
    ] {
        let result = daemon.request("POST", &format!("/machines/Mac-A/{action}"), Some(&body));
        assert_eq!(result.status, 200, "{action}: {}", result.body);
    }
    assert_eq!(std::fs::read(&output).unwrap(), b"identity-transfer");
    let refused = daemon.request(
        "POST",
        "/machines/Mac-A/exec",
        Some(&json!({"lease_id": "stale", "argv": ["true"]})),
    );
    assert_eq!(refused.status, 404);
    assert!(refused.body["error"]
        .as_str()
        .unwrap()
        .contains("Mac-A was renamed Mac-B on"));
    let acquire_old = daemon.request(
        "POST",
        "/acquire",
        Some(&json!({"machine": "Mac-A", "purpose": "other", "env": "none"})),
    );
    assert_eq!(acquire_old.status, 404);
    assert!(acquire_old.body["error"]
        .as_str()
        .unwrap()
        .contains("use Mac-B"));
    drop(daemon);
    // A second rename while stopped must be remembered on startup too.
    std::fs::write(&fixture.name, "Mac-C\n").unwrap();
    let daemon = fixture.start(true, false);
    exec(&daemon);
    assert_eq!(daemon.health()["instance"], "Mac-C");
    let machines = daemon.request("GET", "/machines", None).body;
    assert_eq!(machines["machines"]["Mac-C"]["name"], "Mac-C");
    assert_eq!(machines["machines"]["Mac-C"]["granted_name"], "Mac-A");
    assert_eq!(
        machines["machines"]["Mac-C"]["machine_id"],
        daemon.health()["machine_id"]
    );
    let released = daemon.request(
        "POST",
        "/machines/Mac-A/release",
        Some(&json!({"lease_id": lease})),
    );
    assert_eq!(released.status, 200, "{}", released.body);
    let old = daemon.request("GET", "/machines/Mac-A", None);
    assert_eq!(old.status, 404);
    assert!(old.body["error"]
        .as_str()
        .unwrap()
        .contains("Mac-A was renamed Mac-C on"));
    assert!(old.body["error"].as_str().unwrap().ends_with("use Mac-C"));
}

/// MI-3 / DC-89: virtual-only instances also explain their former label.
#[test]
fn mi3_unexposed_former_name_explains_the_rename() {
    let fixture = Fixture::new();
    let daemon = fixture.start(false, false);
    std::fs::write(&fixture.name, "Mac-B\n").unwrap();
    wait_name(&daemon, "Mac-B");
    for (method, path, body) in [
        ("GET", "/machines/Mac-A", None),
        (
            "POST",
            "/acquire",
            Some(json!({"machine": "Mac-A", "purpose": "identity-test"})),
        ),
    ] {
        let result = daemon.request(method, path, body.as_ref());
        assert_eq!(result.status, 404);
        assert!(
            result.body["error"]
                .as_str()
                .unwrap()
                .contains("Mac-A was renamed Mac-B on"),
            "{}",
            result.body
        );
    }
}

/// MI-2 / MI-3: a pre-MI lease retains its persisted original label and is
/// migrated once; an offline rename is logged with both labels.
#[test]
fn mi3_legacy_lease_migrates_granted_name_after_offline_rename() {
    let fixture = Fixture::new();
    let daemon = fixture.start(true, false);
    let acquired = daemon.request(
        "POST",
        "/acquire",
        Some(&json!({"machine": "Mac-A", "purpose": "legacy-identity", "env": "none"})),
    );
    assert_eq!(acquired.status, 200, "{}", acquired.body);
    let lease = acquired.body["lease_id"].as_str().unwrap();
    let record_path = fixture
        .root
        .join("state/leases")
        .join(lease)
        .join("lease.json");
    drop(daemon);
    let mut record: Value = serde_json::from_slice(&std::fs::read(&record_path).unwrap()).unwrap();
    record.as_object_mut().unwrap().remove("granted_name");
    std::fs::write(&record_path, serde_json::to_vec(&record).unwrap()).unwrap();
    std::fs::write(&fixture.name, "Mac-B\n").unwrap();
    let daemon = fixture.start(true, false);
    let record: Value = serde_json::from_slice(&std::fs::read(&record_path).unwrap()).unwrap();
    assert_eq!(record["granted_name"], "Mac-A");
    let released = daemon.request(
        "POST",
        "/machines/Mac-A/release",
        Some(&json!({"lease_id": lease})),
    );
    assert_eq!(released.status, 200, "{}", released.body);
    let log = std::fs::read_to_string(fixture.root.join("state/service.log")).unwrap();
    assert!(log.contains("machine name changed from Mac-A to Mac-B"));
}

/// MI-1 / DC-87: every virtual list record, offer, and individual machine
/// record identifies the host even when durable exposure is disabled.
#[test]
fn mi1_virtual_records_and_offers_carry_machine_id() {
    let fixture = common::LegacyFixture::new();
    let _daemon = fixture.start_daemon();
    let health = fixture.request("GET", "/health", None).body;
    let acquired = fixture.request(
        "POST",
        "/acquire",
        Some(&json!({"image": "ubuntu2404", "purpose": "identity-record", "env": "none"})),
    );
    assert_eq!(acquired.status, 200, "{}", acquired.body);
    let name = acquired.body["vm"].as_str().unwrap();
    let list = fixture.request("GET", "/machines", None).body;
    assert_eq!(list["machines"][name]["machine_id"], health["machine_id"]);
    assert_eq!(list["machines"][name]["instance"], health["instance"]);
    let offers = list["offers"].as_array().unwrap();
    assert!(!offers.is_empty());
    for offer in offers {
        assert_eq!(offer["machine_id"], health["machine_id"]);
    }
    let record = fixture
        .request("GET", &format!("/machines/{name}"), None)
        .body;
    assert_eq!(record["machine_id"], health["machine_id"]);
    assert_eq!(record["instance"], health["instance"]);
    let released = fixture.request(
        "POST",
        &format!("/machines/{name}/release"),
        Some(&json!({})),
    );
    assert_eq!(released.status, 200);
}
