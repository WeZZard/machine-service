//! The real `machctl` against fake instances and a fake Consul agent
//! (TL-2; `docs/arch/federation.md` FD-2, FD-3; `docs/ux/cli/list.md` CL-1,
//! CL-3; AC-34).
//!
//! Every server here is an in-process HTTP fake on loopback. `CONSUL_HTTP_ADDR`
//! always points at a fake or at a closed port, so no test reads the real
//! Consul agent, and `MACHINE_SERVICE_PORT` always points at a fake, so no
//! test reaches an installed instance. The CLI is an isolated test child
//! whose DNS-SD is a folder of the test's own, empty unless a test
//! advertises in it, so no test browses the network (DC-1xx-ISO-5).

use std::collections::HashMap;
use std::net::TcpListener;
use std::path::Path;
use std::process::Output;
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{json, Value};

const MACHCTL: &str = env!("CARGO_BIN_EXE_machctl");

/// An isolated `machctl` whose DNS-SD folder is `bonjour`.
fn machctl_in(bonjour: &Path) -> service_layout::test_build::harness::Isolated {
    let mut command = service_layout::test_build::harness::command(MACHCTL);
    command.env("MACHINE_SERVICE_TEST_BONJOUR", bonjour);
    command
}

/// An isolated `machctl` whose DNS-SD folder is empty.
fn machctl() -> service_layout::test_build::harness::Isolated {
    static EMPTY: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    machctl_in(EMPTY.get_or_init(|| tempfile::tempdir().unwrap()).path())
}

/// A fake HTTP server with fixed JSON answers per path, recording every
/// path it was asked for.
struct Fake {
    port: u16,
    seen: Arc<Mutex<Vec<String>>>,
}

impl Fake {
    fn new(routes: Vec<(&str, Value)>) -> Fake {
        Fake::listening("127.0.0.1:0", routes)
    }

    fn listening(address: &str, routes: Vec<(&str, Value)>) -> Fake {
        let server = tiny_http::Server::http(address).unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let routes: HashMap<String, String> = routes
            .into_iter()
            .map(|(path, body)| (path.to_string(), body.to_string()))
            .collect();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&seen);
        thread::spawn(move || {
            for request in server.incoming_requests() {
                record.lock().unwrap().push(request.url().to_string());
                let response = match routes.get(request.url()) {
                    Some(body) => tiny_http::Response::from_string(body.clone()),
                    None => tiny_http::Response::from_string("{\"error\": \"unknown VM\"}")
                        .with_status_code(404),
                };
                let _ = request.respond(response);
            }
        });
        Fake { port, seen }
    }

    fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn asked(&self, path: &str) -> bool {
        self.seen.lock().unwrap().iter().any(|seen| seen == path)
    }
}

fn closed_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

/// A registration that states its instance ID, `node`, as every current
/// description does (FD-1, DC-80).
fn registration(node: &str, port: u16, status: &str) -> Value {
    let mut row = unstated(node, port, status);
    row["Service"]["Meta"]["instance"] = json!(node);
    row
}

/// A registration whose description predates the `instance` metadata.
fn unstated(node: &str, port: u16, status: &str) -> Value {
    json!({
        "Node": {"Node": node, "Address": "127.0.0.1"},
        "Service": {"ID": "catalog-machine-service", "Service": "machine-service",
                    "Tags": ["machines", "agents"], "Address": "127.0.0.1", "Port": port,
                    "Meta": {"loopback_only": "false"}},
        "Checks": [
            {"ServiceID": "", "Status": "passing"},
            {"ServiceID": "catalog-machine-service", "Status": status}
        ]
    })
}

/// The AC-34 fixture: the local instance `mac-a`, a reachable `mac-b`, and
/// a stopped `mac-c`, all three listed by Consul.
struct Intranet {
    local: Fake,
    remote: Fake,
    consul: Fake,
    home: tempfile::TempDir,
}

impl Intranet {
    fn new() -> Intranet {
        let local = Fake::new(vec![
            (
                "/health",
                json!({"ok": true, "service": "machine-service", "instance": "mac-a"}),
            ),
            (
                "/machines",
                json!({"machines": {
                    "pilot-mac-local-1a2b": {"state": "running", "image": "macos26",
                                             "image_kind": "macos", "purpose": "local"},
                    "shared": {"state": "running"}
                }, "catalog": {"status": "current", "generation": 1}}),
            ),
            (
                "/machines/pilot-mac-local-1a2b",
                json!({"name": "pilot-mac-local-1a2b", "state": "running", "where": "local"}),
            ),
        ]);
        let remote = Fake::new(vec![
            (
                "/health",
                json!({"ok": true, "service": "machine-service", "instance": "mac-b"}),
            ),
            (
                "/machines",
                json!({"machines": {
                    "mac-b": {"kind": "durable", "state": "available",
                              "capabilities": {"gpu": {"metal3": true},
                                               "ci_runner": {"present": true, "busy": false}}},
                    "pilot-remote-3c4d": {"kind": "virtual", "state": "running"},
                    "shared": {"kind": "virtual", "state": "running"}
                }}),
            ),
            (
                "/machines/pilot-remote-3c4d",
                json!({"name": "pilot-remote-3c4d", "state": "running", "where": "remote"}),
            ),
            (
                "/machines/shared",
                json!({"name": "shared", "state": "running", "where": "remote"}),
            ),
        ]);
        let consul = Fake::new(vec![(
            "/v1/health/service/machine-service",
            json!([
                registration("mac-c", closed_port(), "critical"),
                registration("mac-a", local.port, "passing"),
                registration("mac-b", remote.port, "passing")
            ]),
        )]);
        Intranet {
            local,
            remote,
            consul,
            home: tempfile::tempdir().unwrap(),
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with_consul(args, &self.consul.base())
    }

    fn run_with_consul(&self, args: &[&str], consul: &str) -> Output {
        run_machctl(args, self.home.path(), self.local.port, consul)
    }
}

fn run_machctl(args: &[&str], home: &std::path::Path, local_port: u16, consul: &str) -> Output {
    machctl()
        .args(args)
        .env("HOME", home)
        .env("MACHINE_SERVICE_PORT", local_port.to_string())
        .env("CONSUL_HTTP_ADDR", consul)
        // These fixtures stand in for a Consul agent (DC-84).
        .env("MACHINE_SERVICE_DISCOVERY", "consul")
        .output()
        .unwrap()
}

/// The local instance named `instance` by its `/health`, offering one
/// image, and a Consul agent listing `rows` built for the instance's port.
fn this_mac(instance: &str, rows: impl Fn(u16) -> Vec<Value>) -> (Fake, Fake) {
    let local = Fake::new(vec![
        (
            "/health",
            json!({"ok": true, "service": "machine-service", "instance": instance}),
        ),
        (
            "/machines",
            json!({"machines": {}, "offers": [{"name": "macos26", "kind": "virtual"}]}),
        ),
    ]);
    let consul = Fake::new(vec![(
        "/v1/health/service/machine-service",
        Value::Array(rows(local.port)),
    )]);
    (local, consul)
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// AC-34: the local instance's machines first, then the reachable
/// instance's, the stopped instance as unreachable with its error and its
/// Consul health, and the local instance once although Consul lists it.
#[test]
fn ac34_list_merges_every_instance_and_keeps_the_stopped_one() {
    let intranet = Intranet::new();
    let output = intranet.run(&["list"]);
    let table = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "{table}{}", stderr(&output));

    let lines: Vec<&str> = table.lines().collect();
    assert!(lines[0].starts_with("INSTANCE"), "{table}");
    let instances: Vec<&str> = lines[1..]
        .iter()
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    assert_eq!(
        instances,
        ["mac-a", "mac-a", "mac-b", "mac-b", "mac-b", "mac-c"],
        "{table}"
    );
    assert!(
        lines[1].contains("pilot-mac-local-1a2b") && lines[1].contains("local"),
        "{table}"
    );
    // Inside an instance the durable machine comes first.
    assert!(
        lines[3].contains("mac-b") && lines[3].contains("durable"),
        "{table}"
    );
    assert!(
        lines[3].contains("metal3") && lines[3].ends_with("idle"),
        "{table}"
    );
    assert!(lines[6].contains("unreachable"), "{table}");
    assert!(
        lines[6].contains("connection refused (Consul: critical)"),
        "{table}"
    );
    // The local instance is listed once, under its own name.
    assert_eq!(table.matches("pilot-mac-local-1a2b").count(), 1, "{table}");
    assert!(intranet.remote.asked("/machines"));
}

/// AC-34, CL-1: the JSON form is one object per instance, in the same
/// order, naming reachability, the error and the Consul health.
#[test]
fn ac34_list_json_is_one_object_per_instance() {
    let intranet = Intranet::new();
    let output = intranet.run(&["list", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let list: Value = serde_json::from_slice(&output.stdout).unwrap();
    let list = list.as_array().unwrap();
    let names: Vec<&str> = list
        .iter()
        .map(|i| i["instance"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["mac-a", "mac-b", "mac-c"]);
    assert_eq!(list[0]["local"], true);
    assert_eq!(list[0]["consul"], "passing");
    assert_eq!(list[0]["machines"].as_array().unwrap().len(), 2);
    assert_eq!(list[1]["reachable"], true);
    assert_eq!(list[1]["machines"][0]["name"], "mac-b");
    assert_eq!(list[2]["reachable"], false);
    assert_eq!(list[2]["error"], "connection refused");
    assert_eq!(list[2]["consul"], "critical");
    assert_eq!(list[2]["machines"], json!([]));
}

/// CL-1: `--local` never asks Consul; `--instance` lists one instance;
/// `--kind` and `--require` filter machines.
#[test]
fn list_filters() {
    let intranet = Intranet::new();
    let output = intranet.run(&["list", "--local"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(!stdout(&output).contains("mac-b"));
    assert!(!intranet.consul.asked("/v1/health/service/machine-service"));

    let output = intranet.run(&["list", "--instance", "mac-b"]);
    let table = stdout(&output);
    assert!(
        table.contains("pilot-remote-3c4d") && !table.contains("mac-a"),
        "{table}"
    );

    let output = intranet.run(&["list", "--kind", "durable", "--require", "metal3"]);
    let table = stdout(&output);
    let rows: Vec<&str> = table.lines().skip(1).collect();
    assert_eq!(rows.len(), 2, "{table}");
    assert!(
        rows[0].starts_with("mac-b") && rows[0].contains("durable"),
        "{table}"
    );
    assert!(
        rows[1].starts_with("mac-c") && rows[1].contains("unreachable"),
        "{table}"
    );

    let output = intranet.run(&["list", "--require", "metal"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("unknown capability"),
        "{}",
        stderr(&output)
    );
}

/// FD-2: without a Consul agent the local instance still lists, and the
/// missing registry is said on stderr, never shown as an empty intranet.
#[test]
fn list_without_consul_warns_and_lists_the_local_instance() {
    let intranet = Intranet::new();
    let consul = format!("127.0.0.1:{}", closed_port());
    let output = intranet.run_with_consul(&["list"], &consul);
    assert_eq!(output.status.code(), Some(0));
    assert!(stdout(&output).contains("pilot-mac-local-1a2b"));
    let errors = stderr(&output);
    assert!(
        errors.contains("Consul at http://127.0.0.1:") && errors.contains("connection refused"),
        "{errors}"
    );
}

/// FD-3: when no instance answers the table still names them, and the CLI
/// exits 2 with the start hint. The local instance that refuses is not
/// running, not unreachable (DC-93).
#[test]
fn list_with_no_instance_answering_exits_2() {
    let home = tempfile::tempdir().unwrap();
    let output = machctl()
        .args(["list"])
        .env("HOME", home.path())
        .env("MACHINE_SERVICE_PORT", closed_port().to_string())
        .env("CONSUL_HTTP_ADDR", format!("127.0.0.1:{}", closed_port()))
        .env("MACHINE_SERVICE_DISCOVERY", "consul")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stdout(&output).contains("not running  no machine-service on this Mac"),
        "{}",
        stdout(&output)
    );
    assert!(
        stderr(&output).contains("start it with"),
        "{}",
        stderr(&output)
    );
}

/// CL-3: a lease command goes to the instance that owns the machine, a
/// name two instances list must be qualified, and `<vm>@<instance>` names
/// the owner.
#[test]
fn cl3_lease_commands_go_to_the_owner() {
    let intranet = Intranet::new();

    let output = intranet.run(&["status", "pilot-remote-3c4d"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let record: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(record["where"], "remote");

    let output = intranet.run(&["status", "pilot-mac-local-1a2b"]);
    let record: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(record["where"], "local");

    let output = intranet.run(&["status", "shared"]);
    assert_eq!(output.status.code(), Some(1));
    let errors = stderr(&output);
    assert!(
        errors.contains("shared exists on mac-a and mac-b") && errors.contains("shared@<instance>"),
        "{errors}"
    );

    let output = intranet.run(&["status", "shared@mac-b"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let record: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(record["where"], "remote");

    // An unknown name stays with the local instance, whose answer says so.
    let output = intranet.run(&["status", "ghost"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("unknown VM"),
        "{}",
        stderr(&output)
    );
    assert!(intranet.local.asked("/machines/ghost"));

    // Push to another Mac sends the bytes to the owner's file endpoint
    // (FD-4); this fake has none, and the failure says so.
    let file = intranet.home.path().join("a.txt");
    std::fs::write(&file, "bytes").unwrap();
    let output = intranet.run(&[
        "push",
        "pilot-remote-3c4d",
        file.to_str().unwrap(),
        "/tmp/b",
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("push to pilot-remote-3c4d failed: HTTP 404"),
        "{}",
        stderr(&output)
    );
    assert!(intranet
        .remote
        .seen
        .lock()
        .unwrap()
        .iter()
        .any(|seen| seen.starts_with("/machines/pilot-remote-3c4d/files?path=/tmp/b")));
}

/// AC-47, DC-80: 192.0.2.10's Consul node is `catalog-node-a` and its
/// own name `Mac-A`. Its registration states `Mac-A`, so the list
/// shows it once, under its own name, with its Consul health, and says
/// that its legacy description needs a permanent identity.
#[test]
fn ac47_a_mac_whose_node_name_differs_is_listed_once_by_its_own_name() {
    let (local, consul) = this_mac("Mac-A", |port| {
        let mut row = unstated("catalog-node-a", port, "passing");
        row["Service"]["Meta"]["instance"] = json!("Mac-A");
        vec![row]
    });
    let home = tempfile::tempdir().unwrap();

    let output = run_machctl(&["list"], home.path(), local.port, &consul.base());
    let table = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "{table}{}", stderr(&output));
    let rows: Vec<&str> = table.lines().skip(1).collect();
    assert_eq!(rows.len(), 1, "{table}");
    assert!(
        rows[0].starts_with("Mac-A ") && rows[0].contains("macos26"),
        "{table}"
    );
    assert!(!table.contains("catalog-node-a"), "{table}");
    assert!(
        stderr(&output).contains("no machine_id"),
        "{}",
        stderr(&output)
    );

    let output = run_machctl(&["list", "--json"], home.path(), local.port, &consul.base());
    let list: Value = serde_json::from_slice(&output.stdout).unwrap();
    let list = list.as_array().unwrap();
    assert_eq!(list.len(), 1, "{list:?}");
    assert_eq!(list[0]["instance"], "Mac-A");
    assert_eq!(list[0]["local"], true);
    assert_eq!(list[0]["consul"], "passing");
}

/// AC-47, DC-80: a registration that states no instance ID falls back to
/// its node name, and the CLI says so on standard error. Where the node
/// name is the Mac's name it is still listed once; where it is not, the
/// fallback cannot tell, and the note is what explains the second row.
#[test]
fn ac47_a_registration_without_metadata_falls_back_to_the_node_name_and_says_so() {
    let home = tempfile::tempdir().unwrap();

    let (local, consul) = this_mac("mac-a", |port| vec![unstated("mac-a", port, "passing")]);
    let output = run_machctl(&["list", "--json"], home.path(), local.port, &consul.base());
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let list: Value = serde_json::from_slice(&output.stdout).unwrap();
    let names: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|instance| instance["instance"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["mac-a"]);
    assert_eq!(list[0]["consul"], "passing");
    let errors = stderr(&output);
    assert!(
        errors.contains("machctl: note: the registration on Consul node mac-a")
            && errors.contains("states no instance ID")
            && errors.contains("catalog-description"),
        "{errors}"
    );

    let (local, consul) = this_mac("Mac-A", |port| {
        vec![unstated("catalog-node-a", port, "passing")]
    });
    let output = run_machctl(&["list", "--json"], home.path(), local.port, &consul.base());
    let list: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 2, "{list:?}");
    assert!(
        stderr(&output).contains("Consul node catalog-node-a"),
        "{}",
        stderr(&output)
    );
}

/// An instance that advertises itself on this Mac only, through the Mac's
/// own DNS-SD service, so nothing is sent on any network (DC-85). The fake
/// answers `/health` with `answers` and offers one durable machine.
#[cfg(target_os = "macos")]
fn advertised(
    bonjour: &Path,
    advertise_as: &str,
    answers: &str,
) -> (discovery::dnssd::Advertisement, Fake) {
    let fake = Fake::new(vec![
        (
            "/health",
            json!({"ok": true, "service": "machine-service", "instance": answers, "kinds": ["durable"]}),
        ),
        (
            "/machines",
            json!({"machines": {answers: {"kind": "durable", "state": "available", "name": answers}}}),
        ),
    ]);
    let advertisement = discovery::dnssd::register(
        advertise_as,
        discovery::SERVICE_TYPE,
        fake.port,
        &discovery::txt_entries(advertise_as, "", &["durable"]),
        discovery::dnssd::Scope::Folder(bonjour.to_path_buf()),
        std::time::Duration::from_secs(5),
    )
    .expect("register an advertisement in the test's folder");
    (advertisement, fake)
}

/// `machctl list --json` with the discovery mode `mode` (`None` for the
/// default), no local instance, and a Consul agent that records what it was
/// asked.
#[cfg(target_os = "macos")]
fn list_json(bonjour: &Path, mode: Option<&str>, consul: &Fake) -> (Vec<Value>, String) {
    let home = tempfile::tempdir().unwrap();
    let mut command = machctl_in(bonjour);
    command
        .args(["list", "--json"])
        .env("HOME", home.path())
        .env("MACHINE_SERVICE_PORT", closed_port().to_string())
        .env("CONSUL_HTTP_ADDR", consul.base());
    if let Some(mode) = mode {
        command.env("MACHINE_SERVICE_DISCOVERY", mode);
    }
    let output = command.output().unwrap();
    let listed: Vec<Value> = serde_json::from_str(&stdout(&output)).unwrap_or_default();
    (listed, stderr(&output))
}

#[cfg(target_os = "macos")]
fn instance<'a>(listed: &'a [Value], name: &str) -> Option<&'a Value> {
    listed.iter().find(|instance| instance["instance"] == name)
}

/// AC-48: with the default mode and no Consul agent asked, `machctl list`
/// lists an instance that advertises itself, under its own ID, once its
/// `/health` confirms that ID.
#[cfg(target_os = "macos")]
#[test]
fn ac48_an_advertised_instance_is_listed_without_consul() {
    let name = format!("ac48-{}", std::process::id());
    let bonjour = tempfile::tempdir().unwrap();
    let (_advertisement, _fake) = advertised(bonjour.path(), &name, &name);
    let consul = Fake::new(vec![]);
    let (listed, errors) = list_json(bonjour.path(), None, &consul);
    let found =
        instance(&listed, &name).unwrap_or_else(|| panic!("{name} not in {listed:?}\n{errors}"));
    assert_eq!(found["found_by"], "bonjour", "{found}");
    assert_eq!(found["reachable"], true, "{found}");
    assert_eq!(found["machines"][0]["name"], name.as_str(), "{found}");
    assert!(
        !consul.asked("/v1/health/service/machine-service"),
        "Consul was asked"
    );
}

/// AC-48, FD-2: an advertisement whose `/health` answers another ID is
/// listed as unreachable with both IDs, never used.
#[cfg(target_os = "macos")]
#[test]
fn ac48_an_advertisement_whose_health_answers_another_id_is_not_used() {
    let name = format!("ac48-mismatch-{}", std::process::id());
    let bonjour = tempfile::tempdir().unwrap();
    let (_advertisement, _fake) = advertised(bonjour.path(), &name, "someone-else");
    let (listed, errors) = list_json(bonjour.path(), None, &Fake::new(vec![]));
    let found =
        instance(&listed, &name).unwrap_or_else(|| panic!("{name} not in {listed:?}\n{errors}"));
    assert_eq!(found["reachable"], false, "{found}");
    let error = found["error"].as_str().unwrap_or_default();
    assert!(
        error.contains(&format!("found as {name}")) && error.contains("someone-else"),
        "{error}"
    );
    assert_eq!(found["machines"], json!([]), "{found}");
}

/// AC-49: in Consul mode the CLI asks Consul and lists no advertised
/// instance that Consul does not list.
#[cfg(target_os = "macos")]
#[test]
fn ac49_consul_mode_asks_only_consul() {
    let name = format!("ac49-{}", std::process::id());
    let bonjour = tempfile::tempdir().unwrap();
    let (_advertisement, _fake) = advertised(bonjour.path(), &name, &name);
    let consul = Fake::new(vec![("/v1/health/service/machine-service", json!([]))]);
    let (listed, errors) = list_json(bonjour.path(), Some("consul"), &consul);
    assert!(instance(&listed, &name).is_none(), "{listed:?}\n{errors}");
    assert!(consul.asked("/v1/health/service/machine-service"));
}

/// DC-84: an unknown mode is refused with the two values.
#[test]
fn an_unknown_discovery_mode_is_refused() {
    let home = tempfile::tempdir().unwrap();
    let output = machctl()
        .args(["list"])
        .env("HOME", home.path())
        .env("MACHINE_SERVICE_PORT", closed_port().to_string())
        .env("MACHINE_SERVICE_DISCOVERY", "mdns")
        .output()
        .unwrap();
    assert_ne!(output.status.code(), Some(0));
    assert!(
        stderr(&output).contains("MACHINE_SERVICE_DISCOVERY must be one of bonjour, consul"),
        "{}",
        stderr(&output)
    );
}

/// MI-5/DC-90: permanent identity confirms a rename, and a mismatched
/// permanent identity remains unreachable even when the label agrees.
#[test]
fn mi5_machine_identity_confirms_renames_and_refuses_impostors() {
    let home = tempfile::tempdir().unwrap();
    for (advertised_id, reachable) in [("machine-a", true), ("machine-other", false)] {
        let remote = Fake::new(vec![
            (
                "/health",
                json!({"service":"machine-service", "instance":"Mac-B", "machine_id":"machine-a"}),
            ),
            (
                "/machines",
                json!({"machines":{"Mac-B":{"kind":"durable"}}}),
            ),
        ]);
        let mut row = registration("Mac-A", remote.port, "passing");
        row["Service"]["Meta"]["machine_id"] = json!(advertised_id);
        let consul = Fake::new(vec![("/v1/health/service/machine-service", json!([row]))]);
        let output = run_machctl(
            &["list", "--json"],
            home.path(),
            closed_port(),
            &consul.base(),
        );
        let listed: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
        let found = listed
            .iter()
            .find(|row| row["found_by"] == "consul")
            .unwrap();
        assert_eq!(found["reachable"], reachable, "{found}");
        if reachable {
            assert_eq!(found["instance"], "Mac-B");
            assert_eq!(found["machine_id"], "machine-a");
        } else {
            let error = found["error"].as_str().unwrap();
            assert!(
                error.contains("machine-a") && error.contains("machine-other"),
                "{error}"
            );
        }
    }
}

/// MI-1/MI-5: a label change cannot duplicate the local instance, and a
/// former label still selects its owner with an explanation.
#[test]
fn mi5_deduplicates_by_machine_and_port_and_resolves_former_names() {
    let home = tempfile::tempdir().unwrap();
    let local = Fake::new(vec![
        (
            "/health",
            json!({"service":"machine-service", "instance":"Mac-B", "machine_id":"machine-a", "former_names":[{"name":"Mac-A", "renamed_at":1}]}),
        ),
        (
            "/machines",
            json!({"machines":{"Mac-B":{"kind":"durable"},"pilot-test":{"kind":"virtual"}}}),
        ),
        ("/machines/pilot-test", json!({"name":"pilot-test"})),
    ]);
    let mut row = unstated("catalog-node", local.port, "passing");
    row["Service"]["Meta"]["machine_id"] = json!("machine-a");
    let consul = Fake::new(vec![("/v1/health/service/machine-service", json!([row]))]);
    let output = run_machctl(&["list", "--json"], home.path(), local.port, &consul.base());
    let listed: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0]["instance"], "Mac-B");
    assert!(!stderr(&output).contains("catalog-description"));
    let output = run_machctl(
        &["status", "pilot-test@Mac-A"],
        home.path(),
        local.port,
        &consul.base(),
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("Mac-A was renamed Mac-B"),
        "{}",
        stderr(&output)
    );
}

/// MI-5/DC-90: a saved durable lease follows permanent identity, but every
/// request keeps the name under which the lease was granted.
#[test]
fn mi5_saved_durable_lease_keeps_granted_name_after_a_rename() {
    for (requested_name, history) in [("Mac-B", true), ("Mac-A", false)] {
        let home = tempfile::tempdir().unwrap();
        let remote = Fake::new(vec![
            (
                "/health",
                json!({"service":"machine-service", "instance":"Mac-B", "machine_id":"machine-a", "former_names":if history {json!([{"name":"Mac-A", "renamed_at":1}])} else {json!([])}}),
            ),
            (
                "/machines",
                json!({"machines":{"Mac-B":{"kind":"durable","lease_id":"lease-a", "granted_name":"Mac-A"}}}),
            ),
            (
                "/machines/Mac-A/exec",
                json!({"rc":0,"output":"kept granted name"}),
            ),
        ]);
        let mut row = unstated("catalog-node", remote.port, "passing");
        row["Service"]["Meta"]["machine_id"] = json!("machine-a");
        let consul = Fake::new(vec![("/v1/health/service/machine-service", json!([row]))]);
        let state = home.path().join(".local/state/machine-service");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join("cli-leases.json"), json!({"schema_version":2,"leases":[{"machine":"Mac-A","instance":"http://192.0.2.1:6240","machine_id":"machine-a","lease_id":"lease-a","granted_name":"Mac-A"}]}).to_string()).unwrap();
        let output = run_machctl(
            &["exec", requested_name, "--", "true"],
            home.path(),
            closed_port(),
            &consul.base(),
        );
        assert!(output.status.success(), "{}", stderr(&output));
        assert!(remote.asked("/machines/Mac-A/exec"));
        assert!(stdout(&output).contains("kept granted name"));
    }
}

/// MI-4/MI-5: a legacy Consul registration remains usable, with a concrete
/// instruction to replace its name-based description.
#[test]
fn mi5_legacy_consul_description_requests_a_refresh() {
    let (local, consul) = this_mac("Mac-A", |port| vec![registration("Mac-A", port, "passing")]);
    let home = tempfile::tempdir().unwrap();
    let output = run_machctl(&["list"], home.path(), local.port, &consul.base());
    assert!(output.status.success());
    let note = stderr(&output);
    assert!(
        note.contains("no machine_id")
            && note.contains("reinstall")
            && note.contains("catalog-description"),
        "{note}"
    );
}

/// MI-5/DC-90: qualifying a command cannot bypass a failed confirmation.
#[test]
fn mi5_qualified_command_never_reaches_an_impostor() {
    let home = tempfile::tempdir().unwrap();
    let remote = Fake::new(vec![
        (
            "/health",
            json!({"service":"machine-service", "instance":"Mac-A", "machine_id":"other-machine"}),
        ),
        (
            "/machines/Mac-A/exec",
            json!({"rc":0,"output":"should not execute"}),
        ),
    ]);
    let mut row = registration("Mac-A", remote.port, "passing");
    row["Service"]["Meta"]["machine_id"] = json!("machine-a");
    let consul = Fake::new(vec![("/v1/health/service/machine-service", json!([row]))]);
    let output = run_machctl(
        &["exec", "Mac-A@Mac-A", "--lease-id", "lease-a", "--", "true"],
        home.path(),
        closed_port(),
        &consul.base(),
    );
    assert!(!output.status.success());
    assert!(!remote.asked("/machines/Mac-A/exec"));
    assert!(stderr(&output).contains("other-machine"));
}

/// MI-1/MI-5: reusing a former label cannot redirect or make an existing
/// lease ambiguous; its permanent key still identifies its owner.
#[test]
fn mi5_saved_identity_wins_when_another_machine_reuses_the_label() {
    let home = tempfile::tempdir().unwrap();
    let owner = Fake::new(vec![
        (
            "/health",
            json!({"service":"machine-service", "instance":"Mac-B", "machine_id":"machine-a", "former_names":[]}),
        ),
        (
            "/machines",
            json!({"machines":{"Mac-B":{"kind":"durable","lease_id":"lease-a","granted_name":"Mac-A"}}}),
        ),
        ("/machines/Mac-A/exec", json!({"rc":0,"output":"owner"})),
    ]);
    let other = Fake::new(vec![
        (
            "/health",
            json!({"service":"machine-service", "instance":"Mac-A", "machine_id":"machine-other"}),
        ),
        (
            "/machines",
            json!({"machines":{"Mac-A":{"kind":"durable","lease_id":"lease-other"}}}),
        ),
        (
            "/machines/Mac-A/exec",
            json!({"rc":0,"output":"wrong owner"}),
        ),
    ]);
    let mut row = unstated("catalog-owner", owner.port, "passing");
    row["Service"]["Meta"]["machine_id"] = json!("machine-a");
    let mut other_row = unstated("catalog-other", other.port, "passing");
    other_row["Service"]["Meta"]["machine_id"] = json!("machine-other");
    let consul = Fake::new(vec![(
        "/v1/health/service/machine-service",
        json!([row, other_row]),
    )]);
    let state = home.path().join(".local/state/machine-service");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join("cli-leases.json"), json!({"schema_version":2,"leases":[{"machine":"Mac-A","instance":"http://192.0.2.1:6240","machine_id":"machine-a","lease_id":"lease-a","granted_name":"Mac-A"}]}).to_string()).unwrap();
    for name in ["Mac-A", "Mac-A@Mac-A"] {
        let output = run_machctl(
            &["exec", name, "--", "true"],
            home.path(),
            closed_port(),
            &consul.base(),
        );
        assert!(output.status.success(), "{}", stderr(&output));
        assert_eq!(stdout(&output), "owner");
    }
    assert!(!other.asked("/machines/Mac-A/exec"));
}

/// MI-5/DC-90: a health reply without machine_id falls back to the known
/// label; a permanent-only registration cannot accept an unrelated label.
#[test]
fn mi5_missing_health_identity_uses_the_legacy_name_check() {
    let home = tempfile::tempdir().unwrap();
    let remote = Fake::new(vec![
        (
            "/health",
            json!({"service":"machine-service", "instance":"Mac-B"}),
        ),
        ("/machines", json!({"machines":{}})),
    ]);
    let mut row = unstated("Mac-A", remote.port, "passing");
    row["Service"]["Meta"]["machine_id"] = json!("machine-a");
    let consul = Fake::new(vec![("/v1/health/service/machine-service", json!([row]))]);
    let output = run_machctl(
        &["list", "--json"],
        home.path(),
        closed_port(),
        &consul.base(),
    );
    let listed: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
    let found = listed
        .iter()
        .find(|row| row["found_by"] == "consul")
        .unwrap();
    assert_eq!(found["reachable"], false, "{found}");
    assert!(!remote.asked("/machines"));
}
