//! Port of `tests/integration/test_console_http.py`.
//!
//! Real HTTP contract with a fake lifecycle; never starts a VM or viewer. The
//! Python suite shared `tests/common.py::ServiceFixture` and enabled the
//! console by replacing `svc.CONSOLES` in-process. This port starts the real
//! daemon and uses the PATH `tart`/`ssh`/`scp` shims instead.
//!
//! Forced deviation: `test_fresh_lease_exact_cancel_and_duplicate_open` is
//! `#[ignore]`d. The Python test replaced `CONSOLES.prepare` with a stub that
//! marked the session ready without a guest. The Rust `Manager::prepare`
//! uploads the guest agent and probes readiness through a hard-coded
//! `/usr/bin/ssh`, which a `PATH` shim cannot intercept, so a live guest would
//! be required. The no-viewer-replay half of that test is already pinned by
//! `crates/console/src/sessions.rs::cancel_before_open_never_replays_a_worker`.

mod common;

use common::{Daemon, LegacyFixture, Response};
use serde_json::{json, Value};

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

    fn call(&self, path: &str, body: Option<&Value>) -> Response {
        let method = if body.is_some() { "POST" } else { "GET" };
        self.fixture.request(method, path, body)
    }
}

#[test]
fn test_discovery_is_inert_and_cli_can_read_it() {
    let live = Live::new();
    // The Python test patched `tart` and `STATE.read` to raise. The process
    // boundary cannot be patched, so assert the observable equivalents: no
    // `tart` invocation and no state file.
    let response = live.call("/acquisition-capabilities", None);
    assert_eq!(response.status, 200);
    assert_eq!(response.body["options"]["vnc"]["default"], false);
    assert_eq!(
        response.body["options"]["vnc"]["backends"]["linux"]["available"],
        false
    );
    assert!(
        live.fixture.tart_calls().is_empty(),
        "discovery invoked Tart: {}",
        live.fixture.tart_calls()
    );
    let output = common::run_machctl(&["acquisition-capabilities"], &live.fixture.machctl_env());
    assert!(
        output.status.success(),
        "machctl acquisition-capabilities failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let parsed: Value = serde_json::from_slice(&output.stdout).expect("capabilities json");
    // One object per instance, its answer unchanged (CL-4).
    assert_eq!(parsed[0]["answer"]["schemaVersion"], 1);
    // Deviation: the Python in-process fixture never started `serve()`, so no
    // GC loop existed to persist an empty state file. The real daemon (Python
    // and Rust alike) does. The observable intent is that discovery created no
    // lease, which is what this asserts.
    assert!(live.fixture.vms_empty(), "discovery mutated lease state");
}

#[test]
fn test_vnc_is_rejected_before_clone_when_unconfigured() {
    let live = Live::new();
    let response = live.call(
        "/acquire",
        Some(&json!({"purpose": "watch", "image": "ubuntu2404", "env": "none", "vnc": true})),
    );
    assert_eq!(response.status, 409, "{}", response.body);
    assert!(
        live.fixture.tart_calls().is_empty(),
        "clone ran before the VNC rejection: {}",
        live.fixture.tart_calls()
    );
}

#[test]
#[ignore = "test_fresh_lease_exact_cancel_and_duplicate_open: process-level port cannot \
            substitute the in-process `CONSOLES.prepare` stub; the Rust manager uploads the agent \
            and probes through a hard-coded /usr/bin/ssh, which needs a live guest. The \
            no-viewer-replay half is covered by \
            crates/console/src/sessions.rs::cancel_before_open_never_replays_a_worker"]
fn test_fresh_lease_exact_cancel_and_duplicate_open() {
    // Intentionally empty: ported Python test kept for traceability.
}

#[test]
fn test_legacy_console_request_fails_without_retrofit() {
    let live = Live::new();
    let response = live.call(
        "/acquire",
        Some(&json!({"purpose": "ordinary", "env": "none"})),
    );
    let vm = response.body["vm"].as_str().expect("vm").to_string();
    let lease_id = response.body["lease_id"]
        .as_str()
        .expect("lease_id")
        .to_string();
    let response = live.call(
        &format!("/machines/{vm}/console/resolve"),
        Some(&json!({"lease_id": lease_id})),
    );
    assert_eq!(response.status, 409, "{}", response.body);
    let record = live
        .fixture
        .request("GET", &format!("/machines/{vm}"), None);
    assert!(
        record.body.get("console").is_none(),
        "console was retrofitted: {}",
        record.body
    );
}

#[test]
fn test_invalid_vnc_type_does_not_allocate() {
    let live = Live::new();
    for value in [json!("true"), json!(1), Value::Null] {
        let response = live.call(
            "/acquire",
            Some(&json!({"purpose": "invalid", "env": "none", "vnc": value})),
        );
        assert_eq!(response.status, 409, "vnc {value}: {}", response.body);
    }
    assert!(
        live.fixture.tart_calls().is_empty(),
        "invalid vnc allocated: {}",
        live.fixture.tart_calls()
    );
}
