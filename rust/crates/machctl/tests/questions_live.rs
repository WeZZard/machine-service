//! The live regression of FD-7: catalog questions from a Mac whose own
//! instance offers nothing (`docs/arch/federation.md` FD-7, CL-4,
//! DC-1xx-FED-6).
//!
//! It runs only on request, on a Mac on the lab's network:
//!
//! ```sh
//! MACHINE_SERVICE_TEST_LIVE=TL-6 cargo test -p machctl --test questions_live -- --ignored
//! ```
//!
//! It is read-only. It never acquires, reloads or changes anything: it
//! sends `GET /health`, `GET /machines`, `GET /images` and
//! `GET /acquisition-capabilities` to the installed instances that Bonjour
//! finds at run time, and it names none of them. The local instance is a
//! closed loopback port, which stands for a Mac that offers nothing, because
//! a test build never uses the installed port of its own Mac (DC-251).
//!
//! Before FD-7 it failed: `machctl images` asked only the local instance,
//! found nothing there and exited 2, while an instance on the network listed
//! its images.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::process::Output;
use std::time::Duration;

use serde_json::Value;

const MACHCTL: &str = env!("CARGO_BIN_EXE_machctl");

fn closed_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

/// A live `machctl` whose local instance is a closed port and whose state
/// is the test's own.
fn machctl(args: &[&str], home: &std::path::Path, local: u16) -> Output {
    service_layout::test_build::harness::command(MACHCTL)
        .args(args)
        .env("HOME", home)
        .env("MACHINE_SERVICE_STATE", home.join("state"))
        .env("MACHINE_SERVICE_PORT", local.to_string())
        .env(
            service_layout::test_build::LIVE_VARIABLE,
            service_layout::test_build::LIVE_VALUE,
        )
        .output()
        .unwrap()
}

/// One read-only `GET` of a JSON document, outside machctl, so the test
/// knows what each instance holds before it asks machctl.
fn get(base: &str, path: &str) -> Option<Value> {
    let authority = base.strip_prefix("http://")?.trim_end_matches('/');
    let address = authority.to_socket_addrs().ok()?.next()?;
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(5)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(40)))
        .ok()?;
    write!(
        stream,
        "GET {path} HTTP/1.0\r\nHost: {authority}\r\nAccept: application/json\r\n\r\n"
    )
    .ok()?;
    let mut text = String::new();
    stream.read_to_string(&mut text).ok()?;
    let (head, body) = text.split_once("\r\n\r\n")?;
    head.starts_with("HTTP/1.").then_some(())?;
    serde_json::from_str(body).ok()
}

fn image_names(answer: &Value) -> Vec<String> {
    answer
        .get("images")
        .and_then(Value::as_object)
        .map(|images| images.keys().cloned().collect())
        .unwrap_or_default()
}

/// FD-7: every image that a discovered instance lists shows under that
/// instance in `machctl images --json`, and every such instance answers
/// `machctl acquisition-capabilities`, although the local instance offers
/// nothing.
#[test]
#[ignore = "live: reads the installed instances on the network; run with MACHINE_SERVICE_TEST_LIVE=TL-6 and --ignored"]
fn fd7_live_questions_reach_the_instances_that_offer_images() {
    assert!(
        service_layout::test_build::is_live(),
        "set {}={} to run the live layer",
        service_layout::test_build::LIVE_VARIABLE,
        service_layout::test_build::LIVE_VALUE
    );
    let home = tempfile::tempdir().unwrap();
    let local = closed_port();

    // The instances on the network, as `machctl list` finds them.
    let listed = machctl(&["list", "--json"], home.path(), local);
    let listed: Vec<Value> = serde_json::from_slice(&listed.stdout)
        .unwrap_or_else(|error| panic!("{error}: {}", String::from_utf8_lossy(&listed.stderr)));
    let hosts: Vec<(String, Vec<String>)> = listed
        .iter()
        .filter(|instance| instance["reachable"] == true && instance["local"] == false)
        .filter_map(|instance| {
            let machine_id = instance["machine_id"].as_str()?.to_string();
            let images = image_names(&get(instance["address"].as_str()?, "/images")?);
            (!images.is_empty()).then_some((machine_id, images))
        })
        .collect();
    assert!(
        !hosts.is_empty(),
        "no instance on the network lists an image, so there is nothing to prove: {listed:?}"
    );

    let output = machctl(&["images", "--json"], home.path(), local);
    assert_eq!(
        output.status.code(),
        Some(0),
        "machctl images: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let answers: Vec<Value> = serde_json::from_slice(&output.stdout).expect("images json");
    for (machine_id, images) in &hosts {
        let answer = answers
            .iter()
            .find(|entry| entry["machine_id"] == machine_id.as_str())
            .unwrap_or_else(|| panic!("instance {machine_id} is missing from {answers:?}"));
        assert_eq!(answer["reachable"], true, "{answer}");
        let shown = image_names(&answer["answer"]);
        for image in images {
            assert!(
                shown.contains(image),
                "{image} of {machine_id} not in {shown:?}"
            );
        }
    }

    let output = machctl(&["acquisition-capabilities"], home.path(), local);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let answers: Vec<Value> = serde_json::from_slice(&output.stdout).expect("capabilities json");
    for (machine_id, _) in &hosts {
        assert!(
            answers
                .iter()
                .any(|entry| entry["machine_id"] == machine_id.as_str() && entry["status"] == 200),
            "instance {machine_id} has no acquisition capabilities in {answers:?}"
        );
    }
}
