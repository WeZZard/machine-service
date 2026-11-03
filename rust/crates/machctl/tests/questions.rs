//! The real `machctl`'s catalog questions and image acquisition across
//! instances (TL-2; `docs/arch/federation.md` FD-7; `docs/ux/cli/list.md`
//! CL-2, CL-4).
//!
//! The fixture is a client Mac whose local instance offers nothing, two
//! instances that host virtual machines, and one that is stopped, all
//! listed by a fake Consul agent on loopback. The CLI is an isolated test
//! child whose DNS-SD is an empty folder of the test's own (DC-255).

use std::collections::HashMap;
use std::net::TcpListener;
use std::process::Output;
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{json, Value};

const MACHCTL: &str = env!("CARGO_BIN_EXE_machctl");

/// A fake instance with fixed answers per path, recording what it was asked.
struct Fake {
    port: u16,
    seen: Arc<Mutex<Vec<(String, String)>>>,
}

impl Fake {
    fn new(routes: Vec<(&str, &str, u16, Value)>) -> Fake {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let routes: HashMap<(String, String), (u16, String)> = routes
            .into_iter()
            .map(|(method, path, status, body)| {
                (
                    (method.to_string(), path.to_string()),
                    (status, body.to_string()),
                )
            })
            .collect();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&seen);
        thread::spawn(move || {
            for request in server.incoming_requests() {
                let key = (request.method().to_string(), request.url().to_string());
                record.lock().unwrap().push(key.clone());
                let (status, body) = routes
                    .get(&key)
                    .cloned()
                    .unwrap_or((404, "{\"error\": \"not found\"}".to_string()));
                let _ = request
                    .respond(tiny_http::Response::from_string(body).with_status_code(status));
            }
        });
        Fake { port, seen }
    }

    fn asked(&self, method: &str, path: &str) -> usize {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, p)| m == method && p == path)
            .count()
    }
}

fn closed_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

fn registration(name: &str, machine_id: &str, port: u16) -> Value {
    json!({
        "Node": {"Node": format!("node-{name}"), "Address": "127.0.0.1"},
        "Service": {"ID": "catalog-machine-service", "Service": "machine-service",
                    "Address": "127.0.0.1", "Port": port,
                    "Meta": {"instance": name, "machine_id": machine_id, "loopback_only": "false"}},
        "Checks": [{"ServiceID": "catalog-machine-service", "Status": "passing"}]
    })
}

fn images(generation: u64, acquirable: bool) -> Value {
    json!({"images": {"macos26": {
        "kind": "macos", "base_vm": "pilot-macos26-base", "base_available": true,
        "concurrency": {"running": 0, "limit": 2, "acquirable": acquirable}}},
        "catalog": {"status": "current", "generation": generation}})
}

fn offer(acquirable: bool) -> Value {
    json!({"machines": {}, "offers": [{"name": "macos26", "image": "macos26", "kind": "virtual",
        "state": "offer", "base_available": true,
        "concurrency": {"running": 0, "limit": 2, "acquirable": acquirable}}]})
}

/// A VM host named `name` with `macos26`, answering `/acquire` with
/// `acquire`.
fn host(name: &str, generation: u64, acquirable: bool, acquire: (u16, Value)) -> Fake {
    Fake::new(vec![
        (
            "GET",
            "/health",
            200,
            json!({"ok": true, "service": "machine-service",
                                        "instance": name, "machine_id": format!("id-{name}")}),
        ),
        ("GET", "/machines", 200, offer(acquirable)),
        ("GET", "/images", 200, images(generation, acquirable)),
        ("GET", "/capabilities", 200, json!({"networks": [name]})),
        (
            "GET",
            "/acquisition-capabilities",
            200,
            json!({"schemaVersion": 1, "from": name}),
        ),
        ("POST", "/acquire", acquire.0, acquire.1),
    ])
}

/// The client Mac: its own instance offers nothing (DC-93); `mac-a` and
/// `mac-b` host virtual machines; `mac-c` is stopped.
struct Lab {
    laptop: Fake,
    mac_a: Fake,
    mac_b: Fake,
    consul: Fake,
    home: tempfile::TempDir,
}

impl Lab {
    fn new(a_acquire: (u16, Value), a_acquirable: bool) -> Lab {
        let laptop = Fake::new(vec![
            (
                "GET",
                "/health",
                200,
                json!({"ok": true, "service": "machine-service",
                                            "instance": "laptop", "machine_id": "id-laptop"}),
            ),
            ("GET", "/machines", 200, json!({"machines": {}})),
            (
                "GET",
                "/images",
                200,
                json!({"images": {}, "catalog": {"status": "current", "generation": 1}}),
            ),
            ("GET", "/capabilities", 200, json!({"networks": []})),
            (
                "GET",
                "/acquisition-capabilities",
                200,
                json!({"schemaVersion": 1, "from": "laptop"}),
            ),
        ]);
        let mac_a = host("mac-a", 7, a_acquirable, a_acquire);
        let mac_b = host(
            "mac-b",
            6,
            true,
            (
                200,
                json!({"vm": "pilot-mac-t-b", "lease_id": "lease-b", "state": "running",
                         "machine_id": "id-mac-b"}),
            ),
        );
        let consul = Fake::new(vec![(
            "GET",
            "/v1/health/service/machine-service",
            200,
            json!([
                registration("laptop", "id-laptop", laptop.port),
                registration("mac-a", "id-mac-a", mac_a.port),
                registration("mac-b", "id-mac-b", mac_b.port),
                registration("mac-c", "id-mac-c", closed_port()),
            ]),
        )]);
        Lab {
            laptop,
            mac_a,
            mac_b,
            consul,
            home: tempfile::tempdir().unwrap(),
        }
    }

    fn granting() -> Lab {
        Lab::new(
            (
                200,
                json!({"vm": "pilot-mac-t-a", "lease_id": "lease-a", "state": "running",
                         "machine_id": "id-mac-a"}),
            ),
            true,
        )
    }

    fn run(&self, args: &[&str]) -> Output {
        let bonjour = tempfile::tempdir().unwrap();
        service_layout::test_build::harness::command(MACHCTL)
            .args(args)
            .env("HOME", self.home.path())
            .env("MACHINE_SERVICE_PORT", self.laptop.port.to_string())
            .env("MACHINE_SERVICE_STATE", self.home.path().join("state"))
            .env("MACHINE_SERVICE_TEST_BONJOUR", bonjour.path())
            .env(
                "CONSUL_HTTP_ADDR",
                format!("http://127.0.0.1:{}", self.consul.port),
            )
            // The fixture stands in for a Consul agent (DC-84).
            .env("MACHINE_SERVICE_DISCOVERY", "consul")
            .output()
            .unwrap()
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn json_of(output: &Output) -> Vec<Value> {
    serde_json::from_str(&stdout(output))
        .unwrap_or_else(|error| panic!("{error}: {}\n{}", stdout(output), stderr(output)))
}

fn by_name<'a>(listed: &'a [Value], name: &str) -> &'a Value {
    listed
        .iter()
        .find(|entry| entry["instance"] == name)
        .unwrap_or_else(|| panic!("{name} not in {listed:?}"))
}

/// FD-7, CL-4: `images --json` reports every instance's answer unchanged
/// under its name and machine ID: the empty local catalog, each host's
/// catalog with its own generation, and the stopped instance as unreachable.
#[test]
fn fd7_images_ask_every_instance() {
    let lab = Lab::granting();
    let output = lab.run(&["images", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let listed = json_of(&output);
    assert_eq!(listed.len(), 4, "{listed:?}");
    assert_eq!(listed[0]["instance"], "laptop");
    assert_eq!(listed[0]["local"], true);
    assert_eq!(listed[0]["answer"]["images"], json!({}));
    let a = by_name(&listed, "mac-a");
    assert_eq!(a["machine_id"], "id-mac-a");
    assert_eq!(a["answer"]["catalog"]["generation"], 7);
    assert_eq!(
        by_name(&listed, "mac-b")["answer"]["catalog"]["generation"],
        6
    );
    let c = by_name(&listed, "mac-c");
    assert_eq!(c["reachable"], false);
    assert!(
        c["error"].as_str().unwrap().contains("connection refused"),
        "{c}"
    );

    let table = lab.run(&["images"]);
    let text = stdout(&table);
    assert!(
        text.lines()
            .any(|l| l.starts_with("laptop ") && l.ends_with("provides no images")),
        "{text}"
    );
    assert!(
        text.lines()
            .any(|l| l.starts_with("mac-a ") && l.contains("macos26")),
        "{text}"
    );
    assert!(
        text.lines()
            .any(|l| l.starts_with("mac-c ") && l.contains("unreachable")),
        "{text}"
    );
}

/// CL-4: `--instance` and `--local` narrow a question to one instance.
#[test]
fn cl4_instance_and_local_narrow_a_question() {
    let lab = Lab::granting();
    for (args, only) in [
        (&["capabilities", "--instance", "mac-b"][..], "mac-b"),
        (
            &["acquisition-capabilities", "--instance", "id-mac-a"][..],
            "mac-a",
        ),
        (&["capabilities", "--local"][..], "laptop"),
    ] {
        let output = lab.run(args);
        assert_eq!(
            output.status.code(),
            Some(0),
            "{args:?}: {}",
            stderr(&output)
        );
        let listed = json_of(&output);
        assert_eq!(listed.len(), 1, "{args:?}: {listed:?}");
        assert_eq!(listed[0]["instance"], only, "{args:?}");
    }
    let all = json_of(&lab.run(&["acquisition-capabilities"]));
    assert_eq!(
        by_name(&all, "mac-b")["answer"],
        json!({"schemaVersion": 1, "from": "mac-b"})
    );
    assert_eq!(by_name(&all, "mac-c")["reachable"], false);
}

/// CL-4: `images-show` names the instances whose catalog lists the image.
#[test]
fn cl4_images_show_lists_the_instances_with_the_image() {
    let lab = Lab::granting();
    let output = lab.run(&["images-show", "macos26"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let shown = json_of(&output);
    let names: Vec<&str> = shown
        .iter()
        .map(|e| e["instance"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["mac-a", "mac-b"]);
    assert_eq!(shown[0]["answer"]["base_vm"], "pilot-macos26-base");
    let missing = lab.run(&["images-show", "nope"]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(
        stderr(&missing).contains("laptop: none; mac-a: macos26"),
        "{}",
        stderr(&missing)
    );
}

/// FD-7, CL-2: an image acquisition without `--instance` goes to an
/// instance that offers the image, says why, and leaves the local instance
/// that offers nothing alone.
#[test]
fn fd7_image_acquisition_chooses_an_instance_that_offers_it() {
    let lab = Lab::granting();
    let output = lab.run(&["acquire", "--purpose", "t", "--image", "macos26"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("chose macos26 (virtual, offer on mac-a)"),
        "{}",
        stderr(&output)
    );
    assert_eq!(lab.mac_a.asked("POST", "/acquire"), 1);
    assert_eq!(lab.mac_b.asked("POST", "/acquire"), 0);
    assert_eq!(lab.laptop.asked("POST", "/acquire"), 0);
    let saved = std::fs::read_to_string(lab.home.path().join("state/cli-leases.json")).unwrap();
    assert!(saved.contains("id-mac-a"), "{saved}");
}

/// FD-7, CL-2: a capacity refusal passes to the next instance; an offer
/// that is not acquirable comes after one that is.
#[test]
fn fd7_image_acquisition_passes_capacity_refusals_on() {
    let lab = Lab::new(
        (409, json!({"error": "macOS VM limit reached (2/2)"})),
        true,
    );
    let output = lab.run(&["acquire", "--purpose", "t", "--image", "macos26"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let errors = stderr(&output);
    assert!(
        errors.contains("mac-a refused: macOS VM limit reached"),
        "{errors}"
    );
    assert!(
        errors.contains("chose macos26 (virtual, offer on mac-b)"),
        "{errors}"
    );
    assert_eq!(lab.mac_b.asked("POST", "/acquire"), 1);

    let later = Lab::new((200, json!({})), false);
    let output = later.run(&["acquire", "--purpose", "t", "--image", "macos26"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(later.mac_b.asked("POST", "/acquire"), 1);
    assert_eq!(later.mac_a.asked("POST", "/acquire"), 0);
}

/// CL-4: with no instance answering, a question exits 2 with the hint.
#[test]
fn cl4_no_instance_answering_exits_2() {
    let home = tempfile::tempdir().unwrap();
    let bonjour = tempfile::tempdir().unwrap();
    let output = service_layout::test_build::harness::command(MACHCTL)
        .args(["images"])
        .env("HOME", home.path())
        .env("MACHINE_SERVICE_PORT", closed_port().to_string())
        .env("MACHINE_SERVICE_TEST_BONJOUR", bonjour.path())
        .env("CONSUL_HTTP_ADDR", format!("127.0.0.1:{}", closed_port()))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("not running"),
        "{}",
        stdout(&output)
    );
    assert!(stderr(&output).contains("no machine-service instance answered"));
}
