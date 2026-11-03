//! The service object: resolved configuration, lease state, the image
//! catalog, and the console controller.
//!
//! The Python daemon used module-level globals. The Rust port keeps the same
//! values in one `Service` so the daemon can serve concurrently and tests can
//! build an isolated instance.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value};

use crate::catalog::{CatalogView, FileSource, ImageCatalog, NoSource, REBUILD_WAIT};
use crate::config::{log_to_file, Config};
use crate::console_api::{ConsoleConfig, ConsoleController, Manager};
use crate::error::{OpError, OpResult};
use crate::guest_link::LiveSession;
use crate::host::{BootProcess, Host, RealHost};
use crate::reentrant::ReentrantLock;
use crate::state::State;

/// The bound service instance.
pub struct Service {
    /// The resolved configuration.
    pub config: Config,
    /// The lease state store.
    pub state: State,
    /// The image catalog (`docs/arch/image-catalog.md`, IC-1): every reader
    /// of the image lines goes through it.
    pub catalog: Arc<ImageCatalog>,
    /// The console controller.
    pub consoles: Arc<dyn ConsoleController>,
    /// The host boundary: Tart, SSH, and the image catalog's source.
    pub host: Arc<dyn Host>,
    operation_locks: Mutex<HashMap<String, Arc<ReentrantLock>>>,
    /// Per-VM cancellation flags, set once a release has committed the
    /// `releasing` state. Guest operations observe them; see
    /// `docs/lifecycle-fixes.md` (V4).
    release_flags: Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// Consecutive GC passes in which a `running` record's VM was not running,
    /// keyed by VM name. Held in memory only; see `docs/lifecycle-fixes.md`.
    pub(crate) absent_passes: Mutex<HashMap<String, u32>>,
    /// Open guest sessions, keyed by VM name (`docs/arch/guest-session.md`,
    /// GS-1). A request takes its VM's session out while it runs.
    pub(crate) guest_sessions: Mutex<HashMap<String, LiveSession>>,
    /// The relay link, when `MACHINE_SERVICE_GUEST_LINK=relay` started it
    /// (`docs/arch/broker.md`).
    pub(crate) relay: std::sync::OnceLock<Arc<crate::relay::Relay>>,
}

impl Service {
    /// Build a service from a configuration and an optional trusted console
    /// configuration, backed by the real host.
    pub fn new(config: Config, console_config: Option<ConsoleConfig>) -> Self {
        let host: Arc<dyn Host> = Arc::new(RealHost::new(config.clone()));
        Self::with_host(config, console_config, host)
    }

    /// Build a service against an injected host implementation.
    ///
    /// Tests use this to substitute an in-memory Tart and SSH, mirroring the
    /// Python suite's `mock.patch.object` of the module-level helpers. The
    /// console controller is the real `Manager`; use [`Service::with_consoles`]
    /// to inject a test controller at the same time.
    pub fn with_host(
        config: Config,
        console_config: Option<ConsoleConfig>,
        host: Arc<dyn Host>,
    ) -> Self {
        Self::with_consoles(config, Arc::new(Manager::new(console_config)), host)
    }

    /// Build a service against injected host and console implementations.
    ///
    /// The console analogue of [`Service::with_host`]. Production only ever
    /// binds the real `Manager`, so the seam cannot relax any validation.
    pub fn with_consoles(
        config: Config,
        consoles: Arc<dyn ConsoleController>,
        host: Arc<dyn Host>,
    ) -> Self {
        let fingerprint = config.fingerprint();
        let state = State::new(
            config.state_dir.clone(),
            config.state_file.clone(),
            config.lock_file.clone(),
            fingerprint,
        );
        let source = host
            .catalog_source()
            .unwrap_or_else(|| match &config.pilot {
                Some(pilot) => Arc::new(FileSource::new(pilot.join("images"))),
                None => Arc::new(NoSource),
            });
        let log_file = config.log_file.clone();
        let catalog = ImageCatalog::new(
            source,
            Arc::new(move |message: &str| log_to_file(&log_file, message)),
            REBUILD_WAIT,
        );
        Self {
            config,
            state,
            catalog,
            consoles,
            host,
            operation_locks: Mutex::new(HashMap::new()),
            release_flags: Mutex::new(HashMap::new()),
            absent_passes: Mutex::new(HashMap::new()),
            guest_sessions: Mutex::new(HashMap::new()),
            relay: std::sync::OnceLock::new(),
        }
    }

    /// Read the image catalog, rebuilding it first when its files changed
    /// (IC-3).
    pub fn image_catalog(&self) -> CatalogView {
        self.catalog.read()
    }

    /// Resolve one image line from the catalog, or fail with the Python
    /// error text.
    ///
    /// The record carries the line's guest password from the same build, for
    /// the lease bootstrap only; the snapshot itself holds none (IC-1).
    pub fn line_cfg(&self, image: &str) -> OpResult<Value> {
        let view = self.image_catalog();
        let snapshot = view
            .snapshot()
            .ok_or_else(|| OpError::new(view.status.diagnostic()))?;
        match snapshot.lines.get(image) {
            Some(line) => {
                let mut record = line.to_json();
                record["ssh_pass"] =
                    Value::String(view.guest_password(image).unwrap_or_default().to_string());
                Ok(record)
            }
            None => {
                let available = snapshot
                    .lines
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ");
                Err(OpError::new(format!(
                    "unknown image '{image}' (available: {available})"
                )))
            }
        }
    }

    /// Resolve an env pack name to its directory through the host's home.
    /// A second instance reads only the packs in its own configuration
    /// folder, never the operator's (DC-1xx-LIVE-5).
    pub fn pack_dir(&self, name: &str) -> OpResult<Option<PathBuf>> {
        if let Some(marker) = crate::second::get() {
            return crate::packs::pack_dir_in(&marker.packs_dir, name);
        }
        let home = self.host.home_dir().unwrap_or_else(|| PathBuf::from("/"));
        crate::packs::pack_dir(&home, name)
    }

    /// Inject a credential pack into a running guest through the host.
    pub fn inject_pack(
        &self,
        ip: &str,
        cfg: &Value,
        pack: &std::path::Path,
        key_dir: &std::path::Path,
    ) -> OpResult<bool> {
        crate::packs::inject_pack(self.host.as_ref(), &self.config, ip, cfg, pack, key_dir)
    }

    /// Detect a refused boot through the host's settle window.
    pub fn boot_refused(
        &self,
        process: &mut dyn BootProcess,
        vm: &str,
        settle_s: Option<f64>,
    ) -> OpResult<()> {
        self.host.boot_refused(process, vm, settle_s)
    }

    /// Write a timestamped line to the service log.
    pub fn log(&self, message: &str) {
        log_to_file(&self.config.log_file, message);
    }

    /// Load the trusted console configuration, mapping failure to the daemon's
    /// startup error text.
    pub fn load_console_config() -> OpResult<Option<ConsoleConfig>> {
        crate::console_api::load_config(None, None)
            .map_err(|_| OpError::new("Invalid console configuration; service not started"))
    }

    /// Return the per-VM operation lock, mirroring Python's
    /// `_operation_locks.setdefault(vm, threading.RLock())`.
    ///
    /// The lock is reentrant because `release` re-enters through
    /// `begin_release`, and a failed `acquire` rolls back through the same
    /// path while still holding it.
    pub fn operation_lock(&self, vm: &str) -> Arc<ReentrantLock> {
        let mut locks = self
            .operation_locks
            .lock()
            .expect("operation lock map is never poisoned");
        locks
            .entry(vm.to_string())
            .or_insert_with(|| Arc::new(ReentrantLock::new()))
            .clone()
    }

    /// Return the per-VM release flag that sits beside the operation lock.
    ///
    /// A release sets it after committing the `releasing` state and before
    /// taking the operation lock. Exec, push, and pull run their subprocesses
    /// under [`crate::proc::with_cancellation`] with this flag, so a release
    /// kills the running subprocess instead of waiting for it. The flag is
    /// never cleared: VM names are unique per lease, and a `releasing` record
    /// refuses every new operation.
    pub fn release_flag(&self, vm: &str) -> Arc<AtomicBool> {
        let mut flags = self
            .release_flags
            .lock()
            .expect("release flag map is never poisoned");
        flags
            .entry(vm.to_string())
            .or_insert_with(|| Arc::new(AtomicBool::new(false)))
            .clone()
    }

    /// Fetch a lease record and verify its environment fingerprint.
    pub fn get_record(&self, vm: &str) -> OpResult<Value> {
        let data = self.state.read()?;
        let record = data
            .get("vms")
            .and_then(|vms| vms.get(vm))
            .cloned()
            .ok_or_else(|| OpError::new(format!("unknown VM: {vm}")))?;
        self.state.require_lease_environment(&record)?;
        Ok(record)
    }

    /// Validate and return the lease key directory for a record.
    pub fn key_directory(&self, record: &Value) -> OpResult<PathBuf> {
        // NAT and internet guests are reached over the lease's SSH key; any
        // other recorded network (control-only) has no ownership auditor.
        if acquisition_options::GuestNetwork::of_record(record).is_none() {
            return Err(OpError::new(
                "control-only ownership auditor unavailable; execution/transfers denied",
            ));
        }
        let keyed = record.get("ssh_auth").and_then(Value::as_str) == Some("lease-key")
            && record.get("ssh_verified") == Some(&Value::Bool(true));
        if !keyed {
            return Err(OpError::new(
                "Lease has no verified SSH key; release/reacquire it with key provisioning (no password fallback)",
            ));
        }
        let vm = record.get("vm").and_then(Value::as_str).unwrap_or_default();
        lease_keys::directory(&self.config.state_dir, vm).map_err(|_| {
            OpError::new(
                "Lease SSH credentials unavailable; retained lease requires operator recovery",
            )
        })
    }

    /// Allow catalog rebuilds and build the first snapshot (IC-4). The
    /// daemon calls this once it owns `daemon.lock`, before it serves; no
    /// `line.conf` is executed for this service before then.
    ///
    /// A failure leaves the catalog `unavailable` but armed, so the next
    /// change to the files is tried at once.
    pub fn initialize_image_catalog(&self) -> OpResult<()> {
        let view = self.catalog.start();
        match view.snapshot() {
            Some(_) => Ok(()),
            None => Err(OpError::new(view.status.diagnostic())),
        }
    }

    /// The image-to-`{kind, base_vm}` associations `/applications` pairs
    /// with the portable inventories, from the current catalog.
    pub fn application_associations(&self) -> OpResult<Map<String, Value>> {
        let view = self.image_catalog();
        let snapshot = view
            .snapshot()
            .ok_or_else(|| OpError::new(view.status.diagnostic()))?;
        Ok(snapshot
            .lines
            .iter()
            .map(|(image, line)| {
                (
                    image.clone(),
                    serde_json::json!({"kind": line.kind, "base_vm": line.base_vm}),
                )
            })
            .collect())
    }
}
