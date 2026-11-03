//! Live-only ports of the two e2e cases in `tests/e2e/test_e2e.py`.
//!
//! These exercise the real production path: a running daemon, a real Tart
//! golden image, a real guest, real networking and real gateway
//! credentials. They are `#[ignore]`d so the normal workspace run never
//! boots, clones, or leases a VM.
//!
//! They run only against a second instance that `second-instance` started
//! (`scripts/live-instance.sh`), named by its root, and never against the
//! installed daemon (DC-1xx-LIVE-3). Their credentials are a pack fixture
//! the test owns (DC-1xx-LIVE-5): the files of `LIVE_E2E_PACK`, a folder the
//! operator prepares, copied into the instance's own pack store under a name
//! of the test's and removed after it; the operator's `default` pack is
//! never read, and a second instance cannot read it.
//!
//! ```sh
//! LIVE_PORT=6250 LIVE_BROKER_PORT=6251 LIVE_TART_HOME=<store> scripts/live-instance.sh start ssh
//! LIVE_ROOT=$PWD/target/live/6250 LIVE_E2E_PACK=<pack folder> LIVE_E2E_GH_LOGIN=<login> \
//!   cargo test -p machine-service --test live_e2e -- --ignored
//! ```
//!
//! The Python `E2ETestCase` base released every acquired VM in `tearDown`;
//! [`Lease`] reproduces that with `Drop`, so a failing assertion still releases.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use second_instance::Manifest;
use serde_json::{json, Value};

/// Default e2e image, matching the Python `E2E_IMAGE` default.
const E2E_IMAGE: &str = "ubuntu2404";
/// Default lease TTL, matching the Python `E2E_TTL_HOURS` default.
const E2E_TTL_HOURS: f64 = 1.0;
/// Python `E2E_ACQUIRE_TIMEOUT` default.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(900);
/// Python `test_pi_reaches_gateway` exec timeout.
const PI_EXEC_TIMEOUT: Duration = Duration::from_secs(600);
/// Python `test_gh_token_works` exec timeout.
const GH_EXEC_TIMEOUT: Duration = Duration::from_secs(120);
/// Python teardown release timeout.
const RELEASE_TIMEOUT: Duration = Duration::from_secs(600);

fn need(name: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| panic!("set {name}; see the module documentation"))
}

/// The second instance under `LIVE_ROOT`, refused unless it reports itself
/// as one: its manifest and port.
fn instance() -> Manifest {
    let root = PathBuf::from(need("LIVE_ROOT"));
    let manifest = Manifest::read(&root)
        .expect("read the manifest")
        .unwrap_or_else(|| panic!("no second instance under {}", root.display()));
    let health = common::try_http(manifest.port, "GET", "/health", None, None)
        .unwrap_or_else(|e| panic!("no daemon on port {}: {e:?}", manifest.port));
    if let Err(refused) = second_instance::guard(manifest.port, &health.body) {
        panic!("{refused}");
    }
    manifest
}

/// The test's own credential pack in the instance's pack store, removed on
/// drop.
struct Pack {
    dir: PathBuf,
    name: String,
}

impl Pack {
    fn create(manifest: &Manifest, test: &str) -> Pack {
        use std::os::unix::fs::PermissionsExt;
        let source = PathBuf::from(need("LIVE_E2E_PACK"));
        let name = format!("live-e2e-{test}-{}", std::process::id());
        let dir = manifest.packs().join(&name);
        std::fs::create_dir_all(&dir).expect("pack folder");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let pack = Pack { dir, name };
        for entry in std::fs::read_dir(&source).expect("LIVE_E2E_PACK") {
            let path = entry.expect("pack entry").path();
            if path.is_file() {
                let target = pack.dir.join(path.file_name().unwrap());
                std::fs::copy(&path, &target).expect("copy a pack file");
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
            }
        }
        pack
    }

    fn in_store(&self, store: &Path) -> bool {
        self.dir.starts_with(store)
    }
}

impl Drop for Pack {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A leased VM that is released on drop, mirroring the Python `tearDown`.
struct Lease {
    port: u16,
    vm: String,
}

impl Drop for Lease {
    fn drop(&mut self) {
        let _ = common::try_http_timeout(
            self.port,
            "POST",
            &format!("/machines/{}/release", self.vm),
            Some(&json!({"reason": "e2e-teardown"})),
            None,
            RELEASE_TIMEOUT,
        );
    }
}

/// Python `E2ETestCase.acquire`.
fn acquire(port: u16, purpose: &str, env: &str) -> (Value, Lease) {
    let response = common::http_timeout(
        port,
        "POST",
        "/acquire",
        Some(&json!({
            "purpose": purpose,
            "image": E2E_IMAGE,
            "env": env,
            "ttl_hours": E2E_TTL_HOURS,
        })),
        None,
        ACQUIRE_TIMEOUT,
    );
    assert_eq!(response.status, 200, "acquire failed: {:?}", response.body);
    let vm = response
        .body
        .get("vm")
        .and_then(Value::as_str)
        .expect("acquired VM name")
        .to_string();
    let lease = Lease {
        port,
        vm: vm.clone(),
    };
    (response.body, lease)
}

/// Python `api("POST", f"/machines/{rec['vm']}/exec", body, timeout=...)`.
fn exec(port: u16, vm: &str, body: &Value, timeout: Duration) -> common::Response {
    common::http_timeout(
        port,
        "POST",
        &format!("/machines/{vm}/exec"),
        Some(body),
        None,
        timeout,
    )
}

/// `test_pi_reaches_gateway`
///
/// US3: `pi` in the guest reaches the gateway with the test's credential
/// pack and answers a trivial prompt.
#[test]
#[ignore = "test_pi_reaches_gateway: requires a second instance under LIVE_ROOT, a real Tart hypervisor with a bootable ubuntu2404 golden image, guest network access, and pi gateway credentials in the LIVE_E2E_PACK fixture"]
fn test_pi_reaches_gateway() {
    let manifest = instance();
    let port = manifest.port;
    let pack = Pack::create(&manifest, "pi");
    assert!(pack.in_store(&manifest.packs()));
    let (record, _lease) = acquire(port, "e2e-pi", &pack.name);
    let vm = record["vm"].as_str().expect("acquired VM name");
    let response = exec(
        port,
        vm,
        &json!({
            "script": "exec bash -lc \"pi --model glm-5.3-flash 'Reply with exactly: ok'\""
        }),
        PI_EXEC_TIMEOUT,
    );
    assert_eq!(response.status, 200, "pi exec failed: {:?}", response.body);
    assert_eq!(response.body["rc"], 0);
    assert!(response.body["output"]
        .as_str()
        .unwrap_or("")
        .to_lowercase()
        .contains("ok"));
}

/// `test_gh_token_works`
///
/// US3: the test pack's GitHub token authenticates `gh` in the guest as
/// `LIVE_E2E_GH_LOGIN`; a login shell is used because the pack env vars land
/// via `.bashrc`/`.profile`.
#[test]
#[ignore = "test_gh_token_works: requires a second instance under LIVE_ROOT, a real Tart hypervisor with a bootable ubuntu2404 golden image, guest network access to github.com, and a GitHub token in the LIVE_E2E_PACK fixture whose login is LIVE_E2E_GH_LOGIN"]
fn test_gh_token_works() {
    let manifest = instance();
    let port = manifest.port;
    let login = need("LIVE_E2E_GH_LOGIN");
    let pack = Pack::create(&manifest, "gh");
    let (record, _lease) = acquire(port, "e2e-gh", &pack.name);
    let vm = record["vm"].as_str().expect("acquired VM name");
    let response = exec(
        port,
        vm,
        &json!({"argv": ["bash", "-lc", "gh api user --jq .login"]}),
        GH_EXEC_TIMEOUT,
    );
    assert_eq!(response.status, 200);
    assert_eq!(response.body["rc"], 0);
    assert_eq!(response.body["output"].as_str().unwrap_or("").trim(), login);
}
