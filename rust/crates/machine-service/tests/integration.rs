//! Port of `tests/integration/test_integration.py`.
//!
//! The real `machine-service` daemon over real HTTP plus the real `machctl` client.
//! The Python suite stubbed `tart`, `_ssh`, and `_scp` *inside* the daemon
//! process with `mock.patch.object`; a Rust test cannot reach into another
//! process, so this port starts the real binary and supplies fake `tart`,
//! `ssh`, and `scp` executables through `PATH` (see `tests/common/mod.rs`).
//!
//! Forced deviations (each carries its own comment at the assertion):
//!   * `host_macos_guests` reads `/usr/bin/pgrep`, which cannot be shimmed
//!     through `PATH`; the snapshot shape test asserts the documented zero
//!     value and is therefore only valid on a host with no running
//!     Virtualization.framework guests (the Python fixture stubbed it to 0).
//!   * The Python fake `ssh` received the request timeout as an argument and
//!     echoed it. A real external `ssh` never sees the deadline, so the two
//!     assertion sites that compared against `str(timeout)` instead assert
//!     that the request reached `ssh` without clamping its observable shape.
//!     The exact deadline is pinned at the `Host` seam by
//!     `machine-service-core/tests/exec_timeout.rs`.
//!
//! All test function names keep their Python spelling so the two suites can be
//! diffed.

mod common;

use common::{Daemon, LegacyFixture, Response};
use serde_json::{json, Value};
use std::path::Path;

struct Live {
    fixture: LegacyFixture,
    _daemon: Daemon,
}

impl Live {
    fn new() -> Live {
        let fixture = LegacyFixture::new();
        let daemon = fixture.start_daemon();
        Live {
            fixture,
            _daemon: daemon,
        }
    }

    fn request(&self, method: &str, path: &str, body: Option<&Value>) -> Response {
        self.fixture.request(method, path, body)
    }

    fn machctl(&self, arguments: &[&str]) -> std::process::Output {
        common::run_machctl(arguments, &self.fixture.machctl_env())
    }

    fn machctl_ok(&self, arguments: &[&str]) -> std::process::Output {
        let output = self.machctl(arguments);
        assert!(
            output.status.success(),
            "machctl {arguments:?} failed rc={:?}:\nstdout: {}\nstderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        output
    }
}

// ---------------------------------------------------------------- HTTP API

#[test]
fn test_application_catalog_new_layout_and_exact_missing_paths() {
    let live = Live::new();
    let response = live.request("GET", "/applications", None);
    assert_eq!(response.status, 200);
    assert_eq!(
        response.body["images"][0]["applications"][0]["name"],
        json!("Fixture App")
    );
    let state = live
        .fixture
        .state_dir
        .canonicalize()
        .expect("canonical state dir");
    let inventory = state.join("pilot-images/images/ubuntu2404/applications.json");
    let association = std::fs::read_dir(state.join("pilot-state/stores"))
        .expect("stores dir")
        .map(|entry| {
            entry
                .expect("store entry")
                .path()
                .join("base/ubuntu2404.json")
        })
        .next()
        .expect("ubuntu2404 association");
    std::fs::remove_file(&inventory).expect("remove inventory");
    std::fs::remove_file(&association).expect("remove association");
    let response = live.request("GET", "/applications", None);
    assert_eq!(response.status, 503);
    let error = response.body["error"].as_str().expect("error text");
    assert!(
        error.contains(&inventory.to_string_lossy().to_string()),
        "{error}"
    );
    assert!(
        error.contains(&association.to_string_lossy().to_string()),
        "{error}"
    );
    assert!(!inventory.exists());
    assert!(!association.exists());
}

#[test]
fn test_explicit_work_clone_retains_source_proof_and_leaves_work_untouched() {
    let live = Live::new();
    let response = live.request(
        "POST",
        "/acquire",
        Some(&json!({"purpose": "work-check", "image": "ubuntu2404", "source": "work", "env": "none"})),
    );
    assert_eq!(response.status, 200, "{}", response.body);
    let lease = response.body;
    assert_eq!(lease["source"], "work");
    assert_eq!(lease["source_vm"], "pilot-ubuntu-work");
    assert_eq!(lease["source_fingerprint"]["base_vm"], "pilot-ubuntu-work");
    assert_ne!(lease["vm"], lease["source_vm"]);
    let response = live.request(
        "POST",
        "/acquire",
        Some(&json!({"purpose": "bad-work", "image": "ubuntu2404", "source": "work", "env": "none", "expected_source_fingerprint": {"wrong": true}})),
    );
    assert_eq!(response.status, 409);
    assert!(response.body["error"]
        .as_str()
        .expect("error")
        .contains("fingerprint"));
    let vm = lease["vm"].as_str().expect("vm").to_string();
    let response = live.request("POST", &format!("/machines/{vm}/release"), Some(&json!({})));
    assert_eq!(response.status, 200);
    let path = lease["source_fingerprint"]["path"]
        .as_str()
        .expect("fingerprint path");
    assert!(Path::new(path).exists());
}

#[test]
fn test_work_clone_rejects_credential_packs_and_unknown_source() {
    let live = Live::new();
    for extra in [
        json!({"source": "work", "env": "default"}),
        json!({"source": "/tmp/arbitrary", "env": "none"}),
    ] {
        let mut body = json!({"purpose": "bad", "image": "ubuntu2404"});
        for (key, value) in extra.as_object().expect("object") {
            body[key] = value.clone();
        }
        let response = live.request("POST", "/acquire", Some(&body));
        assert_eq!(response.status, 409, "{}", response.body);
    }
}

#[test]
fn test_exec_request_timeout_reaches_ssh_without_clamping() {
    let live = Live::new();
    let response = live.request(
        "POST",
        "/acquire",
        Some(&json!({"purpose": "timeouts", "image": "ubuntu2404", "env": "none"})),
    );
    assert_eq!(response.status, 200);
    let vm = response.body["vm"].as_str().expect("vm").to_string();
    for timeout in [3600, 3720, 7200] {
        for command in [
            json!({"argv": ["fixture-timeout"]}),
            json!({"script": "fixture-timeout"}),
        ] {
            let mut body = command.clone();
            body["timeout"] = json!(timeout);
            let response = live.request("POST", &format!("/machines/{vm}/exec"), Some(&body));
            assert_eq!(response.status, 200, "{}", response.body);
            // Deviation: the external `ssh` shim cannot see the request
            // deadline, so assert the command crossed the process boundary
            // instead of comparing the echoed timeout.
            let observed = if body.get("script").is_some() {
                live.fixture.ssh_stdin()
            } else {
                live.fixture.ssh_calls()
            };
            assert!(
                observed.contains("fixture-timeout"),
                "timeout {timeout} did not reach ssh: {observed}"
            );
        }
    }
}

#[test]
fn test_health() {
    let live = Live::new();
    let response = live.request("GET", "/health", None);
    assert_eq!(response.status, 200);
    assert_eq!(response.body["ok"], json!(true));
    assert_eq!(response.body["service"], json!("machine-service"));
    // FD-1: the instance ID and the kinds it offers; exposure is off by
    // default, so no durable kind (AC-43).
    assert!(response.body["instance"]
        .as_str()
        .is_some_and(|id| !id.is_empty()));
    assert!(!response.body["kinds"]
        .as_array()
        .expect("kinds")
        .contains(&json!("durable")));
    // The catalog status rides along (IC-5).
    assert_eq!(response.body["catalog"]["status"], "current");
    assert_eq!(response.body["catalog"]["generation"], 1);
    assert!(response.body["catalog"]["built_at"].as_f64().is_some());
}

#[test]
fn test_unknown_route_404() {
    let live = Live::new();
    let response = live.request("GET", "/nope", None);
    assert_eq!(response.status, 404);
    assert!(response.body.get("error").is_some());
}

#[test]
fn test_route_matching_keeps_the_query_string() {
    // CPython compares the raw `BaseHTTPRequestHandler.self.path`, which still
    // carries the query string, so a request whose target has a query matches
    // no route. Every case below therefore expects the generic 404 that the
    // Python daemon sends, not the 200 the route would send without a query.
    let live = Live::new();
    for path in [
        "/health?x=1",
        "/machines?x=1",
        "/images?x=1",
        "/capabilities?x=1",
        "/acquisition-capabilities?x=1",
        "/applications?x=1",
    ] {
        let response = live.request("GET", path, None);
        assert_eq!(response.status, 404, "GET {path}: {}", response.body);
        assert_eq!(response.body["error"], "not found", "GET {path}");
    }
    // The VM route captures `[^/]+`, so a query becomes part of the captured
    // name and the lookup reports the unknown-VM error instead.
    let response = live.request("GET", "/machines/ghost?x=1", None);
    assert_eq!(response.status, 404);
    assert_eq!(response.body["error"], "unknown VM");
    // Mutating routes are unroutable with a query as well, and must not run.
    let response = live.request("POST", "/gc?x=1", None);
    assert_eq!(response.status, 404, "{}", response.body);
    assert_eq!(response.body["error"], "not found");
    let response = live.request(
        "POST",
        "/acquire?x=1",
        Some(&json!({"purpose": "query-string-must-not-acquire"})),
    );
    assert_eq!(response.status, 404, "{}", response.body);
    assert_eq!(response.body["error"], "not found");
    assert!(live.fixture.vms_empty(), "no lease may be created");
    // A VM action route and the console route are unroutable with a query too.
    let response = live.request(
        "POST",
        "/machines/ghost/exec?x=1",
        Some(&json!({"argv": ["true"]})),
    );
    assert_eq!(response.status, 404, "{}", response.body);
    assert_eq!(response.body["error"], "not found");
    let response = live.request(
        "POST",
        "/machines/ghost/console/resolve?x=1",
        Some(&json!({"lease_id": "0123456789abcdef0123456789abcdef"})),
    );
    assert_eq!(response.status, 404, "{}", response.body);
    assert_eq!(response.body["error"], "not found");
    // The same routes without a query keep working.
    assert_eq!(live.request("GET", "/health", None).status, 200);
    assert_eq!(live.request("GET", "/machines", None).status, 200);
}

#[test]
fn test_unknown_vm_404() {
    let live = Live::new();
    let response = live.request("GET", "/machines/ghost", None);
    assert_eq!(response.status, 404);
    assert_eq!(response.body["error"], "unknown VM");
}

#[test]
fn test_acquire_missing_purpose_is_409() {
    let live = Live::new();
    let response = live.request("POST", "/acquire", Some(&json!({})));
    assert_eq!(response.status, 409);
    assert!(response.body["error"]
        .as_str()
        .expect("error")
        .contains("purpose"));
}

#[test]
fn test_acquire_new_field_names() {
    let live = Live::new();
    let response = live.request(
        "POST",
        "/acquire",
        Some(&json!({"purpose": "http-a", "image": "ubuntu2404", "env": "none"})),
    );
    assert_eq!(response.status, 200, "{}", response.body);
    let record = response.body;
    assert_eq!(record["state"], "running");
    assert_eq!(record["image"], "ubuntu2404");
    assert_eq!(record["env"], Value::Null);
    assert_eq!(record["image_kind"], "linux");
}

/// `test_invalid_values` (Options): the JSON/HTTP boundary rejects a boolean, a
/// float, or a string where an integer resource is expected, matching the
/// Python `resolve(**kwargs)` `ValueError` for `True`, `1.5`, and `"6"`.
///
/// The typed `acquisition-options::resolve` signature cannot carry those
/// values, so only the daemon's `check_option_integers` boundary can assert
/// them; `0` and `-1` are additionally refused downstream with the same text.
#[test]
fn test_acquire_rejects_non_integer_resources_at_http_boundary() {
    let live = Live::new();
    let cases = [
        ("cpu", json!(0)),
        ("cpu", json!(-1)),
        ("cpu", json!(true)),
        ("cpu", json!(1.5)),
        ("cpu", json!("6")),
        ("memory_mb", json!(0)),
        ("memory_mb", json!(-1)),
        ("memory_mb", json!(true)),
        ("memory_mb", json!(1.5)),
        ("memory_mb", json!("6")),
        ("disk_gb", json!(0)),
        ("disk_gb", json!(-1)),
        ("disk_gb", json!(true)),
        ("disk_gb", json!(1.5)),
        ("disk_gb", json!("6")),
    ];
    for (index, (field, value)) in cases.into_iter().enumerate() {
        let mut body = json!({
            "purpose": format!("bad-resource-{index}"),
            "image": "ubuntu2404",
            "env": "none",
        });
        body[field] = value.clone();
        let response = live.request("POST", "/acquire", Some(&body));
        assert_eq!(
            response.status, 409,
            "{field}={value} was not refused: {}",
            response.body
        );
        assert!(
            response.body["error"]
                .as_str()
                .unwrap_or_default()
                .contains("must be a positive integer or null"),
            "{field}={value}: {}",
            response.body
        );
    }
}

#[test]
fn test_acquire_old_field_aliases_on_the_wire() {
    let live = Live::new();
    let response = live.request(
        "POST",
        "/acquire",
        Some(&json!({"purpose": "http-b", "line": "ubuntu2404", "pack": "default"})),
    );
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(response.body["image"], "ubuntu2404");
    assert_eq!(response.body["env"], "default");
    let response = live.request(
        "POST",
        "/acquire",
        Some(&json!({"purpose": "http-c", "line": "ubuntu2404", "lane": "default"})),
    );
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(response.body["env"], "default");
}

#[test]
fn test_purpose_collision_409() {
    let live = Live::new();
    live.request(
        "POST",
        "/acquire",
        Some(&json!({"purpose": "dup", "image": "ubuntu2404", "env": "none"})),
    );
    let response = live.request(
        "POST",
        "/acquire",
        Some(&json!({"purpose": "dup", "image": "ubuntu2404", "env": "none"})),
    );
    assert_eq!(response.status, 409);
    assert!(response.body["error"]
        .as_str()
        .expect("error")
        .contains("already leased"));
}

#[test]
fn test_macos_limit_409() {
    let live = Live::new();
    for index in [1, 2] {
        let response = live.request(
            "POST",
            "/acquire",
            Some(&json!({"purpose": format!("mac{index}"), "env": "none"})),
        );
        assert_eq!(response.status, 200, "{}", response.body);
    }
    let response = live.request(
        "POST",
        "/acquire",
        Some(&json!({"purpose": "mac3", "env": "none"})),
    );
    assert_eq!(response.status, 409);
    assert!(response.body["error"]
        .as_str()
        .expect("error")
        .contains("limit reached"));
}

#[test]
fn test_purpose_validation_409() {
    let live = Live::new();
    let response = live.request(
        "POST",
        "/acquire",
        Some(&json!({"purpose": "BAD PURPOSE", "env": "none"})),
    );
    assert_eq!(response.status, 409);
    assert!(response.body["error"]
        .as_str()
        .expect("error")
        .contains("purpose"));
}

#[test]
fn test_unknown_image_409() {
    let live = Live::new();
    let response = live.request(
        "POST",
        "/acquire",
        Some(&json!({"purpose": "x", "image": "nope", "env": "none"})),
    );
    assert_eq!(response.status, 409);
    assert!(response.body["error"]
        .as_str()
        .expect("error")
        .contains("unknown image"));
}

#[test]
fn test_state_persists_across_requests() {
    let live = Live::new();
    let response = live.request(
        "POST",
        "/acquire",
        Some(&json!({"purpose": "persist", "image": "ubuntu2404", "env": "none"})),
    );
    let record = response.body;
    let vm = record["vm"].as_str().expect("vm").to_string();
    let response = live.request("GET", &format!("/machines/{vm}"), None);
    assert_eq!(response.status, 200);
    assert_eq!(response.body["vm"], record["vm"]);
    assert_eq!(response.body["state"], "running");
    let state = live.fixture.read_state();
    assert!(state.contains_key(&vm));
}

#[test]
fn test_release_roundtrip() {
    let live = Live::new();
    let response = live.request(
        "POST",
        "/acquire",
        Some(&json!({"purpose": "rel", "image": "ubuntu2404", "env": "none"})),
    );
    let vm = response.body["vm"].as_str().expect("vm").to_string();
    let response = live.request(
        "POST",
        &format!("/machines/{vm}/release"),
        Some(&json!({"reason": "test"})),
    );
    assert_eq!(response.status, 200);
    assert!(response.body["released"].as_bool().unwrap_or(false));
    let response = live.request("GET", &format!("/machines/{vm}"), None);
    assert_eq!(response.status, 404);
}

#[test]
fn test_heartbeat_roundtrip_and_bounds() {
    let live = Live::new();
    let response = live.request(
        "POST",
        "/acquire",
        Some(&json!({"purpose": "hb", "image": "ubuntu2404", "env": "none"})),
    );
    let vm = response.body["vm"].as_str().expect("vm").to_string();
    let response = live.request(
        "POST",
        &format!("/machines/{vm}/heartbeat"),
        Some(&json!({"ttl_hours": 5})),
    );
    assert_eq!(response.status, 200);
    let remaining = response.body["ttl_hours_remaining"]
        .as_f64()
        .expect("ttl remaining");
    assert!((remaining - 5.0).abs() <= 0.02);
    let response = live.request(
        "POST",
        &format!("/machines/{vm}/heartbeat"),
        Some(&json!({"ttl_hours": 9999})),
    );
    assert_eq!(response.status, 409);
}

#[test]
fn test_exec_requires_argv_or_script() {
    let live = Live::new();
    let response = live.request(
        "POST",
        "/acquire",
        Some(&json!({"purpose": "ex", "image": "ubuntu2404", "env": "none"})),
    );
    let vm = response.body["vm"].as_str().expect("vm").to_string();
    let response = live.request("POST", &format!("/machines/{vm}/exec"), Some(&json!({})));
    assert_eq!(response.status, 409);
    assert!(response.body["error"]
        .as_str()
        .expect("error")
        .contains("argv"));
}

#[test]
fn test_exec_rejects_non_running() {
    let live = Live::new();
    let response = live.request(
        "POST",
        "/acquire",
        Some(&json!({"purpose": "nr", "image": "ubuntu2404", "env": "none"})),
    );
    let vm = response.body["vm"].as_str().expect("vm").to_string();
    let state_file = live.fixture.state_file();
    let mut state: Value =
        serde_json::from_str(&std::fs::read_to_string(&state_file).expect("state")).expect("json");
    state["vms"][&vm]["state"] = json!("provisioning");
    std::fs::write(&state_file, state.to_string()).expect("write state");
    let response = live.request(
        "POST",
        &format!("/machines/{vm}/exec"),
        Some(&json!({"argv": ["true"]})),
    );
    assert_eq!(response.status, 409);
    assert!(response.body["error"]
        .as_str()
        .expect("error")
        .contains("not running"));
}

#[test]
fn test_exec_ok() {
    let live = Live::new();
    let response = live.request(
        "POST",
        "/acquire",
        Some(&json!({"purpose": "exec-ok", "image": "ubuntu2404", "env": "none"})),
    );
    let vm = response.body["vm"].as_str().expect("vm").to_string();
    let response = live.request(
        "POST",
        &format!("/machines/{vm}/exec"),
        Some(&json!({"argv": ["echo", "hi"]})),
    );
    assert_eq!(response.status, 200);
    assert_eq!(response.body["rc"], 0);
    assert!(response.body["output"]
        .as_str()
        .expect("output")
        .contains("echo hi"));
}

#[test]
fn test_gc_endpoint() {
    let live = Live::new();
    let response = live.request("POST", "/gc", Some(&json!({})));
    assert_eq!(response.status, 200);
    assert_eq!(response.body, json!({"gc": "done"}));
}

// ---------------------------------------------------------------- snapshots

#[test]
fn test_images_snapshot_shape() {
    let live = Live::new();
    let response = live.request("GET", "/images", None);
    assert_eq!(response.status, 200, "{}", response.body);
    assert!(response.body["images"].get("macos26").is_some());
    assert!(response.body["images"].get("ubuntu2404").is_some());
    assert_eq!(
        response.body["images"]["macos26"]["concurrency"]["limit"],
        2
    );
    assert_eq!(response.body["images"]["macos26"]["base_available"], true);

    // This daemon holds no leases, so its own macOS running count is exactly 0
    // and the limit is the configured line default.
    let capacity = &response.body["capacity"];
    assert_eq!(capacity["macos_running"], 0);
    assert_eq!(capacity["macos_limit"], 2);

    // The host gauge is host-wide: `host_macos_guests` shells out to
    // `/usr/bin/pgrep` by absolute path, so the PATH shims cannot intercept it.
    // The Python fixture stubbed that function to 0; here the true value varies
    // while other VMs come and go, so asserting the host's momentary guest
    // count would be a flake rather than a test. Assert the shape and the
    // derivation the daemon documents instead.
    match capacity.get("host_macos_guests") {
        Some(guests) => {
            let guests = guests.as_u64().expect("host gauge is a count");
            let running = capacity["macos_running"]
                .as_u64()
                .expect("macos_running is a count");
            assert_eq!(
                capacity["foreign_macos_guests"],
                json!(guests.saturating_sub(running)),
                "foreign guests are the host count minus this daemon's own"
            );
        }
        None => assert!(
            capacity.get("foreign_macos_guests").is_none(),
            "an unreadable gauge reports neither field"
        ),
    }
}

#[test]
fn test_vms_snapshot_includes_limits() {
    let live = Live::new();
    let response = live.request("GET", "/machines", None);
    assert_eq!(response.status, 200);
    assert_eq!(response.body["limits"]["max_macos_running"], 2);
    assert_eq!(response.body["limits"]["grace_hours"], 6);
}

// ---------------------------------------------------------------- machctl CLI

#[test]
fn test_list_empty() {
    let live = Live::new();
    let output = live.machctl_ok(&["list", "--local"]);
    // CL-1, MM-3: with no lease the instance lists its two offers.
    let text = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[0].starts_with("INSTANCE"), "{text}");
    assert_eq!(lines.len(), 3, "{text}");
    for (line, image) in lines[1..].iter().zip(["macos26", "ubuntu2404"]) {
        assert!(line.contains(image) && line.contains(" offer "), "{text}");
    }
}

#[test]
fn test_list_json() {
    let live = Live::new();
    let output = live.machctl_ok(&["list", "--local", "--json"]);
    let parsed: Value = serde_json::from_slice(&output.stdout).expect("list json");
    // CL-1, TL-2: explicit local scope keeps real LAN instances out of
    // this fixture, regardless of the default discovery mode.
    let instances = parsed.as_array().expect("one object per instance");
    assert_eq!(instances.len(), 1, "{parsed}");
    assert_eq!(instances[0]["local"], true);
    assert_eq!(instances[0]["reachable"], true);
    let offers: Vec<&str> = instances[0]["machines"]
        .as_array()
        .expect("machines")
        .iter()
        .map(|record| {
            assert_eq!(record["state"], "offer", "{record}");
            record["name"].as_str().expect("name")
        })
        .collect();
    assert_eq!(offers, ["macos26", "ubuntu2404"], "{parsed}");
    assert_eq!(instances[0]["catalog"]["status"], "current");
}

#[test]
fn test_images_table_and_json() {
    let live = Live::new();
    let output = live.machctl_ok(&["images"]);
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(text.contains("macos26"));
    assert!(text.contains("acquirable="));
    let output = live.machctl_ok(&["images", "--json"]);
    let parsed: Value = serde_json::from_slice(&output.stdout).expect("images json");
    // One object per instance, its answer unchanged (CL-4).
    assert_eq!(parsed[0]["local"], true);
    assert_eq!(parsed[0]["answer"]["capacity"]["macos_limit"], 2);
}

#[test]
fn test_images_show() {
    let live = Live::new();
    let output = live.machctl_ok(&["images-show", "ubuntu2404"]);
    let parsed: Value = serde_json::from_slice(&output.stdout).expect("image json");
    // One object per instance that lists the image (CL-4).
    assert_eq!(parsed[0]["answer"]["kind"], "linux");
    assert_eq!(parsed[0]["local"], true);
}

#[test]
fn test_images_show_unknown_image() {
    let live = Live::new();
    let output = live.machctl(&["images-show", "nope"]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown image 'nope'"), "{stderr}");
    assert!(stderr.contains("macos26, ubuntu2404"), "{stderr}");
}

#[test]
fn test_acquire_and_list() {
    let live = Live::new();
    let output = live.machctl_ok(&[
        "acquire",
        "--purpose",
        "cli-a",
        "--image",
        "ubuntu2404",
        "--env",
        "none",
    ]);
    let record: Value = serde_json::from_slice(&output.stdout).expect("acquire json");
    assert_eq!(record["state"], "running");
    let output = live.machctl_ok(&["list", "--local"]);
    let text = String::from_utf8_lossy(&output.stdout);
    let vm = record["vm"].as_str().expect("vm");
    let row = text
        .lines()
        .find(|line| line.contains(vm))
        .unwrap_or_else(|| panic!("no row for {vm}: {text}"));
    assert!(row.contains(" virtual "), "{row}");
    assert!(row.contains(" running "), "{row}");
    assert!(row.contains("cli-a, "), "{row}");
    assert!(row.contains("(ubuntu2404)"), "{row}");
}

#[test]
fn test_cli_work_source_and_expected_fingerprint() {
    let live = Live::new();
    let output = live.machctl_ok(&[
        "acquire",
        "--purpose",
        "cli-work",
        "--image",
        "ubuntu2404",
        "--source",
        "work",
        "--env",
        "none",
    ]);
    let lease: Value = serde_json::from_slice(&output.stdout).expect("lease json");
    assert_eq!(lease["source"], "work");
    let fingerprint_file = live.fixture.state_dir.join("expected-work.json");
    std::fs::write(
        &fingerprint_file,
        serde_json::to_vec(&lease["source_fingerprint"]).expect("fingerprint json"),
    )
    .expect("write fingerprint");
    live.machctl_ok(&["release", lease["vm"].as_str().expect("vm")]);
    let output = live.machctl_ok(&[
        "acquire",
        "--purpose",
        "cli-work2",
        "--image",
        "ubuntu2404",
        "--source",
        "work",
        "--env",
        "none",
        "--expected-source-fingerprint",
        fingerprint_file.to_str().expect("path"),
    ]);
    let second: Value = serde_json::from_slice(&output.stdout).expect("second lease");
    assert_eq!(second["source_fingerprint"], lease["source_fingerprint"]);
    let refused = live.machctl(&["acquire", "--purpose", "bad-work", "--source", "work"]);
    assert_ne!(refused.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("--env none"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
}

#[test]
fn test_acquire_old_flag_aliases() {
    let live = Live::new();
    let output = live.machctl_ok(&[
        "acquire",
        "--purpose",
        "cli-b",
        "--line",
        "ubuntu2404",
        "--pack",
        "default",
    ]);
    let record: Value = serde_json::from_slice(&output.stdout).expect("acquire json");
    assert_eq!(record["image"], "ubuntu2404");
    assert_eq!(record["env"], "default");
}

#[test]
fn test_acquire_purpose_collision_message() {
    let live = Live::new();
    live.machctl_ok(&[
        "acquire",
        "--purpose",
        "cli-c",
        "--image",
        "ubuntu2404",
        "--env",
        "none",
    ]);
    let output = live.machctl(&[
        "acquire",
        "--purpose",
        "cli-c",
        "--image",
        "ubuntu2404",
        "--env",
        "none",
    ]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("already leased"), "{stderr}");
    assert!(stderr.contains("machctl release"), "{stderr}");
}

#[test]
fn test_acquire_limit_message() {
    let live = Live::new();
    for index in [1, 2] {
        live.machctl_ok(&[
            "acquire",
            "--purpose",
            &format!("cli-m{index}"),
            "--env",
            "none",
        ]);
    }
    let output = live.machctl(&["acquire", "--purpose", "cli-m3", "--env", "none"]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("limit reached"), "{stderr}");
    assert!(stderr.contains("free capacity"), "{stderr}");
}

#[test]
fn test_acquire_bad_purpose_message() {
    let live = Live::new();
    let output = live.machctl(&["acquire", "--purpose", "BAD"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("purpose"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_status_unknown_vm() {
    let live = Live::new();
    let output = live.machctl(&["status", "ghost"]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown VM"), "{stderr}");
    assert!(stderr.contains("machctl list"), "{stderr}");
}

#[test]
fn test_heartbeat_cli() {
    let live = Live::new();
    let response = live.request(
        "POST",
        "/acquire",
        Some(&json!({"purpose": "cli-hb", "image": "ubuntu2404", "env": "none"})),
    );
    let vm = response.body["vm"].as_str().expect("vm").to_string();
    let output = live.machctl_ok(&["heartbeat", &vm, "--ttl-hours", "6"]);
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ttl_hours_remaining=6"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn test_release_cli() {
    let live = Live::new();
    let response = live.request(
        "POST",
        "/acquire",
        Some(&json!({"purpose": "cli-rel", "image": "ubuntu2404", "env": "none"})),
    );
    let vm = response.body["vm"].as_str().expect("vm").to_string();
    let output = live.machctl_ok(&["release", &vm]);
    assert!(
        String::from_utf8_lossy(&output.stdout).contains(&format!("released {vm}")),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let response = live.request("GET", &format!("/machines/{vm}"), None);
    assert_eq!(response.status, 404);
}

#[test]
fn test_exec_cli_explicit_timeout_is_forwarded() {
    let live = Live::new();
    let output = live.machctl_ok(&[
        "acquire",
        "--purpose",
        "cli-timeout",
        "--image",
        "ubuntu2404",
        "--env",
        "none",
    ]);
    let lease: Value = serde_json::from_slice(&output.stdout).expect("lease json");
    let vm = lease["vm"].as_str().expect("vm").to_string();
    let result = live.machctl_ok(&["exec", &vm, "--timeout", "3600", "--", "fixture-timeout"]);
    // Deviation: a real external `ssh` cannot see the deadline, so assert the
    // command reached the guest boundary. The exact deadline is pinned by
    // `machine-service-core/tests/exec_timeout.rs`.
    assert!(
        String::from_utf8_lossy(&result.stdout).contains("fixture-timeout"),
        "{}",
        String::from_utf8_lossy(&result.stdout)
    );
    let refused = live.machctl(&["exec", &vm, "--timeout", "0", "--", "fixture-timeout"]);
    assert_ne!(refused.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("positive"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
}

#[test]
fn test_exec_cli_rc_passthrough() {
    let live = Live::new();
    let response = live.request(
        "POST",
        "/acquire",
        Some(&json!({"purpose": "cli-ex", "image": "ubuntu2404", "env": "none"})),
    );
    let vm = response.body["vm"].as_str().expect("vm").to_string();
    let output = live.machctl_ok(&["exec", &vm, "--", "echo", "cli-hello"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("echo cli-hello"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn test_exec_cli_nonzero_rc_propagates() {
    let live = Live::new();
    let response = live.request(
        "POST",
        "/acquire",
        Some(&json!({"purpose": "cli-ex2", "image": "ubuntu2404", "env": "none"})),
    );
    let vm = response.body["vm"].as_str().expect("vm").to_string();
    let output = live.machctl(&["exec", &vm, "--", "false"]);
    assert_eq!(output.status.code(), Some(1));
}

#[test]
fn test_gc_cli() {
    let live = Live::new();
    let output = live.machctl_ok(&["gc"]);
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "gc done");
}

/// AC-28, FD-4 (DC-70): bytes travel over HTTP to and from a virtual
/// machine through its guest link, a file as itself and a folder as a tar
/// stream; the staging folder is gone afterwards.
#[test]
fn test_ac28_byte_transfers_to_and_from_a_virtual_machine() {
    let live = Live::new();
    let lease = live.request(
        "POST",
        "/acquire",
        Some(&json!({"purpose": "bytes", "image": "ubuntu2404", "env": "none"})),
    );
    assert_eq!(lease.status, 200, "{}", lease.body);
    let vm = lease.body["vm"].as_str().expect("vm").to_string();
    let port = live.fixture.port();
    let files = |path: &str| format!("/machines/{vm}/files?path={path}");

    let bytes = b"guest bytes";
    let digest = common::sha256_hex(bytes);
    let (status, _, body) = common::raw_http(
        port,
        "PUT",
        &files("/tmp/in.txt"),
        &[
            ("Content-Type", "application/octet-stream"),
            ("X-Content-SHA256", &digest),
        ],
        bytes,
    );
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let (status, headers, body) = common::raw_http(port, "GET", &files("/tmp/in.txt"), &[], b"");
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert_eq!(body, bytes);
    assert!(headers
        .iter()
        .any(|(name, value)| name == "x-content-sha256" && *value == digest));

    let scratch = tempfile::tempdir().expect("scratch");
    let tree = scratch.path().join("tree");
    std::fs::create_dir_all(tree.join("nested")).expect("tree");
    std::fs::write(tree.join("nested/c.txt"), "sea").expect("c.txt");
    let archive = scratch.path().join("tree.tar");
    byte_transfer::pack(&tree, &archive).expect("pack");
    let tar = std::fs::read(&archive).expect("tar");
    let (status, _, body) = common::raw_http(
        port,
        "PUT",
        &format!("{}&name=tree", files("/tmp/tree")),
        &[
            ("Content-Type", "application/x-tar"),
            ("X-Content-SHA256", &common::sha256_hex(&tar)),
        ],
        &tar,
    );
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let (status, headers, body) = common::raw_http(port, "GET", &files("/tmp/tree"), &[], b"");
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert!(headers
        .iter()
        .any(|(name, value)| name == "content-type" && value == "application/x-tar"));
    let fetched = scratch.path().join("fetched.tar");
    std::fs::write(&fetched, &body).expect("fetched");
    let unpacked = scratch.path().join("unpacked");
    byte_transfer::unpack(&fetched, &unpacked).expect("unpack");
    assert_eq!(
        std::fs::read_to_string(unpacked.join("nested/c.txt")).expect("c.txt"),
        "sea"
    );

    // An unknown machine is refused before any byte is kept.
    let (status, _, _) = common::raw_http(
        port,
        "PUT",
        "/machines/nope/files?path=/tmp/x",
        &[
            ("Content-Type", "application/octet-stream"),
            ("X-Content-SHA256", &digest),
        ],
        bytes,
    );
    assert_eq!(status, 404);
    let staged = live.fixture.state_dir.join("transfers");
    let left: Vec<_> = std::fs::read_dir(&staged)
        .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
        .unwrap_or_default();
    assert!(left.is_empty(), "{left:?}");
    let released = live.request("POST", &format!("/machines/{vm}/release"), Some(&json!({})));
    assert_eq!(released.status, 200, "{}", released.body);
}
