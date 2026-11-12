//! Work-image clone admission, ported from `tests/unit/test_work_source.py`.
//!
//! The Python fixture reached the Tart/SSH boundary with `mock.patch.object`
//! on the daemon module. This port substitutes the in-memory [`common::FakeHost`]
//! behind the `machine_service_core::host::Host` trait.
//!
//! `common::FakeHost` exposes its image map as a plain field, so a service
//! built from a [`common::Fixture`] cannot have its `WORK_VM` changed after
//! construction (the `Arc` is shared with the service). This file wraps the
//! fake in a small local [`TestHost`] decorator that owns the line map and the
//! clone hook the Python suite installed with `mock.patch.object(svc, "tart")`.
//! `tests/common/mod.rs` is not modified.
//!
//! Run with:
//! `cd rust && CARGO_TARGET_DIR=$PWD/target-host cargo test -p machine-service-core --test work_source`

mod common;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{json, Map, Value};

use machine_service_core::catalog::CatalogSource;
use machine_service_core::error::OpResult;
use machine_service_core::host::{BootProcess, Host, TartOutput};
use machine_service_core::service::Service;

/// The configured work VM used by the Python suite.
const WORK: &str = "pilot-ubuntu-work";

// ------------------------------------------------------------------- TestHost

/// A `Host` decorator over [`common::FakeHost`] with an optional one-shot
/// hook after `tart clone`.
///
/// Every non-Tart method delegates to the fake, the image catalog included:
/// a line change goes through the fake's `lines_override`, which the catalog
/// sees as a changed input on its next read.
struct TestHost {
    inner: Arc<common::FakeHost>,
    after_clone: Mutex<Option<Box<dyn FnOnce() + Send + Sync>>>,
}

impl TestHost {
    fn new(inner: Arc<common::FakeHost>) -> Self {
        Self {
            inner,
            after_clone: Mutex::new(None),
        }
    }

    /// Set `ubuntu2404`'s `work_vm`, mirroring `lines['ubuntu2404']['work_vm'] = work`.
    fn set_work(&self, work: Value) {
        let mut slot = self.inner.lines_override.lock().expect("lines_override");
        let lines = slot.get_or_insert_with(common::default_lines);
        lines
            .get_mut("ubuntu2404")
            .expect("ubuntu2404 line")
            .as_object_mut()
            .expect("line is an object")
            .insert("work_vm".to_string(), work);
    }

    /// Install a one-shot side effect run after the next `tart clone` returns.
    fn set_after_clone(&self, hook: Box<dyn FnOnce() + Send + Sync>) {
        *self.after_clone.lock().expect("after_clone") = Some(hook);
    }
}

impl Host for TestHost {
    fn tart(&self, args: &[String], check: bool, timeout_s: u64) -> OpResult<TartOutput> {
        let result = self.inner.tart(args, check, timeout_s);
        if args.first().map(String::as_str) == Some("clone") {
            if let Some(mutate) = self.after_clone.lock().expect("after_clone").take() {
                mutate();
            }
        }
        result
    }

    fn ssh(
        &self,
        ip: &str,
        user: &str,
        key_dir: &Path,
        remote_cmd: &str,
        stdin: Option<Vec<u8>>,
        timeout_s: u64,
    ) -> Option<(i32, String)> {
        self.inner
            .ssh(ip, user, key_dir, remote_cmd, stdin, timeout_s)
    }

    fn scp(
        &self,
        ip: &str,
        user: &str,
        key_dir: &Path,
        src: &str,
        dst: &str,
        timeout_s: u64,
    ) -> Option<(i32, String)> {
        self.inner.scp(ip, user, key_dir, src, dst, timeout_s)
    }

    fn wait_ip(&self, name: &str, timeout_s: u64) -> Option<String> {
        self.inner.wait_ip(name, timeout_s)
    }

    fn wait_ssh(&self, ip: &str, user: &str, key_dir: &Path, timeout_s: u64) -> bool {
        self.inner.wait_ssh(ip, user, key_dir, timeout_s)
    }

    fn verify_transfer(&self, ip: &str, user: &str, key_dir: &Path) -> OpResult<()> {
        self.inner.verify_transfer(ip, user, key_dir)
    }

    fn bootstrap(
        &self,
        ip: &str,
        user: &str,
        password: &str,
        key_dir: &Path,
        timeout_s: f64,
    ) -> OpResult<()> {
        self.inner.bootstrap(ip, user, password, key_dir, timeout_s)
    }

    fn spawn_run(
        &self,
        vm: &str,
        network: machine_service_core::acquisition_options::GuestNetwork,
    ) -> OpResult<Box<dyn BootProcess>> {
        self.inner.spawn_run(vm, network)
    }

    fn softnet(&self) -> Result<PathBuf, String> {
        self.inner.softnet()
    }

    fn guest_firewall(&self) -> Result<machine_service_core::isolation::GuestFirewall, String> {
        self.inner.guest_firewall()
    }

    fn firewall_table(&self) -> PathBuf {
        self.inner.firewall_table()
    }

    fn dot_forwarder(&self) -> OpResult<Vec<u8>> {
        self.inner.dot_forwarder()
    }

    fn open_probe(&self) -> OpResult<Box<dyn machine_service_core::host::ProbeListener>> {
        self.inner.open_probe()
    }

    fn boot_settle_s(&self) -> f64 {
        self.inner.boot_settle_s()
    }

    fn host_macos_guests(&self) -> Option<usize> {
        self.inner.host_macos_guests()
    }

    fn catalog_source(&self) -> Option<Arc<dyn CatalogSource>> {
        self.inner.catalog_source()
    }

    fn home_dir(&self) -> Option<PathBuf> {
        self.inner.home_dir()
    }

    fn tart_store_root(&self) -> PathBuf {
        self.inner.tart_store_root()
    }
}

// ---------------------------------------------------------------- WorkService

/// One isolated service whose `ubuntu2404` line has a configured work VM, with
/// a real on-disk Tart metadata directory and the fake Tart/SSH behind it.
struct WorkService {
    fixture: common::Fixture,
    host: Arc<TestHost>,
    service: Arc<Service>,
    root: PathBuf,
    fingerprint: Value,
}

impl WorkService {
    /// Mirror of the Python `setUp`: empty state, a configured work VM, and real
    /// Tart metadata whose fingerprint is computed from disk.
    fn new() -> Self {
        let fixture = common::Fixture::new();
        fixture.write_state(json!({}));
        let host = Arc::new(TestHost::new(Arc::clone(&fixture.host)));
        let tart_home = fixture
            .dir
            .path()
            .canonicalize()
            .expect("canonicalize")
            .join("tart");
        // `stopped_work_fingerprint` reads the host's Tart store; point it at
        // the real fixture tree instead of patching `TART_HOME`.
        *host.inner.tart_store.lock().expect("tart_store") = tart_home.clone();
        host.inner
            .tart
            .vms
            .lock()
            .expect("vms")
            .insert(WORK.to_string(), false);
        host.set_work(json!(WORK));
        let root = tart_home.join("vms").join(WORK);
        std::fs::create_dir_all(&root).expect("work root");
        for name in ["config.json", "disk.img", "nvram.bin"] {
            std::fs::write(root.join(name), b"fixture-stopped-work").expect("work file");
        }
        let fingerprint = machine_service_core::application_catalog::fingerprint_base(
            &tart_home.join("vms"),
            WORK,
            "linux",
        )
        .expect("work fingerprint");
        let service = Arc::new(Service::with_host(
            fixture.service.config.clone(),
            None,
            Arc::clone(&host) as Arc<dyn Host>,
        ));
        service
            .initialize_image_catalog()
            .expect("in-memory image catalog");
        Self {
            fixture,
            host,
            service,
            root,
            fingerprint,
        }
    }

    /// Python's `acquire()` helper: purpose `work-acceptance`, `ubuntu2404`,
    /// `env=none`, `source=work`.
    fn acquire(&self, expected: Option<&Value>) -> OpResult<Value> {
        self.service.acquire(
            "work-acceptance",
            "ubuntu2404",
            "none",
            &json!(24),
            None,
            None,
            None,
            true,
            "nat",
            None,
            "work",
            expected,
            false,
            false,
        )
    }

    fn read_state(&self) -> Map<String, Value> {
        self.fixture.read_state()
    }
}

/// The Python `setUp` empty-state fixture for cases that do not configure a
/// work VM.
fn empty_fixture() -> common::Fixture {
    let fixture = common::Fixture::new();
    fixture.write_state(json!({}));
    fixture
}

// --------------------------------------------------------------------- tests

#[test]
fn clones_configured_work_and_retains_source_identity() {
    let env = WorkService::new();
    let lease = env.acquire(Some(&env.fingerprint)).expect("acquire");
    assert_eq!(lease["source"], "work");
    assert_eq!(lease["source_vm"], WORK);
    assert_eq!(lease["source_fingerprint"], env.fingerprint);
    assert!(lease["env"].is_null());
    let vm = lease["vm"].as_str().expect("vm").to_string();
    assert!(
        env.fixture.host.tart.ops().iter().any(|op| op == "clone"),
        "clone recorded"
    );
    assert!(env
        .fixture
        .host
        .tart
        .args_for("clone")
        .iter()
        .any(|args| args == &vec![WORK.to_string(), vm.clone()]));
    assert_eq!(
        env.fixture.host.tart.vms.lock().expect("vms").get(WORK),
        Some(&false)
    );
    env.service
        .release(&vm, "released", false)
        .expect("release");
    assert!(env
        .fixture
        .host
        .tart
        .vms
        .lock()
        .expect("vms")
        .contains_key(WORK));
}

#[test]
fn rejects_running_work_before_reservation() {
    let env = WorkService::new();
    env.fixture
        .host
        .tart
        .vms
        .lock()
        .expect("vms")
        .insert(WORK.to_string(), true);
    let error = env.acquire(None).unwrap_err();
    assert!(error.to_string().contains("stopped"), "{error}");
    assert!(env.read_state().is_empty());
    assert!(!env.fixture.host.tart.ops().iter().any(|op| op == "clone"));
}

#[test]
fn rejects_absent_or_invalid_configured_source() {
    let env = WorkService::new();
    for work in [
        Value::Null,
        json!("../outside"),
        json!("/absolute"),
        json!("pilot-ubuntu-base"),
    ] {
        env.host.set_work(work.clone());
        let error = env.acquire(None).unwrap_err();
        assert!(
            error.to_string().contains("WORK_VM"),
            "work_vm={work}: {error}"
        );
    }
    assert!(env.read_state().is_empty());
}

#[test]
fn rejects_credentials_and_arbitrary_source() {
    let fixture = empty_fixture();
    for (source, env) in [("work", "default"), ("other-vm", "none")] {
        let result = fixture.service.acquire(
            "rejected",
            "ubuntu2404",
            env,
            &json!(24),
            None,
            None,
            None,
            true,
            "nat",
            None,
            source,
            None,
            false,
            false,
        );
        assert!(result.is_err(), "source={source} env={env}");
    }
    assert!(fixture.read_state().is_empty());
}

#[test]
fn rejects_expected_fingerprint_mismatch_before_clone() {
    let env = WorkService::new();
    let error = env.acquire(Some(&json!({"wrong": true}))).unwrap_err();
    assert!(error.to_string().contains("expected"), "{error}");
    assert!(env.read_state().is_empty());
    assert!(!env.fixture.host.tart.ops().iter().any(|op| op == "clone"));
}

#[test]
fn changed_source_during_clone_cleans_only_new_lease() {
    let env = WorkService::new();
    let root = env.root.clone();
    env.host.set_after_clone(Box::new(move || {
        std::fs::write(root.join("disk.img"), b"changed-image-by-another-process")
            .expect("mutate disk");
    }));
    let error = env.acquire(None).unwrap_err();
    assert!(
        error.to_string().contains("changed during clone"),
        "{error}"
    );
    assert!(env.read_state().is_empty());
    let mut names: Vec<String> = env
        .fixture
        .host
        .tart
        .vms
        .lock()
        .expect("vms")
        .keys()
        .cloned()
        .collect();
    names.sort();
    assert_eq!(names, vec![WORK.to_string()]);
}

#[test]
fn default_base_source_remains_unchanged() {
    let fixture = empty_fixture();
    let lease = fixture.acquire("base-test", "ubuntu2404", "none");
    let vm = lease["vm"].as_str().expect("vm").to_string();
    assert!(fixture
        .host
        .tart
        .args_for("clone")
        .iter()
        .any(|args| args == &vec!["pilot-ubuntu-base".to_string(), vm.clone()]));
    assert!(lease.get("source_fingerprint").is_none());
    fixture.release(&vm);
}

#[test]
fn request_validation_requires_explicit_credential_free_work() {
    let env = WorkService::new();
    machine_service_core::control_only::validate_request(&json!({
        "purpose": "acceptance",
        "source": "work",
        "env": "none",
        "expected_source_fingerprint": env.fingerprint,
    }))
    .expect("valid credential-free work request");
    for payload in [
        json!({"source": "work"}),
        json!({"source": "work", "env": "default"}),
        json!({"source": "base", "expected_source_fingerprint": {}}),
        json!({"source": "work", "env": "none", "expected_source_fingerprint": "bad"}),
    ] {
        assert!(
            machine_service_core::control_only::validate_request(&payload).is_err(),
            "payload should be rejected: {payload}"
        );
    }
}
