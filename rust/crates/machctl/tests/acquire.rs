//! `machctl acquire` by machine and by capability against fake instances
//! and a fake Consul agent (TL-2; `docs/ux/cli/list.md` CL-2; DC-69).
//!
//! Every server is an in-process HTTP fake on loopback, `CONSUL_HTTP_ADDR`
//! points at a fake, and `MACHINE_SERVICE_PORT` at the fake local instance,
//! so no test reaches a real instance or the real Consul agent.

use std::collections::HashMap;
use std::process::Output;
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{json, Value};

const MACHCTL: &str = env!("CARGO_BIN_EXE_machctl");

/// One request a fake saw: method, URL and JSON body.
type Seen = (String, String, Value);

/// A fake instance with a fixed status and JSON answer per path.
struct Fake {
    port: u16,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Fake {
    fn new(routes: Vec<(&str, u16, Value)>) -> Fake {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let routes: HashMap<String, (u16, String)> = routes
            .into_iter()
            .map(|(path, status, body)| (path.to_string(), (status, body.to_string())))
            .collect();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&seen);
        thread::spawn(move || {
            for mut request in server.incoming_requests() {
                let mut text = String::new();
                let _ = request.as_reader().read_to_string(&mut text);
                let body = serde_json::from_str(&text).unwrap_or(Value::Null);
                record.lock().unwrap().push((
                    request.method().to_string(),
                    request.url().to_string(),
                    body,
                ));
                let (status, body) = routes
                    .get(request.url())
                    .cloned()
                    .unwrap_or((404, "{\"error\": \"unknown VM\"}".to_string()));
                let _ = request
                    .respond(tiny_http::Response::from_string(body).with_status_code(status));
            }
        });
        Fake { port, seen }
    }

    fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// The bodies of every `POST` to `path` this fake received.
    fn posts(&self, path: &str) -> Vec<Value> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(method, url, _)| method == "POST" && url == path)
            .map(|(_, _, body)| body.clone())
            .collect()
    }

    /// The bodies of every `POST /acquire` this fake received.
    fn acquisitions(&self) -> Vec<Value> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(method, url, _)| method == "POST" && url == "/acquire")
            .map(|(_, _, body)| body.clone())
            .collect()
    }
}

fn registration(node: &str, port: u16) -> Value {
    json!({
        "Node": {"Node": node, "Address": "127.0.0.1"},
        "Service": {"ID": "catalog-machine-service", "Service": "machine-service",
                    "Tags": ["machines"], "Address": "127.0.0.1", "Port": port,
                    "Meta": {"loopback_only": "false"}},
        "Checks": [{"ServiceID": "catalog-machine-service", "Status": "passing"}]
    })
}

/// The local instance `mac-a` offering `macos26`, and `mac-b` whose
/// durable machine has Metal 3 and Xcode but is leased by someone else.
struct Intranet {
    local: Fake,
    remote: Fake,
    consul: Fake,
    home: tempfile::TempDir,
}

impl Intranet {
    fn new() -> Intranet {
        let local = Fake::new(vec![
            ("/health", 200, json!({"ok": true, "instance": "mac-a"})),
            (
                "/machines",
                200,
                json!({"machines": {}, "offers": [{
                    "name": "macos26", "image": "macos26", "kind": "virtual", "state": "offer",
                    "concurrency": {"running": 0, "limit": 2, "acquirable": true},
                    "capabilities": {"gpu": {"metal3": false},
                                     "software": {"xcode": [{"version": "26.6", "path": null}]}}
                }]}),
            ),
            (
                "/acquire",
                200,
                json!({"vm": "pilot-mac-t-1a2b", "lease_id": "lease-vm", "state": "running"}),
            ),
            (
                "/machines/pilot-mac-t-1a2b/exec",
                200,
                json!({"output": "hi\n", "rc": 0}),
            ),
            ("/machines/pilot-mac-t-1a2b/release", 200, json!({})),
        ]);
        let remote = Fake::new(vec![
            (
                "/health",
                200,
                json!({"ok": true, "service": "machine-service", "instance": "mac-b"}),
            ),
            (
                "/machines",
                200,
                json!({"machines": {"mac-b": {
                    "kind": "durable", "state": "available",
                    "capabilities": {"gpu": {"metal3": true},
                                     "software": {"xcode": [{"version": "26.6", "path": "/Applications/Xcode.app"}]},
                                     "ci_runner": {"present": false, "busy": null}}
                }}}),
            ),
            (
                "/acquire",
                409,
                json!({"error": "durable machine leased: mac-b (purpose other, until 2026-10-21 12:00:00)"}),
            ),
        ]);
        let consul = Fake::new(vec![
            (
                "/v1/health/service/machine-service",
                200,
                json!([
                    registration("mac-a", local.port),
                    registration("mac-b", remote.port)
                ]),
            ),
            (
                "/v1/agent/self",
                200,
                json!({"Config": {"NodeName": "mac-a"}}),
            ),
        ]);
        Intranet {
            local,
            remote,
            consul,
            home: tempfile::tempdir().unwrap(),
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        service_layout::test_build::harness::command(MACHCTL)
            .args(args)
            .env("HOME", self.home.path())
            .env("MACHINE_SERVICE_PORT", self.local.port.to_string())
            .env("MACHINE_SERVICE_STATE", self.home.path().join("state"))
            .env("CONSUL_HTTP_ADDR", self.consul.base())
            // These fixtures stand in for a Consul agent (DC-84).
            .env("MACHINE_SERVICE_DISCOVERY", "consul")
            .output()
            .unwrap()
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// CL-2: the durable machine is preferred and named with why; refused for
/// capacity, the next candidate, an offer on another instance, is leased.
#[test]
fn cl2_acquire_by_capability_falls_through_capacity_refusals() {
    let intranet = Intranet::new();
    let output = intranet.run(&["acquire", "--purpose", "t", "--require", "xcode"]);
    let errors = stderr(&output);
    assert_eq!(output.status.code(), Some(0), "{errors}");
    let chose_durable = errors
        .find("chose mac-b (durable, xcode, CI idle)")
        .expect("durable chosen first");
    let chose_offer = errors
        .find("chose macos26 (virtual, xcode, offer on mac-a)")
        .expect("then the offer");
    assert!(chose_durable < chose_offer, "{errors}");
    assert!(errors.contains("durable machine leased"), "{errors}");
    let lease: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(lease["lease_id"], "lease-vm");

    let durable = intranet.remote.acquisitions();
    assert_eq!(durable.len(), 1);
    assert_eq!(durable[0]["machine"], "mac-b");
    assert_eq!(durable[0]["purpose"], "t");
    for field in ["image", "network", "cpu"] {
        assert!(durable[0].get(field).is_none(), "{field}: {}", durable[0]);
    }
    let offer = intranet.local.acquisitions();
    assert_eq!(offer.len(), 1);
    assert_eq!(offer[0]["image"], "macos26");
    assert!(offer[0].get("machine").is_none());
}

/// CL-2: when every candidate is refused for capacity the command fails
/// naming each refusal; a requirement nothing meets says so.
#[test]
fn cl2_no_candidate_left() {
    let intranet = Intranet::new();
    let output = intranet.run(&["acquire", "--purpose", "t", "--require", "metal3"]);
    assert_eq!(output.status.code(), Some(1));
    let errors = stderr(&output);
    assert!(
        errors.contains("chose mac-b (durable, metal3, CI idle)"),
        "{errors}"
    );
    assert!(
        errors.contains("every machine with metal3 refused for capacity"),
        "{errors}"
    );
    assert!(intranet.local.acquisitions().is_empty());

    let output = intranet.run(&["acquire", "--purpose", "t", "--require", "unreal"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("no machine is available with unreal"),
        "{}",
        stderr(&output)
    );
}

/// CL-2: `--machine` goes to the instance that lists the machine and asks
/// for it by name, with no image.
#[test]
fn cl2_acquire_a_named_durable_machine() {
    let intranet = Intranet::new();
    let output = intranet.run(&["acquire", "--purpose", "t", "--machine", "mac-b"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("durable machine leased: mac-b"),
        "{}",
        stderr(&output)
    );
    let asked = intranet.remote.acquisitions();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0]["machine"], "mac-b");
    assert!(asked[0].get("image").is_none());
    assert!(intranet.local.acquisitions().is_empty());
}

/// CL-2: `--instance` takes an image acquisition to that instance; the
/// forms exclude each other with a usage error.
#[test]
fn cl2_image_on_an_instance_and_usage_errors() {
    let intranet = Intranet::new();
    let output = intranet.run(&[
        "acquire",
        "--purpose",
        "t",
        "--image",
        "macos26",
        "--instance",
        "mac-b",
    ]);
    // The fake answers every acquisition of mac-b with its refusal.
    assert_eq!(output.status.code(), Some(1));
    let asked = intranet.remote.acquisitions();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0]["image"], "macos26");

    for args in [
        &[
            "acquire",
            "--purpose",
            "t",
            "--require",
            "metal3",
            "--image",
            "macos26",
        ][..],
        &[
            "acquire",
            "--purpose",
            "t",
            "--machine",
            "mac-b",
            "--cpu",
            "4",
        ][..],
        &["acquire", "--purpose", "t", "--require", "nope"][..],
    ] {
        let output = intranet.run(args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?}: {}",
            stderr(&output)
        );
    }
}

/// CL-3 (DC-72): an acquisition is remembered in `cli-leases.json`, so the
/// next commands send its `lease_id` without one typed; `--lease-id`
/// overrides it, and a release forgets it.
#[test]
fn cl3_remembered_lease_ids() {
    let intranet = Intranet::new();
    let output = intranet.run(&["acquire", "--purpose", "t", "--require", "virtual"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let file = intranet.home.path().join("state/cli-leases.json");
    let text = std::fs::read_to_string(&file).expect("cli-leases.json");
    assert!(text.contains("\"lease-vm\""), "{text}");
    assert!(text.contains("pilot-mac-t-1a2b"), "{text}");
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);

    let output = intranet.run(&["exec", "pilot-mac-t-1a2b", "--", "echo", "hi"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let output = intranet.run(&[
        "exec",
        "pilot-mac-t-1a2b",
        "--lease-id",
        "typed",
        "--",
        "echo",
        "hi",
    ]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let execs = intranet.local.posts("/machines/pilot-mac-t-1a2b/exec");
    assert_eq!(execs[0]["lease_id"], "lease-vm");
    assert_eq!(execs[1]["lease_id"], "typed");

    let output = intranet.run(&["release", "pilot-mac-t-1a2b"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let releases = intranet.local.posts("/machines/pilot-mac-t-1a2b/release");
    assert_eq!(releases[0]["lease_id"], "lease-vm");
    let text = std::fs::read_to_string(&file).expect("cli-leases.json");
    assert!(!text.contains("lease-vm"), "{text}");
}

/// MI-5/DC-90: virtual acquisition replies predating machine_id still save
/// the owner identity from /health, so the remembered key survives renames.
#[test]
fn mi5_virtual_lease_saves_machine_identity_from_health() {
    let intranet = Intranet::new();
    let local = Fake::new(vec![
        (
            "/health",
            200,
            json!({"instance":"Mac-A","machine_id":"machine-a"}),
        ),
        // An image acquisition asks an instance that offers the image (FD-7).
        (
            "/machines",
            200,
            json!({"machines": {}, "offers": [{"name": "macos26", "kind": "virtual",
                "state": "offer", "concurrency": {"acquirable": true}}]}),
        ),
        (
            "/acquire",
            200,
            json!({"vm":"pilot-test","lease_id":"lease-a","state":"running"}),
        ),
    ]);
    let intranet = Intranet { local, ..intranet };
    let output = intranet.run(&["acquire", "--purpose", "t", "--image", "macos26"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let saved: Value = serde_json::from_str(
        &std::fs::read_to_string(intranet.home.path().join("state/cli-leases.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(saved["leases"][0]["machine_id"], "machine-a");
}

/// MI-4/MI-5: --instance resolves the current health name even when a
/// permanent Consul description contains no name at all.
#[test]
fn mi5_image_acquisition_selects_current_name_from_permanent_registration() {
    let intranet = Intranet::new();
    let remote = Fake::new(vec![
        (
            "/health",
            200,
            json!({"instance":"Mac-B","machine_id":"machine-b"}),
        ),
        ("/machines", 200, json!({"machines":{}})),
        (
            "/acquire",
            200,
            json!({"vm":"pilot-test","lease_id":"lease-b","state":"running","machine_id":"machine-b"}),
        ),
    ]);
    let mut row = registration("catalog-node", remote.port);
    row["Service"]["Meta"]["machine_id"] = json!("machine-b");
    let consul = Fake::new(vec![(
        "/v1/health/service/machine-service",
        200,
        json!([row]),
    )]);
    let intranet = Intranet {
        remote,
        consul,
        ..intranet
    };
    let output = intranet.run(&[
        "acquire",
        "--purpose",
        "t",
        "--image",
        "macos26",
        "--instance",
        "Mac-B",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(intranet.remote.acquisitions().len(), 1);
}
