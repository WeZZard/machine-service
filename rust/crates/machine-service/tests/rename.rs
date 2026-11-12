//! The coordinated switch to machine-service (MG-3): old names fail loudly.
//!
//! The real `machine-service` binary runs against the fake-Tart fixture in
//! `tests/common/mod.rs`.

mod common;

use std::process::Stdio;
use std::time::{Duration, Instant};

use common::{free_port, run_machctl, try_http, LegacyFixture};
use serde_json::json;

/// AC-36: machine-service started with `VM_SERVICE_PORT` set refuses to
/// start, and names the variable and its replacement `MACHINE_SERVICE_PORT`.
#[test]
fn ac36_daemon_refuses_an_old_variable() {
    let fixture = LegacyFixture::new();
    let port = free_port();
    let err = fixture.root.join("daemon.err");
    let mut command = common::daemon_command(&fixture.root);
    command
        .envs(fixture.env())
        .env("MACHINE_SERVICE_PORT", port.to_string())
        .env("VM_SERVICE_PORT", "6290")
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&err).expect("stderr file"));
    let mut child = command.spawn().expect("spawn machine-service");

    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait") {
            break status;
        }
        if Instant::now() >= deadline {
            panic!("machine-service kept running with VM_SERVICE_PORT set");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stderr = std::fs::read_to_string(&err).unwrap_or_default();

    assert!(!status.success());
    assert!(
        stderr.contains("VM_SERVICE_PORT is now MACHINE_SERVICE_PORT"),
        "{stderr}"
    );
    assert!(try_http(port, "GET", "/health", None, None).is_err());
}

/// AC-35: after the coordinated switch, `GET /vms` and every `/vms/<name>`
/// path answer HTTP 410 Gone with the `/machines` path that replaces them,
/// and no operation runs; `/machines` serves what `/vms` served, and
/// `machctl list` reads it.
#[test]
fn ac35_old_paths_are_gone_and_machines_serves_their_operations() {
    let fixture = LegacyFixture::new();
    let _daemon = fixture.start_daemon();
    let acquired = fixture.request(
        "POST",
        "/acquire",
        Some(&json!({"purpose": "renamed", "image": "ubuntu2404", "env": "none"})),
    );
    assert_eq!(acquired.status, 200, "{}", acquired.body);
    let vm = acquired.body["vm"].as_str().expect("vm").to_string();
    let ssh_before = fixture.ssh_calls();
    let state_before = fixture.read_state();

    let cases = [
        ("GET", "/vms".to_string(), "/machines".to_string()),
        ("GET", format!("/vms/{vm}"), format!("/machines/{vm}")),
        ("GET", "/vms?x=1".to_string(), "/machines".to_string()),
        (
            "POST",
            format!("/vms/{vm}/exec"),
            format!("/machines/{vm}/exec"),
        ),
        (
            "POST",
            format!("/vms/{vm}/heartbeat"),
            format!("/machines/{vm}/heartbeat"),
        ),
        (
            "POST",
            format!("/vms/{vm}/release"),
            format!("/machines/{vm}/release"),
        ),
    ];
    for (method, path, replacement) in &cases {
        let body = (*method == "POST").then(|| json!({"argv": ["touch", "/tmp/ran"]}));
        let response = fixture.request(method, path, body.as_ref());
        assert_eq!(response.status, 410, "{method} {path}: {}", response.body);
        let error = response.body["error"].as_str().expect("error");
        assert!(
            error.contains(replacement.as_str()),
            "{method} {path}: {error}"
        );
        assert_eq!(response.body["replacement"], json!(replacement));
    }

    // No request on an old path reached the guest or changed the lease.
    assert_eq!(fixture.ssh_calls(), ssh_before);
    assert_eq!(fixture.read_state(), state_before);

    let list = fixture.request("GET", "/machines", None);
    assert_eq!(list.status, 200);
    assert_eq!(list.body["machines"][&vm]["state"], "running");
    assert!(list.body.get("vms").is_none());
    let one = fixture.request("GET", &format!("/machines/{vm}"), None);
    assert_eq!(one.status, 200);
    assert_eq!(one.body["vm"], json!(vm));

    let output = run_machctl(&["list", "--local"], &fixture.machctl_env());
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains(&vm));

    let released = fixture.request("POST", &format!("/machines/{vm}/release"), Some(&json!({})));
    assert_eq!(released.status, 200, "{}", released.body);
}

/// `/health` names the renamed service.
#[test]
fn health_names_machine_service() {
    let fixture = LegacyFixture::new();
    let _daemon = fixture.start_daemon();
    let health = fixture.request("GET", "/health", None);
    assert_eq!(health.body["ok"], json!(true));
    assert_eq!(health.body["service"], json!("machine-service"));
    let root = fixture.request("GET", "/", None);
    assert_eq!(root.body["service"], "machine-service");
    assert_eq!(root.body["endpoints"][3], "/machines");
}
