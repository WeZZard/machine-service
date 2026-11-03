//! Ported from `tests/unit/test_key_lifecycle.py` and
//! `tests/unit/test_gc_renewal.py`.
//!
//! Offline lease lifecycle regressions: real keys/state, fake guest transport.
//! The Python suite patched module-level helpers with `mock.patch.object`. The
//! Rust port substitutes a [`common::FakeHost`] and drives the same ordering
//! through its hooks.
//!
//! The Python `KeyLifecycleFixture` exercises the *real* `verify_transfer` with
//! an in-memory byte-copy `scp`, so the port installs a `transfer_hook` that
//! reimplements that sequence over the fake `scp`/`ssh` hooks.
//!
//! Forced deviations from the Python source, each because the behavior is
//! white-box and has no `Host` seam:
//!
//! * `mock.patch.object(lease_keys, "create")` — no create hook. "Keys were
//!   generated before bootstrap" is asserted where it is observable: the clone
//!   `tart` call (still `pending`, keys present) and the bootstrap hook.
//! * `mock.patch.object(STATE, "_store")` — the store-failure hook now exists
//!   (`state::set_store_failure`, `debug_assertions`-gated) and is exercised by
//!   the stock-console failure tests; here the single "running implies
//!   ssh_verified" transition is still asserted on the final record.
//! * `mock.patch.object(svc, "line_cfg")` — replaced by counting the image
//!   catalog's identity reads on the test host's in-memory catalog source.
//! * `mock.patch.object(svc, "operation_lock")` in the two concurrency tests —
//!   the real lock is not observable; a bounded wait substitutes for the
//!   Python "release reached the per-VM lock" event.
//! * `mock.patch.object(lease_keys, "cleanup")` — no cleanup hook. The
//!   "keys erased only after VM absence" ordering is asserted from a `tart`
//!   observer at the post-delete moment.
//! * `TestDaemonOwnership` drives the daemon's `main()` in `bin/vm-service`,
//!   which lives in the `machine-service` binary crate, not `machine-service-core`; it is
//!   kept as an `#[ignore]` with the reason below.

mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use machine_service_core::catalog::CatalogSource;
use machine_service_core::config::unix_now;
use machine_service_core::error::{OpError, OpResult};
use machine_service_core::host::{BootProcess, Host, TartOutput};
use machine_service_core::service::Service;
use machine_service_core::ssh::shell_quote;

// ---------------------------------------------------------------------------
// Test host: a `Host` wrapper with the failure knobs the Python suite reached
// through `mock.patch.object`.
// ---------------------------------------------------------------------------

/// A post-`tart` observer: `(operation, full argument vector)`.
type TartObserver = Arc<dyn Fn(&str, &[String]) + Send + Sync>;

/// A `Host` that delegates to the in-memory `FakeHost` and adds injectable
/// `bootstrap` failure, a no-op `delete`, and a post-`tart` observer. Its
/// image catalog is the inner fake's.
struct TestHost {
    inner: Arc<common::FakeHost>,
    shared: Arc<Mutex<Shared>>,
    bootstrap_error: Mutex<Option<String>>,
    no_op_delete: AtomicBool,
    tart_observer: Mutex<Option<TartObserver>>,
}

impl TestHost {
    fn new(inner: Arc<common::FakeHost>, shared: Arc<Mutex<Shared>>) -> Self {
        Self {
            inner,
            shared,
            bootstrap_error: Mutex::new(None),
            no_op_delete: AtomicBool::new(false),
            tart_observer: Mutex::new(None),
        }
    }
}

impl Host for TestHost {
    fn tart(&self, args: &[String], check: bool, timeout_s: u64) -> OpResult<TartOutput> {
        let op = args.first().cloned().unwrap_or_default();
        if op == "delete" && self.no_op_delete.load(Ordering::SeqCst) {
            // Python's `no_op_delete` returns a fake success without touching
            // the fake Tart, so the clone survives the "successful" delete.
            return Ok(TartOutput::default());
        }
        let result = self.inner.tart(args, check, timeout_s);
        if result.is_ok() {
            let observer = self.tart_observer.lock().expect("tart_observer").clone();
            if let Some(observer) = observer {
                observer(&op, args);
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
        // Record the invocation even when the injected failure short-circuits,
        // mirroring a `unittest.mock` call list.
        self.shared.lock().expect("shared").bootstrap_args.push((
            ip.to_string(),
            user.to_string(),
            password.to_string(),
            key_dir.to_path_buf(),
            timeout_s,
        ));
        if let Some(message) = self.bootstrap_error.lock().expect("bootstrap_error").take() {
            return Err(OpError::new(message));
        }
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

// ---------------------------------------------------------------------------
// Shared hook state and the key-lifecycle fixture.
// ---------------------------------------------------------------------------

/// One recorded `ssh` invocation: `(ip, user, key_dir, command, stdin, timeout)`.
type SshFullCall = (String, String, PathBuf, String, Option<Vec<u8>>, u64);
/// One recorded `scp` invocation: `(ip, user, key_dir, src, dst, timeout)`.
type ScpFullCall = (String, String, PathBuf, String, String, u64);
/// One recorded `bootstrap` invocation.
type BootstrapArgs = (String, String, String, PathBuf, f64);
/// One recorded `wait_ssh` invocation.
type WaitSshArgs = (String, String, PathBuf, u64);

/// Mutable state shared between the test and its hooks.
#[derive(Default)]
struct Shared {
    events: Vec<String>,
    remote_bytes: HashMap<String, Vec<u8>>,
    transfer_failure: Option<String>,
    ssh_calls: Vec<SshFullCall>,
    scp_calls: Vec<ScpFullCall>,
    bootstrap_args: Vec<BootstrapArgs>,
    wait_ssh_args: Vec<WaitSshArgs>,
    wait_ssh_ok: bool,
    custom_ssh: Option<common::SshHook>,
}

/// A `threading.Event`-like primitive for the concurrency tests.
struct Event {
    flag: Mutex<bool>,
    cond: Condvar,
}

impl Event {
    fn new() -> Self {
        Self {
            flag: Mutex::new(false),
            cond: Condvar::new(),
        }
    }

    fn set(&self) {
        *self.flag.lock().expect("event") = true;
        self.cond.notify_all();
    }

    #[must_use]
    fn wait_timeout(&self, timeout: Duration) -> bool {
        let mut flag = self.flag.lock().expect("event");
        let deadline = Instant::now() + timeout;
        while !*flag {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let (guard, _) = self
                .cond
                .wait_timeout(flag, deadline - now)
                .expect("event wait");
            flag = guard;
        }
        *flag
    }
}

/// The ported `KeyLifecycleFixture`: real isolated disk state and lease keys,
/// fake transport, and the byte-copy probes of the Python fixture.
struct KeyFixture {
    base: common::Fixture,
    service: Arc<Service>,
    host: Arc<TestHost>,
    shared: Arc<Mutex<Shared>>,
}

impl KeyFixture {
    fn new() -> Self {
        let base = common::Fixture::with_transfer_stub(false);
        let state_dir = base.service.config.state_dir.clone();
        let shared = Arc::new(Mutex::new(Shared {
            wait_ssh_ok: true,
            ..Shared::default()
        }));
        let host = Arc::new(TestHost::new(Arc::clone(&base.host), Arc::clone(&shared)));
        let service = Arc::new(Service::with_host(
            base.service.config.clone(),
            None,
            Arc::clone(&host) as Arc<dyn Host>,
        ));
        service
            .initialize_image_catalog()
            .expect("in-memory image catalog");
        install_hooks(&base.host, &Arc::downgrade(&service), &state_dir, &shared);
        Self {
            base,
            service,
            host,
            shared,
        }
    }

    fn state_dir(&self) -> PathBuf {
        self.service.config.state_dir.clone()
    }

    fn key_path(&self, vm: &str) -> PathBuf {
        self.state_dir().join("ssh").join(vm)
    }

    fn acquire(&self, purpose: &str) -> OpResult<Value> {
        self.acquire_env(purpose, "none", true)
    }

    fn acquire_env(&self, purpose: &str, env: &str, wait: bool) -> OpResult<Value> {
        self.service.acquire(
            purpose,
            "ubuntu2404",
            env,
            &json!(24),
            None,
            None,
            None,
            wait,
            "nat",
            None,
            "base",
            None,
            false,
            false,
        )
    }

    fn release(&self, vm: &str) -> OpResult<Value> {
        self.service.release(vm, "released", false)
    }

    fn events(&self) -> Vec<String> {
        self.shared.lock().expect("shared").events.clone()
    }

    fn bootstrap_args(&self) -> Vec<BootstrapArgs> {
        self.shared.lock().expect("shared").bootstrap_args.clone()
    }

    fn wait_ssh_args(&self) -> Vec<WaitSshArgs> {
        self.shared.lock().expect("shared").wait_ssh_args.clone()
    }

    fn ssh_calls(&self) -> Vec<SshFullCall> {
        self.shared.lock().expect("shared").ssh_calls.clone()
    }

    fn scp_calls(&self) -> Vec<ScpFullCall> {
        self.shared.lock().expect("shared").scp_calls.clone()
    }

    fn remote_bytes_len(&self) -> usize {
        self.shared.lock().expect("shared").remote_bytes.len()
    }

    fn reset_ssh(&self) {
        self.shared.lock().expect("shared").ssh_calls.clear();
    }

    fn reset_scp(&self) {
        self.shared.lock().expect("shared").scp_calls.clear();
    }

    fn reset_bootstrap(&self) {
        self.shared.lock().expect("shared").bootstrap_args.clear();
    }

    fn set_wait_ssh(&self, ok: bool) {
        self.shared.lock().expect("shared").wait_ssh_ok = ok;
    }

    fn set_transfer_failure(&self, failure: &str) {
        self.shared.lock().expect("shared").transfer_failure = Some(failure.to_string());
    }

    fn set_custom_ssh(&self, hook: common::SshHook) {
        self.shared.lock().expect("shared").custom_ssh = Some(hook);
    }

    fn set_bootstrap_error(&self, message: &str) {
        *self.host.bootstrap_error.lock().expect("bootstrap_error") = Some(message.to_string());
    }

    fn set_no_op_delete(&self, value: bool) {
        self.host.no_op_delete.store(value, Ordering::SeqCst);
    }

    fn set_tart_observer(&self, observer: TartObserver) {
        *self.host.tart_observer.lock().expect("tart_observer") = Some(observer);
    }

    /// How many times the image catalog was read since the last reset.
    fn discover_calls(&self) -> usize {
        self.base.host.catalog.identity_reads.load(Ordering::SeqCst)
    }

    fn reset_discover_calls(&self) {
        self.base
            .host
            .catalog
            .identity_reads
            .store(0, Ordering::SeqCst);
    }

    fn bootstrap_vm(&self, index: usize) -> String {
        self.bootstrap_args()[index]
            .3
            .file_name()
            .expect("key dir name")
            .to_string_lossy()
            .to_string()
    }

    fn assert_removed(&self, vm: &str) {
        assert!(
            !self.base.read_state().contains_key(vm),
            "record remains for {vm}"
        );
        assert!(!self.base.host.tart.exists(vm), "VM remains: {vm}");
        assert!(!self.key_path(vm).exists(), "keys remain: {vm}");
    }
}

/// The Python `observe_unready`: every readiness stage runs while the record is
/// `provisioning`, unverified, with the expected key directory.
fn observe_unready(
    shared: &Arc<Mutex<Shared>>,
    service: &Weak<Service>,
    state_dir: &Path,
    stage: &str,
    key_dir: &Path,
) {
    let vm = key_dir
        .file_name()
        .expect("key dir name")
        .to_string_lossy()
        .to_string();
    let service = service.upgrade().expect("service alive during hook");
    let record = service.get_record(&vm).expect("record");
    assert_eq!(record["state"], "provisioning", "stage {stage}");
    assert_eq!(record["ssh_verified"], false, "stage {stage}");
    assert_eq!(
        lease_keys::directory(state_dir, &vm).expect("lease keys"),
        key_dir,
        "stage {stage}"
    );
    shared
        .lock()
        .expect("shared")
        .events
        .push(stage.to_string());
}

fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .expect("stat key material")
        .permissions()
        .mode()
        & 0o777
}

/// Install the fixture's `bootstrap`, `wait_ssh`, `scp`, `ssh`, and
/// `verify_transfer` replacements, mirroring
/// `mock.patch.object(svc, "_ssh", side_effect=self.ssh)` and friends.
fn install_hooks(
    inner: &Arc<common::FakeHost>,
    service: &Weak<Service>,
    state_dir: &Path,
    shared: &Arc<Mutex<Shared>>,
) {
    // bootstrap: observe_unready, then assert the generated key material.
    {
        let shared = Arc::clone(shared);
        let service = service.clone();
        let state_dir = state_dir.to_path_buf();
        *inner.bootstrap_hook.lock().expect("bootstrap_hook") = Some(Arc::new(
            move |_ip: &str, _user: &str, _password: &str, key_dir: &Path, _timeout: f64| {
                observe_unready(&shared, &service, &state_dir, "bootstrap", key_dir);
                let identity = std::fs::read(key_dir.join("identity")).expect("identity");
                assert!(
                    identity.starts_with(b"-----BEGIN OPENSSH PRIVATE KEY-----"),
                    "identity is not an OpenSSH private key"
                );
                let public =
                    std::fs::read_to_string(key_dir.join("identity.pub")).expect("identity.pub");
                assert!(public.starts_with("ssh-ed25519 "), "identity.pub prefix");
                assert_eq!(mode(key_dir), 0o700, "key dir mode");
                for name in ["identity", "identity.pub", "known_hosts"] {
                    assert_eq!(mode(&key_dir.join(name)), 0o600, "{name} mode");
                }
                // A fixture marker only: not a real host pin/authentication proof.
                std::fs::write(key_dir.join("known_hosts"), "offline-fixture-host-pin\n")
                    .expect("known_hosts");
            },
        ));
    }

    // wait_ssh: key-command stage, with a switchable failure.
    {
        let shared = Arc::clone(shared);
        let service = service.clone();
        let state_dir = state_dir.to_path_buf();
        *inner.wait_ssh_hook.lock().expect("wait_ssh_hook") = Some(Arc::new(
            move |ip: &str, user: &str, key_dir: &Path, timeout: u64| {
                let ok = {
                    let mut guard = shared.lock().expect("shared");
                    guard.wait_ssh_args.push((
                        ip.to_string(),
                        user.to_string(),
                        key_dir.to_path_buf(),
                        timeout,
                    ));
                    guard.wait_ssh_ok
                };
                if ok {
                    observe_unready(&shared, &service, &state_dir, "key-command", key_dir);
                }
                ok
            },
        ));
    }

    // scp: the Python `copy_bytes` probe.
    {
        let shared = Arc::clone(shared);
        let service = service.clone();
        let state_dir = state_dir.to_path_buf();
        let inner_for_scp = Arc::clone(inner);
        *inner.scp_hook.lock().expect("scp_hook") = Some(Arc::new(
            move |ip: &str, user: &str, key_dir: &Path, src: &str, dst: &str, timeout: u64| {
                shared.lock().expect("shared").scp_calls.push((
                    ip.to_string(),
                    user.to_string(),
                    key_dir.to_path_buf(),
                    src.to_string(),
                    dst.to_string(),
                    timeout,
                ));
                if dst.contains("vm-service-key-probe-") {
                    observe_unready(&shared, &service, &state_dir, "upload", key_dir);
                    if shared.lock().expect("shared").transfer_failure.as_deref() == Some("upload")
                    {
                        return Some((1, "upload refused".to_string()));
                    }
                    if let Ok(bytes) = std::fs::read(src) {
                        shared
                            .lock()
                            .expect("shared")
                            .remote_bytes
                            .insert(dst.to_string(), bytes);
                    }
                    return Some((0, String::new()));
                }
                if src.contains("vm-service-key-probe-") {
                    observe_unready(&shared, &service, &state_dir, "download", key_dir);
                    let failure = shared.lock().expect("shared").transfer_failure.clone();
                    if failure.as_deref() == Some("download") {
                        return Some((1, "download refused".to_string()));
                    }
                    let mut content = shared
                        .lock()
                        .expect("shared")
                        .remote_bytes
                        .get(src)
                        .cloned()
                        .unwrap_or_default();
                    if failure.as_deref() == Some("checksum") {
                        if let Some(first) = content.first_mut() {
                            *first ^= 1;
                        }
                    }
                    if failure.as_deref() != Some("missing-output") {
                        let _ = std::fs::write(dst, &content);
                    }
                    return Some((0, String::new()));
                }
                let prefix = format!("{user}@{ip}:/tmp/pack/");
                if dst.starts_with(&prefix) {
                    observe_unready(&shared, &service, &state_dir, "env-upload", key_dir);
                }
                inner_for_scp.ssh.scp(src, dst, timeout)
            },
        ));
    }

    // ssh: probe cleanup, env check, then forward to the fake.
    {
        let shared = Arc::clone(shared);
        let service = service.clone();
        let state_dir = state_dir.to_path_buf();
        let inner_for_ssh = Arc::clone(inner);
        *inner.ssh_hook.lock().expect("ssh_hook") = Some(Arc::new(
            move |ip: &str,
                  user: &str,
                  key_dir: &Path,
                  remote_cmd: &str,
                  stdin: Option<&[u8]>,
                  timeout: u64| {
                let custom = {
                    let mut guard = shared.lock().expect("shared");
                    guard.ssh_calls.push((
                        ip.to_string(),
                        user.to_string(),
                        key_dir.to_path_buf(),
                        remote_cmd.to_string(),
                        stdin.map(|bytes| bytes.to_vec()),
                        timeout,
                    ));
                    guard.custom_ssh.clone()
                };
                if let Some(custom) = custom {
                    return custom(ip, user, key_dir, remote_cmd, stdin, timeout);
                }
                if remote_cmd.contains("vm-service-key-probe-") {
                    observe_unready(&shared, &service, &state_dir, "probe-cleanup", key_dir);
                    if shared.lock().expect("shared").transfer_failure.as_deref() == Some("cleanup")
                    {
                        return Some((1, "cleanup refused".to_string()));
                    }
                    shared.lock().expect("shared").remote_bytes.clear();
                }
                if remote_cmd.contains("test -s ~/.config/zsh/secrets.zsh") {
                    observe_unready(&shared, &service, &state_dir, "env-check", key_dir);
                }
                inner_for_ssh
                    .ssh
                    .ssh(remote_cmd, stdin.map(|bytes| bytes.to_vec()), timeout)
            },
        ));
    }

    // verify_transfer: the real sequence, over the fake hooks.
    {
        let inner_for_transfer = Arc::clone(inner);
        let state_dir = state_dir.to_path_buf();
        *inner.transfer_hook.lock().expect("transfer_hook") =
            Some(Arc::new(move |ip: &str, user: &str, key_dir: &Path| {
                verify_transfer_over_fake(&inner_for_transfer, &state_dir, ip, user, key_dir)
            }));
    }
}

/// The Python `verify_transfer` sequence, driven through the fake `scp`/`ssh`.
fn verify_transfer_over_fake(
    host: &Arc<common::FakeHost>,
    state_dir: &Path,
    ip: &str,
    user: &str,
    key_dir: &Path,
) -> OpResult<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nonce = format!(
        "{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    );
    let remote = format!("/var/tmp/vm-service-key-probe-{nonce}");
    let temp = tempfile::Builder::new()
        .prefix("key-probe-")
        .tempdir_in(state_dir)
        .map_err(OpError::from)?;
    let source = temp.path().join("source");
    let destination = temp.path().join("returned");
    let content = probe_bytes();
    // The Python probe compares the round-tripped payload to a literal
    // 4096-byte `os.urandom(4096)`; pin the length here so a shorter probe can
    // never pass the checksum comparison.
    assert_eq!(
        content.len(),
        4096,
        "readiness probe payload must be exactly 4096 bytes"
    );
    std::fs::write(&source, &content).map_err(OpError::from)?;

    let result = (|| -> OpResult<()> {
        let sent = host.scp(
            ip,
            user,
            key_dir,
            &source.to_string_lossy(),
            &format!("{user}@{ip}:{remote}"),
            30,
        );
        if sent.as_ref().map(|(rc, _)| *rc) != Some(0) {
            return Err(OpError::new("Key-only readiness upload failed"));
        }
        let received = host.scp(
            ip,
            user,
            key_dir,
            &format!("{user}@{ip}:{remote}"),
            &destination.to_string_lossy(),
            30,
        );
        let downloaded = received.as_ref().map(|(rc, _)| *rc) == Some(0);
        let matches = std::fs::read(&destination)
            .map(|bytes| bytes == content)
            .unwrap_or(false);
        if !downloaded || !matches {
            return Err(OpError::new(
                "Key-only readiness download or checksum failed",
            ));
        }
        Ok(())
    })();

    let cleanup = format!("rm -f -- {}", shell_quote(&remote));
    // Literal Python cleanup: `rm -f -- /var/tmp/<...>-key-probe-<nonce>`.
    assert!(
        cleanup.starts_with("rm -f -- /var/tmp/") && cleanup.contains("key-probe-"),
        "unexpected probe cleanup command: {cleanup}"
    );
    let removed = host.ssh(ip, user, key_dir, &cleanup, None, 20);
    if result.is_ok() && removed.as_ref().map(|(rc, _)| *rc) != Some(0) {
        return Err(OpError::new("Key-only readiness probe cleanup failed"));
    }
    result
}

fn probe_bytes() -> Vec<u8> {
    use std::io::Read;
    let mut buffer = vec![0_u8; 4096];
    if let Ok(mut file) = std::fs::File::open("/dev/urandom") {
        if file.read_exact(&mut buffer).is_ok() {
            return buffer;
        }
    }
    for (index, byte) in buffer.iter_mut().enumerate() {
        *byte = (index as u8).wrapping_mul(31);
    }
    buffer
}

fn key_dir_snapshot(dir: &Path) -> HashMap<String, Vec<u8>> {
    let mut snapshot = HashMap::new();
    for entry in std::fs::read_dir(dir).expect("key dir") {
        let entry = entry.expect("key entry");
        let name = entry.file_name().to_string_lossy().to_string();
        snapshot.insert(name, std::fs::read(entry.path()).expect("key bytes"));
    }
    snapshot
}

// ---------------------------------------------------------------------------
// TestKeyReadiness
// ---------------------------------------------------------------------------

#[test]
fn keys_generated_before_bootstrap_and_running_after_all_readiness() {
    let fixture = KeyFixture::new();
    let _ = fixture.base.seed_pack("default", true, false);

    // `lease_keys.create` has no hook. The clone `tart` call is the first
    // observable moment after creation, and the record is still `pending`
    // there, reproducing the Python `generated` wrapper's assertions.
    let state_dir = fixture.state_dir();
    let service = Arc::downgrade(&fixture.service);
    let shared = Arc::clone(&fixture.shared);
    fixture.set_tart_observer(Arc::new(move |op, args| {
        if op == "clone" {
            let vm = args.get(2).cloned().unwrap_or_default();
            let service = service.upgrade().expect("service alive");
            let record = service.get_record(&vm).expect("record");
            assert_eq!(record["state"], "pending");
            assert!(state_dir.join("ssh").join(&vm).join("identity").is_file());
            shared
                .lock()
                .expect("shared")
                .events
                .push("generated".to_string());
        }
    }));

    let record = fixture
        .acquire_env("ordered", "default", true)
        .expect("acquire");
    assert_eq!(record["state"], "running");
    assert_eq!(record["ssh_verified"], true);
    assert_eq!(record["ssh_auth"], "lease-key");
    assert_eq!(record["ssh_user"], "admin");
    assert_eq!(record["ssh_host_trust"], "lease-tofu");
    assert_eq!(fixture.remote_bytes_len(), 0);
    // `STATE._store` has no hook; the one running transition is atomic, so the
    // persisted record carrying `running` also carries `ssh_verified`.
    assert_eq!(
        fixture.events(),
        vec![
            "generated",
            "bootstrap",
            "key-command",
            "upload",
            "download",
            "probe-cleanup",
            "env-upload",
            "env-check",
        ]
    );
}

#[test]
fn env_none_still_creates_distinct_keys_and_checks_bytes() {
    let fixture = KeyFixture::new();
    let first = fixture.acquire("one").expect("first acquire");
    let second = fixture.acquire("two").expect("second acquire");
    assert_eq!(first["env"], Value::Null);
    assert_eq!(second["env"], Value::Null);
    let first_vm = first["vm"].as_str().expect("vm");
    let second_vm = second["vm"].as_str().expect("vm");
    assert_ne!(
        std::fs::read(fixture.key_path(first_vm).join("identity")).expect("first identity"),
        std::fs::read(fixture.key_path(second_vm).join("identity")).expect("second identity")
    );
    let mut expected = Vec::new();
    for _ in 0..2 {
        for stage in [
            "bootstrap",
            "key-command",
            "upload",
            "download",
            "probe-cleanup",
        ] {
            expected.push(stage.to_string());
        }
    }
    assert_eq!(fixture.events(), expected);
    assert_eq!(fixture.bootstrap_args().len(), 2);

    // The readiness roundtrip must clean up each probe with the literal
    // `rm -f -- /var/tmp/...-key-probe-...` command and leave no tempdir.
    let cleanups = fixture
        .ssh_calls()
        .into_iter()
        .filter(|call| call.3.starts_with("rm -f -- /var/tmp/") && call.3.contains("key-probe-"))
        .collect::<Vec<_>>();
    assert_eq!(
        cleanups.len(),
        2,
        "expected one probe cleanup per acquire: {cleanups:?}"
    );
    assert!(
        std::fs::read_dir(fixture.state_dir())
            .expect("state dir")
            .all(|entry| !entry
                .expect("state entry")
                .file_name()
                .to_string_lossy()
                .starts_with("key-probe-")),
        "a key-probe tempdir was left behind"
    );
}

#[test]
fn wait_false_still_provisions_and_verifies_with_bounded_probe() {
    let fixture = KeyFixture::new();
    let record = fixture.acquire_env("keys", "none", false).expect("acquire");
    assert_eq!(record["state"], "running");
    assert_eq!(record["ssh_verified"], true);
    let vm = record["vm"].as_str().expect("vm").to_string();
    let key_dir = fixture.key_path(&vm);
    let bootstrap = fixture.bootstrap_args();
    assert_eq!(bootstrap.len(), 1);
    assert_eq!(bootstrap[0].0, "192.168.64.99");
    assert_eq!(bootstrap[0].1, "admin");
    assert_eq!(bootstrap[0].2, "admin");
    assert_eq!(bootstrap[0].3, key_dir);
    assert_eq!(bootstrap[0].4, 30.0);
    let wait = fixture.wait_ssh_args();
    assert_eq!(wait.len(), 1);
    assert_eq!(wait[0].0, "192.168.64.99");
    assert_eq!(wait[0].1, "admin");
    assert_eq!(wait[0].2, key_dir);
    assert_eq!(wait[0].3, 0);
    assert_eq!(
        fixture.events(),
        vec![
            "bootstrap",
            "key-command",
            "upload",
            "download",
            "probe-cleanup"
        ]
    );
}

#[test]
fn command_failure_never_runs_transfer_and_removes_vm_and_keys() {
    let fixture = KeyFixture::new();
    fixture.set_wait_ssh(false);
    let error = fixture.acquire("keys").expect_err("wait_ssh must fail");
    assert!(
        error.to_string().contains("SSH never became ready"),
        "{error}"
    );
    let vm = fixture.bootstrap_vm(0);
    fixture.assert_removed(&vm);
    assert!(fixture.scp_calls().is_empty());
}

#[test]
fn bootstrap_failure_removes_vm_and_keys() {
    let fixture = KeyFixture::new();
    fixture.set_bootstrap_error("bootstrap failed");
    let error = fixture.acquire("keys").expect_err("bootstrap must fail");
    assert!(error.to_string().contains("bootstrap failed"), "{error}");
    let vm = fixture.bootstrap_vm(0);
    fixture.assert_removed(&vm);
    assert!(fixture.wait_ssh_args().is_empty());
}

#[test]
fn transfer_failures_never_mark_running_and_roll_back() {
    let fixture = KeyFixture::new();
    for failure in [
        "upload",
        "download",
        "checksum",
        "missing-output",
        "cleanup",
    ] {
        fixture.set_transfer_failure(failure);
        let error = fixture.acquire(failure).expect_err("transfer must fail");
        assert!(
            error.to_string().contains("readiness"),
            "{failure}: {error}"
        );
        assert!(
            !fixture
                .ssh_calls()
                .iter()
                .any(|call| call.3.contains("/tmp/pack")),
            "{failure}: env pack injected despite transfer failure"
        );
        assert!(
            fixture
                .ssh_calls()
                .iter()
                .any(|call| call.3.starts_with("rm -f -- /var/tmp/")
                    && call.3.contains("key-probe-")),
            "{failure}: probe cleanup command was not issued"
        );
        assert!(
            std::fs::read_dir(fixture.state_dir())
                .expect("state dir")
                .all(|entry| !entry
                    .expect("state entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with("key-probe-")),
            "{failure}: a key-probe tempdir was left behind"
        );
        let vm = fixture.bootstrap_vm(fixture.bootstrap_args().len() - 1);
        fixture.assert_removed(&vm);
    }
}

#[test]
fn envpack_failure_rolls_back_after_successful_key_checks() {
    let fixture = KeyFixture::new();
    let _ = fixture.base.seed_pack("default", true, false);
    fixture.base.host.ssh.map("test -s", 1, "");
    let error = fixture
        .acquire_env("bad-pack", "default", true)
        .expect_err("env pack check must fail");
    assert!(
        error.to_string().contains("env pack injection failed"),
        "{error}"
    );
    let vm = fixture.bootstrap_vm(0);
    fixture.assert_removed(&vm);
    assert_eq!(
        fixture.events(),
        vec![
            "bootstrap",
            "key-command",
            "upload",
            "download",
            "probe-cleanup",
            "env-upload",
            "env-check"
        ]
    );
}

// ---------------------------------------------------------------------------
// TestKeyTeardown
// ---------------------------------------------------------------------------

#[test]
fn release_only_erases_keys_after_vm_absence_proven() {
    let fixture = KeyFixture::new();
    let record = fixture.acquire("keys").expect("acquire");
    let vm = record["vm"].as_str().expect("vm").to_string();
    let key_path = fixture.key_path(&vm);

    // `lease_keys.cleanup` has no hook. Production checks VM absence and then
    // calls cleanup; observe the post-delete moment instead.
    let service = Arc::downgrade(&fixture.service);
    let inner = Arc::clone(&fixture.base.host);
    let observed_vm = vm.clone();
    let observed_key_path = key_path.clone();
    let observed = Arc::new(AtomicBool::new(false));
    let observed_flag = Arc::clone(&observed);
    fixture.set_tart_observer(Arc::new(move |op, _args| {
        if op == "delete" {
            let service = service.upgrade().expect("service alive");
            assert!(
                !inner.vm_exists(&observed_vm).expect("vm_exists"),
                "VM still present at cleanup"
            );
            assert_eq!(
                service.get_record(&observed_vm).expect("record")["state"],
                "releasing"
            );
            assert!(
                observed_key_path.join("identity").is_file(),
                "keys erased before VM absence was proven"
            );
            observed_flag.store(true, Ordering::SeqCst);
        }
    }));

    let result = fixture.release(&vm).expect("release");
    assert_eq!(result["released"], true);
    assert!(observed.load(Ordering::SeqCst), "delete was not observed");
    fixture.assert_removed(&vm);
}

#[test]
fn stop_or_delete_failure_retains_releasing_keys_until_retry() {
    for op in ["stop", "delete"] {
        let fixture = KeyFixture::new();
        let record = fixture.acquire(op).expect("acquire");
        let vm = record["vm"].as_str().expect("vm").to_string();
        let before = std::fs::read(fixture.key_path(&vm).join("identity")).expect("identity");
        fixture.base.host.tart.inject(op, &format!("{op} refused"));
        let error = fixture.release(&vm).expect_err("teardown must fail");
        assert!(
            error.to_string().contains("retained for retry"),
            "{op}: {error}"
        );
        assert_eq!(
            fixture.base.read_state().get(&vm).expect("record")["state"],
            "releasing"
        );
        assert!(fixture.base.host.tart.exists(&vm));
        assert_eq!(
            std::fs::read(fixture.key_path(&vm).join("identity")).expect("identity"),
            before
        );
        fixture.base.host.tart.fail.lock().expect("fail").remove(op);
        fixture.release(&vm).expect("retry release");
        fixture.assert_removed(&vm);
    }
}

#[test]
fn successful_delete_response_without_absence_keeps_keys() {
    let fixture = KeyFixture::new();
    let record = fixture.acquire("keys").expect("acquire");
    let vm = record["vm"].as_str().expect("vm").to_string();
    let before = std::fs::read(fixture.key_path(&vm).join("identity")).expect("identity");
    fixture.set_no_op_delete(true);
    let error = fixture
        .release(&vm)
        .expect_err("delete must not prove absence");
    assert!(error.to_string().contains("retained for retry"), "{error}");
    assert_eq!(
        fixture.base.read_state().get(&vm).expect("record")["state"],
        "releasing"
    );
    assert_eq!(
        std::fs::read(fixture.key_path(&vm).join("identity")).expect("identity"),
        before
    );
    fixture.set_no_op_delete(false);
    fixture.release(&vm).expect("retry release");
    fixture.assert_removed(&vm);
}

#[test]
fn failed_acquire_rollback_retains_record_and_keys_until_gc_retry() {
    let fixture = KeyFixture::new();
    fixture.set_bootstrap_error("bootstrap failed");
    fixture.base.host.tart.inject("delete", "delete refused");
    let error = fixture.acquire("keys").expect_err("acquire must fail");
    assert!(
        error.to_string().contains("retained for cleanup"),
        "{error}"
    );
    let vm = fixture.bootstrap_vm(0);
    let record = fixture.base.read_state().get(&vm).expect("record").clone();
    assert_eq!(record["state"], "releasing");
    assert_eq!(record["ssh_verified"], false);
    assert!(fixture.key_path(&vm).join("identity").is_file());
    assert!(fixture.base.host.tart.exists(&vm));
    fixture
        .base
        .host
        .tart
        .fail
        .lock()
        .expect("fail")
        .remove("delete");
    fixture.service.gc_once().expect("gc_once");
    fixture.assert_removed(&vm);
}

// ---------------------------------------------------------------------------
// TestPersistedIdentity
// ---------------------------------------------------------------------------

#[test]
fn legacy_lease_exec_refuses_password_fallback_but_release_works() {
    let fixture = KeyFixture::new();
    let vm = "pilot-legacy-abc";
    fixture.base.write_state(json!({
        vm: {
            "vm": vm,
            "purpose": "legacy",
            "image": "ubuntu2404",
            "image_kind": "linux",
            "env": null,
            "state": "running",
            "ip": "192.168.64.99",
            "ttl_expires_at": 4102444800.0
        }
    }));
    fixture
        .base
        .host
        .tart
        .vms
        .lock()
        .expect("vms")
        .insert(vm.to_string(), true);

    let error = fixture
        .service
        .guest_exec(vm, &json!({"argv": ["true"]}))
        .expect_err("no lease key");
    assert!(
        error.to_string().contains("no password fallback"),
        "{error}"
    );
    // `lease_keys.create` has no hook; a missing key directory proves it was
    // never called.
    assert!(!fixture.key_path(vm).exists());
    assert!(fixture.bootstrap_args().is_empty());
    assert!(fixture.ssh_calls().is_empty());
    fixture.release(vm).expect("release");
    fixture.assert_removed(vm);
}

#[test]
fn missing_keys_refuse_all_operations_without_regenerating() {
    let fixture = KeyFixture::new();
    let record = fixture.acquire("keys").expect("acquire");
    let vm = record["vm"].as_str().expect("vm").to_string();
    let key_dir = fixture.key_path(&vm);
    std::fs::remove_file(key_dir.join("identity")).expect("remove identity");
    let local = fixture.base.dir.path().join("payload");
    std::fs::write(&local, "safe").expect("payload");
    fixture.reset_ssh();
    fixture.reset_scp();
    fixture.reset_bootstrap();

    let exec = fixture
        .service
        .guest_exec(&vm, &json!({"argv": ["true"]}))
        .expect_err("missing keys refuse exec");
    assert!(
        exec.to_string().contains("credentials unavailable"),
        "{exec}"
    );
    let push = fixture
        .service
        .guest_push(
            &vm,
            &json!({"local_path": local.to_string_lossy(), "remote_path": "/tmp/payload"}),
        )
        .expect_err("missing keys refuse push");
    assert!(
        push.to_string().contains("credentials unavailable"),
        "{push}"
    );
    let pull = fixture
        .service
        .guest_pull(
            &vm,
            &json!({"local_path": local.to_string_lossy(), "remote_path": "/tmp/payload"}),
        )
        .expect_err("missing keys refuse pull");
    assert!(
        pull.to_string().contains("credentials unavailable"),
        "{pull}"
    );

    assert!(fixture.bootstrap_args().is_empty());
    assert!(fixture.ssh_calls().is_empty());
    assert!(fixture.scp_calls().is_empty());
    assert_eq!(
        fixture.base.read_state().get(&vm).expect("record")["state"],
        "running"
    );
    // No regeneration: the removed identity is still absent.
    assert!(!key_dir.join("identity").exists());
    fixture.release(&vm).expect("release");
    fixture.assert_removed(&vm);
}

#[test]
fn restart_uses_persisted_identity_without_bootstrap_or_rekey() {
    let fixture = KeyFixture::new();
    let record = fixture.acquire("keys").expect("acquire");
    let vm = record["vm"].as_str().expect("vm").to_string();
    let key_dir = fixture.key_path(&vm);
    let before = key_dir_snapshot(&key_dir);

    let restarted = Arc::new(Service::with_host(
        fixture.service.config.clone(),
        None,
        Arc::clone(&fixture.host) as Arc<dyn Host>,
    ));
    restarted
        .initialize_image_catalog()
        .expect("restarted image catalog");
    fixture.reset_bootstrap();
    fixture.reset_ssh();
    fixture.reset_discover_calls();
    fixture.set_custom_ssh(Arc::new(|_ip, _user, _key_dir, _cmd, _stdin, _timeout| {
        Some((0, "persisted".to_string()))
    }));

    let out = restarted
        .guest_exec(&vm, &json!({"argv": ["echo", "again"]}))
        .expect("guest_exec");
    assert_eq!(out["output"], "persisted");
    let calls = fixture.ssh_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, record["ip"].as_str().expect("ip"));
    assert_eq!(calls[0].1, record["ssh_user"].as_str().expect("ssh_user"));
    assert_eq!(calls[0].2, key_dir);
    assert!(fixture.bootstrap_args().is_empty());
    // No line rediscovery, standing in for the Python `line_cfg` patch.
    assert_eq!(fixture.discover_calls(), 0);
    assert_eq!(key_dir_snapshot(&key_dir), before);
}

#[test]
fn changed_line_credentials_do_not_change_leased_exec_push_pull_identity() {
    let fixture = KeyFixture::new();
    let record = fixture.acquire("keys").expect("acquire");
    let vm = record["vm"].as_str().expect("vm").to_string();
    let key_dir = fixture.key_path(&vm);
    let ip = record["ip"].as_str().expect("ip").to_string();

    // Replace the line credentials so a rediscovery would visibly change the
    // identity used (`bash -s` -> `zsh -s`, `admin` -> `replacement`).
    let mut replacement = common::default_lines();
    replacement.insert(
        "ubuntu2404".to_string(),
        json!({
            "kind": "macos",
            "base_vm": "pilot-ubuntu-base",
            "clone_prefix": "pilot-",
            "ssh_user": "replacement",
            "ssh_pass": "new-password",
            "defaults": {},
            "source": "test://ubuntu2404/line.conf"
        }),
    );
    *fixture
        .base
        .host
        .lines_override
        .lock()
        .expect("lines_override") = Some(replacement);
    fixture.reset_bootstrap();
    fixture.reset_ssh();
    fixture.reset_scp();
    fixture.reset_discover_calls();

    let local = fixture.base.dir.path().join("payload");
    std::fs::write(&local, "safe").expect("payload");
    fixture
        .service
        .guest_exec(&vm, &json!({"script": "echo unchanged"}))
        .expect("guest_exec");
    fixture
        .service
        .guest_push(
            &vm,
            &json!({"local_path": local.to_string_lossy(), "remote_path": "/tmp/payload"}),
        )
        .expect("guest_push");
    fixture
        .service
        .guest_pull(
            &vm,
            &json!({"local_path": local.to_string_lossy(), "remote_path": "/tmp/payload"}),
        )
        .expect("guest_pull");

    let calls = fixture.ssh_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, ip);
    assert_eq!(calls[0].1, "admin");
    assert_eq!(calls[0].2, key_dir);
    assert_eq!(calls[0].3, "bash -s");
    let scp = fixture.scp_calls();
    assert_eq!(scp.len(), 2);
    let needle = format!("admin@{ip}:/tmp/payload");
    for call in &scp {
        assert_eq!(call.0, ip);
        assert_eq!(call.1, "admin");
        assert_eq!(call.2, key_dir);
        assert!(call.3.contains(&needle) || call.4.contains(&needle));
    }
    assert!(fixture.bootstrap_args().is_empty());
    assert_eq!(fixture.discover_calls(), 0);
}

#[test]
fn exec_exit_255_is_returned_once_without_replay_or_bootstrap() {
    let fixture = KeyFixture::new();
    let record = fixture.acquire("keys").expect("acquire");
    let vm = record["vm"].as_str().expect("vm").to_string();
    let identity_before = std::fs::read(fixture.key_path(&vm).join("identity")).expect("identity");
    fixture.reset_bootstrap();
    fixture.reset_ssh();
    fixture.set_custom_ssh(Arc::new(|_ip, _user, _key_dir, _cmd, _stdin, _timeout| {
        Some((255, "uncertain execution".to_string()))
    }));

    let script = "side-effect-command";
    let result = fixture
        .service
        .guest_exec(&vm, &json!({"script": script}))
        .expect("guest_exec");
    assert_eq!(result["rc"], 255);
    assert_eq!(result["output"], "uncertain execution");
    let calls = fixture.ssh_calls();
    assert_eq!(calls.len(), 1);
    // Stdin is forwarded verbatim to the single ssh invocation, so a script
    // reaches the guest exactly once and is never replayed.
    assert_eq!(
        calls[0].4.as_deref(),
        Some(script.as_bytes()),
        "stdin was not forwarded to ssh"
    );
    // The ssh identity args never introduce sshpass. The full argv composition
    // (including the absence of sshpass) is pinned by
    // `machine-service-core::ssh::tests::ssh_argv_is_key_only_and_never_sshpass`.
    let identity_args = lease_keys::key_args(&calls[0].2).expect("key args");
    assert!(
        !identity_args.iter().any(|arg| arg.contains("sshpass")),
        "ssh identity args mention sshpass: {identity_args:?}"
    );
    assert!(fixture.bootstrap_args().is_empty());
    assert_eq!(
        fixture.base.read_state().get(&vm).expect("record")["state"],
        "running"
    );
    assert_eq!(
        std::fs::read(fixture.key_path(&vm).join("identity")).expect("identity"),
        identity_before
    );
}

/// A release commits `releasing` at once and sets the VM's release flag; see
/// `docs/lifecycle-fixes.md` (V4). This fixture's exec does not run through
/// the cancellable subprocess helper, so it stands for an operation that
/// cannot be preempted: teardown must still wait for it, and must not remove
/// the lease key under it. A heartbeat returns at once and is refused,
/// because teardown has committed.
#[test]
fn release_waits_for_inflight_exec_without_blocking_heartbeat() {
    let fixture = KeyFixture::new();
    let record = fixture.acquire("keys").expect("acquire");
    let vm = record["vm"].as_str().expect("vm").to_string();
    let previous_ttl = record["ttl_expires_at"].as_f64().expect("ttl");

    let entered = Arc::new(Event::new());
    let finish = Arc::new(Event::new());
    let release_done = Arc::new(Event::new());
    let heartbeat_done = Arc::new(Event::new());
    let key_path = fixture.key_path(&vm);
    {
        let entered = Arc::clone(&entered);
        let finish = Arc::clone(&finish);
        let key_path = key_path.clone();
        fixture.set_custom_ssh(Arc::new(
            move |_ip, _user, _key_dir, _cmd, _stdin, _timeout| {
                entered.set();
                assert!(
                    finish.wait_timeout(Duration::from_secs(5)),
                    "exec fixture was not unblocked"
                );
                assert!(key_path.join("identity").is_file());
                Some((0, "finished".to_string()))
            },
        ));
    }

    let results: Arc<Mutex<HashMap<String, OpResult<Value>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let service = Arc::clone(&fixture.service);

    let exec_service = Arc::clone(&service);
    let exec_vm = vm.clone();
    let exec_results = Arc::clone(&results);
    let exec = std::thread::spawn(move || {
        let result = exec_service.guest_exec(&exec_vm, &json!({"argv": ["slow"]}));
        exec_results
            .lock()
            .expect("results")
            .insert("exec".into(), result);
    });
    assert!(
        entered.wait_timeout(Duration::from_secs(2)),
        "exec did not enter transport"
    );

    let release_service = Arc::clone(&service);
    let release_vm = vm.clone();
    let release_results = Arc::clone(&results);
    let release_done_thread = Arc::clone(&release_done);
    let release = std::thread::spawn(move || {
        let result = release_service.release(&release_vm, "released", false);
        release_results
            .lock()
            .expect("results")
            .insert("release".into(), result);
        release_done_thread.set();
    });
    // Bounded substitute for the Python `operation_lock` patch: release must
    // reach and block on the per-VM lock held by the in-flight exec, after
    // committing `releasing`.
    assert!(
        !release_done.wait_timeout(Duration::from_millis(100)),
        "release overtook in-flight exec"
    );
    assert_eq!(
        fixture.base.read_state().get(&vm).expect("record")["state"],
        "releasing"
    );
    assert!(key_path.join("identity").is_file());

    let heartbeat_service = Arc::clone(&service);
    let heartbeat_vm = vm.clone();
    let heartbeat_results = Arc::clone(&results);
    let heartbeat_done_thread = Arc::clone(&heartbeat_done);
    let heartbeat = std::thread::spawn(move || {
        let result = heartbeat_service.heartbeat(&heartbeat_vm, Some(&json!(5)));
        heartbeat_results
            .lock()
            .expect("results")
            .insert("heartbeat".into(), result);
        heartbeat_done_thread.set();
    });
    assert!(
        heartbeat_done.wait_timeout(Duration::from_secs(2)),
        "heartbeat blocked on guest operation"
    );
    let unchanged_ttl = fixture.base.read_state().get(&vm).expect("record")["ttl_expires_at"]
        .as_f64()
        .expect("ttl");
    assert_eq!(
        unchanged_ttl, previous_ttl,
        "heartbeat renewed a releasing lease"
    );

    finish.set();
    exec.join().expect("exec thread");
    release.join().expect("release thread");
    heartbeat.join().expect("heartbeat thread");

    let results = results.lock().expect("results");
    let exec_result = results
        .get("exec")
        .expect("exec result")
        .as_ref()
        .expect("exec ok");
    assert_eq!(exec_result["output"], "finished");
    let release_result = results
        .get("release")
        .expect("release result")
        .as_ref()
        .expect("release ok");
    assert_eq!(release_result["released"], true);
    let heartbeat_error = results
        .get("heartbeat")
        .expect("heartbeat result")
        .as_ref()
        .expect_err("a releasing lease cannot renew");
    assert!(
        heartbeat_error.to_string().contains("releasing"),
        "{heartbeat_error}"
    );
    drop(results);
    fixture.assert_removed(&vm);
}

// ---------------------------------------------------------------------------
// TestKeyTransferProtection
// ---------------------------------------------------------------------------

#[test]
fn push_pull_reject_private_paths_ancestors_and_canonical_aliases() {
    let fixture = KeyFixture::new();
    let record = fixture.acquire("keys").expect("acquire");
    let vm = record["vm"].as_str().expect("vm").to_string();
    let key_dir = fixture.key_path(&vm);
    let identity = key_dir.join("identity");
    let before = std::fs::read(&identity).expect("identity");
    fixture.reset_scp();
    let tmp = fixture.base.dir.path().to_path_buf();
    let alias = tmp.join("identity-alias");
    let directory_alias = tmp.join("ssh-alias");
    std::os::unix::fs::symlink(&identity, &alias).expect("identity alias");
    std::os::unix::fs::symlink(&key_dir, &directory_alias).expect("key dir alias");
    let state_dir = fixture.state_dir();
    let paths = vec![
        identity.clone(),
        key_dir.clone(),
        key_dir.parent().expect("key dir parent").to_path_buf(),
        state_dir.clone(),
        state_dir.parent().expect("state dir parent").to_path_buf(),
        alias,
        directory_alias.join("identity"),
        key_dir
            .join("..")
            .join(key_dir.file_name().expect("name"))
            .join("identity"),
        key_dir.join("new-key"),
    ];
    for (index, local) in paths.iter().enumerate() {
        // A new destination is meaningful only for download.
        if local.exists() {
            let error = fixture
                .service
                .guest_push(
                    &vm,
                    &json!({"local_path": local.to_string_lossy(), "remote_path": "/tmp/data"}),
                )
                .expect_err("push must reject private material");
            assert!(
                error.to_string().contains("private lease SSH material"),
                "push {index}: {error}"
            );
        }
        let error = fixture
            .service
            .guest_pull(
                &vm,
                &json!({"local_path": local.to_string_lossy(), "remote_path": "/tmp/data"}),
            )
            .expect_err("pull must reject private material");
        assert!(
            error.to_string().contains("private lease SSH material"),
            "pull {index}: {error}"
        );
    }
    assert!(fixture.scp_calls().is_empty());
    assert_eq!(std::fs::read(&identity).expect("identity"), before);
    assert!(!key_dir.join("new-key").exists());
}

#[test]
fn recursive_transfer_tree_rejects_nested_symlinks() {
    let fixture = KeyFixture::new();
    let record = fixture.acquire("keys").expect("acquire");
    let vm = record["vm"].as_str().expect("vm").to_string();
    let tree = fixture.base.dir.path().join("tree");
    std::fs::create_dir_all(tree.join("nested")).expect("tree");
    std::os::unix::fs::symlink(
        fixture.key_path(&vm).join("identity"),
        tree.join("nested").join("key"),
    )
    .expect("nested symlink");
    fixture.reset_scp();

    for operation in ["push", "pull"] {
        let body = json!({"local_path": tree.to_string_lossy(), "remote_path": "/tmp/tree"});
        let error = if operation == "push" {
            fixture.service.guest_push(&vm, &body)
        } else {
            fixture.service.guest_pull(&vm, &body)
        }
        .expect_err("nested symlink must be rejected");
        assert!(
            error.to_string().contains("symlinks"),
            "{operation}: {error}"
        );
    }
    assert!(fixture.scp_calls().is_empty());
}

#[test]
fn envpack_refuses_symlink_export_and_rolls_back() {
    let fixture = KeyFixture::new();
    let pack = fixture.base.seed_pack("default", true, false);
    let original = pack.join("original");
    std::fs::rename(pack.join("env.extra"), &original).expect("rename env.extra");
    std::os::unix::fs::symlink(&original, pack.join("env.extra")).expect("env.extra symlink");

    let error = fixture
        .acquire_env("linked-pack", "default", true)
        .expect_err("symlink pack must fail");
    assert!(error.to_string().contains("symlinks"), "{error}");
    let vm = fixture.bootstrap_vm(0);
    fixture.assert_removed(&vm);
    assert!(
        !fixture
            .scp_calls()
            .iter()
            .any(|call| call.3.contains("env.extra")),
        "env.extra was exported"
    );
}

// ---------------------------------------------------------------------------
// test_gc_renewal.py
// ---------------------------------------------------------------------------

/// A renewal that lands before GC commits teardown preserves the lease, even
/// while a guest operation holds the VM's operation lock. GC decides under
/// the state lock before it touches the operation lock, so it no longer
/// queues on the lock with a stale decision; see `docs/lifecycle-fixes.md`
/// (V4). A renewal after the commit is refused
/// (`heartbeat_refuses_after_teardown_has_committed`).
#[test]
fn renewal_while_gc_waits_preserves_vm_and_private_key() {
    let fixture = KeyFixture::new();
    let record = fixture.acquire("keys").expect("acquire");
    let vm = record["vm"].as_str().expect("vm").to_string();
    {
        let mut state = fixture.base.read_state();
        let now = unix_now();
        let entry = state.get_mut(&vm).expect("record");
        entry["ttl_expires_at"] = json!(now - 100.0);
        entry["grace_until"] = json!(now - 50.0);
        entry["warned"] = json!(true);
        fixture.base.write_state(Value::Object(state));
    }

    let entered = Arc::new(Event::new());
    let finish = Arc::new(Event::new());
    let key_path = fixture.key_path(&vm);
    {
        let entered = Arc::clone(&entered);
        let finish = Arc::clone(&finish);
        let key_path = key_path.clone();
        fixture.set_custom_ssh(Arc::new(
            move |_ip, _user, _key_dir, _cmd, _stdin, _timeout| {
                entered.set();
                assert!(
                    finish.wait_timeout(Duration::from_secs(5)),
                    "blocked exec not released"
                );
                assert!(key_path.join("identity").is_file());
                Some((0, "done".to_string()))
            },
        ));
    }

    let command_service = Arc::clone(&fixture.service);
    let command_vm = vm.clone();
    let command = std::thread::spawn(move || {
        command_service
            .guest_exec(&command_vm, &json!({"argv": ["slow"]}))
            .expect("blocked exec");
    });
    assert!(entered.wait_timeout(Duration::from_secs(2)));

    let renewed = fixture
        .service
        .heartbeat(&vm, Some(&json!(1)))
        .expect("heartbeat");
    assert!(renewed["grace_until"].is_null());

    let gc_done = Arc::new(Event::new());
    let gc_service = Arc::clone(&fixture.service);
    let gc_done_thread = Arc::clone(&gc_done);
    let gc = std::thread::spawn(move || {
        gc_service.gc_once().expect("gc_once");
        gc_done_thread.set();
    });
    assert!(
        gc_done.wait_timeout(Duration::from_secs(2)),
        "GC waited on the in-flight command of a renewed lease"
    );

    finish.set();
    command.join().expect("command thread");
    gc.join().expect("gc thread");

    assert_eq!(
        fixture.base.read_state().get(&vm).expect("record")["state"],
        "running"
    );
    assert!(key_path.join("identity").is_file());
    assert!(fixture.service.host.vm_exists(&vm).expect("vm_exists"));
}

#[test]
fn heartbeat_refuses_after_teardown_has_committed() {
    let fixture = KeyFixture::new();
    let record = fixture.acquire("keys").expect("acquire");
    let vm = record["vm"].as_str().expect("vm").to_string();
    {
        let mut state = fixture.base.read_state();
        state.get_mut(&vm).expect("record")["state"] = json!("releasing");
        fixture.base.write_state(Value::Object(state));
    }
    let error = fixture
        .service
        .heartbeat(&vm, Some(&json!(1)))
        .expect_err("releasing lease cannot renew");
    assert!(error.to_string().contains("releasing"), "{error}");
}

// ---------------------------------------------------------------------------
// TestDaemonOwnership
// ---------------------------------------------------------------------------

/// The Python `TestDaemonOwnership` drives `svc.main()`: a kernel `flock` on
/// `daemon.lock` must stop HTTP bind and GC before they start. That entry point
/// is `serve()` in the `machine-service` binary crate; `machine-service-core` exposes no
/// `main`, no bind, and no GC start, so the test cannot live in this crate.
#[test]
#[ignore = "test_existing_kernel_owner_prevents_http_bind_and_gc_start: drives the daemon \
            entry point in the machine-service binary crate; machine-service-core has no main/bind/GC-start \
            seam and cannot assert this behavior"]
fn existing_kernel_owner_prevents_http_bind_and_gc_start() {
    // Intentionally empty: kept as an `#[ignore]` for traceability.
}
