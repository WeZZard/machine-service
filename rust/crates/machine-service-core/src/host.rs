//! The host boundary: every interaction with Tart, SSH, and the guest passes
//! through one [`Host`] implementation.
//!
//! The Python daemon reached these through module-level functions, and the test
//! suite replaced them with `mock.patch.object`. The Rust port keeps the same
//! seam as an explicit trait: [`RealHost`] performs the real work, and tests
//! substitute an in-memory implementation without touching production code.
//!
//! Helpers that are pure derivations of `tart` (`tart_list`, `vm_exists`,
//! `vm_running`, `vm_ip`, `boot_refused`) are provided methods so an
//! implementation only has to supply the primitive it genuinely cannot fake.

use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::Arc;
use std::time::{Duration, Instant};

use acquisition_options::GuestNetwork;
use serde_json::Value;

use crate::catalog::CatalogSource;
use crate::config::Config;
use crate::error::{OpError, OpResult};
use crate::guest_link::GuestChannel;

/// A booting guest process, observed while the hypervisor settles.
///
/// The Python fixture replaced `subprocess.Popen` with an object exposing
/// `poll()`. This trait is that same contract.
pub trait BootProcess: Send {
    /// `None` while the process is alive, `Some(rc)` once it has exited.
    fn poll(&mut self) -> Option<i32>;
}

/// The real boot process: a detached `tart run` child.
struct ChildBootProcess {
    child: Child,
}

impl BootProcess for ChildBootProcess {
    fn poll(&mut self) -> Option<i32> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(status.code().unwrap_or(-1)),
            Ok(None) => None,
            Err(_) => Some(-1),
        }
    }
}

/// A listener on this host that an `internet` guest must not reach (DC-14).
pub trait ProbeListener: Send {
    /// The port it listens on, on all of the host's addresses.
    fn port(&self) -> u16;
    /// Whether a connection has arrived.
    fn accepted(&mut self) -> bool;
}

/// The real probe listener: a non-blocking TCP listener on `0.0.0.0`.
struct TcpProbeListener {
    listener: std::net::TcpListener,
}

impl ProbeListener for TcpProbeListener {
    fn port(&self) -> u16 {
        self.listener
            .local_addr()
            .map(|address| address.port())
            .unwrap_or(0)
    }

    fn accepted(&mut self) -> bool {
        self.listener.accept().is_ok()
    }
}

/// Raw captured output of one `tart` invocation.
#[derive(Debug, Clone, Default)]
pub struct TartOutput {
    /// The exit code, or `-1` when the process was signalled.
    pub code: i32,
    /// Captured standard output.
    pub stdout: Vec<u8>,
    /// Captured standard error.
    pub stderr: Vec<u8>,
}

/// The host operations the service performs.
///
/// Implementations must be safe to share across the daemon's request threads.
pub trait Host: Send + Sync {
    /// Run `tart` and capture output; `check` mirrors Python `check=True`.
    fn tart(&self, args: &[String], check: bool, timeout_s: u64) -> OpResult<TartOutput>;

    /// Run a remote command. `None` on a connection or process failure,
    /// matching the Python `_ssh` helper.
    fn ssh(
        &self,
        ip: &str,
        user: &str,
        key_dir: &Path,
        remote_cmd: &str,
        stdin: Option<Vec<u8>>,
        timeout_s: u64,
    ) -> Option<(i32, String)>;

    /// Copy one path with `scp -r`. `None` on a process failure.
    fn scp(
        &self,
        ip: &str,
        user: &str,
        key_dir: &Path,
        src: &str,
        dst: &str,
        timeout_s: u64,
    ) -> Option<(i32, String)>;

    /// Start `remote_cmd` in the guest with its standard input and output
    /// carrying a guest session (`docs/arch/guest-session.md`, GS-2).
    fn open_guest_channel(
        &self,
        ip: &str,
        user: &str,
        key_dir: &Path,
        remote_cmd: &str,
    ) -> OpResult<GuestChannel> {
        let _ = (ip, user, key_dir, remote_cmd);
        Err(OpError::new(
            "guest session channels are unavailable on this host",
        ))
    }

    /// The guest session agent for a guest of `kind` (GS-3).
    fn session_agent(&self, kind: &str) -> OpResult<Vec<u8>> {
        crate::guest_link::agent_bytes_for(kind)
    }

    /// Wait for a VM to report an IP within `timeout_s`.
    fn wait_ip(&self, name: &str, timeout_s: u64) -> Option<String>;

    /// Wait for key-only SSH readiness.
    fn wait_ssh(&self, ip: &str, user: &str, key_dir: &Path, timeout_s: u64) -> bool;

    /// Prove separate key-only push and pull connections and byte integrity.
    fn verify_transfer(&self, ip: &str, user: &str, key_dir: &Path) -> OpResult<()>;

    /// Install lease credentials in the guest.
    fn bootstrap(
        &self,
        ip: &str,
        user: &str,
        password: &str,
        key_dir: &Path,
        timeout_s: f64,
    ) -> OpResult<()>;

    /// Spawn `tart run <vm> --no-graphics` detached, with `--net-softnet` for
    /// a guest on the `internet` network.
    fn spawn_run(&self, vm: &str, network: GuestNetwork) -> OpResult<Box<dyn BootProcess>>;

    /// The Softnet executable Tart would run for a guest on the `internet`
    /// network, or the reason, naming Softnet, that Tart cannot run it.
    ///
    /// A host that cannot tell reports Softnet unavailable, so an `internet`
    /// acquisition fails rather than falling back to NAT.
    fn softnet(&self) -> Result<PathBuf, String> {
        Err("Softnet unavailable: this host does not run Tart (no NAT fallback)".to_string())
    }

    /// The installed guest firewall, or the reason, with its install
    /// command, that an `internet` lease cannot be offered.
    fn guest_firewall(&self) -> Result<crate::isolation::GuestFirewall, String> {
        Err("guest firewall unknown on this host (no NAT fallback)".to_string())
    }

    /// The table file this service writes guest addresses to, whether or not
    /// the firewall is installed; used to prune entries.
    fn firewall_table(&self) -> PathBuf {
        let variable = std::env::var(crate::isolation::TABLE_FILE_VARIABLE).ok();
        crate::isolation::table_file(variable.as_deref())
    }

    /// The macOS DNS-over-TLS forwarder to upload into a guest (DC-16).
    fn dot_forwarder(&self) -> OpResult<Vec<u8>> {
        let variable = std::env::var(crate::isolation::FORWARDER_VARIABLE).ok();
        let path = crate::isolation::forwarder_path(variable.as_deref());
        std::fs::read(&path).map_err(|error| {
            OpError::new(format!(
                "DNS-over-TLS forwarder unavailable at {}: {error}; stage {} beside machine-service (no NAT fallback)",
                path.display(),
                crate::isolation::FORWARDER
            ))
        })
    }

    /// Open a listener on all of this host's addresses, on an ephemeral port.
    fn open_probe(&self) -> OpResult<Box<dyn ProbeListener>> {
        Err(OpError::new(
            "guest isolation probes are unavailable on this host",
        ))
    }

    /// The Tart executable this instance would run, or why there is none.
    ///
    /// An instance without Tart creates no virtual machine: it reports no
    /// `virtual` kind, lists no offer and refuses an image acquisition
    /// (MM-1, MG-4 step 5, AC-38, DC-66). Hosts that fake Tart have it.
    fn tart_executable(&self) -> Result<PathBuf, String> {
        Ok(PathBuf::from("tart"))
    }

    /// The boot-settle window, in seconds.
    fn boot_settle_s(&self) -> f64;

    /// Advisory host-wide Virtualization.framework guest count. `None` when the
    /// gauge is unavailable; never fails.
    fn host_macos_guests(&self) -> Option<usize>;

    /// The image catalog's inputs and strict builder (IC-2, IC-4), or `None`
    /// for the `images/*/line.conf` files of the configured pilot-images
    /// checkout.
    fn catalog_source(&self) -> Option<Arc<dyn CatalogSource>> {
        None
    }

    /// The caller's home directory, used for credential packs and the Tart
    /// store. Defaults to `$HOME`.
    fn home_dir(&self) -> Option<PathBuf> {
        std::env::var_os("HOME").map(PathBuf::from)
    }

    /// The Tart store root, whose `vms` child holds VM metadata.
    fn tart_store_root(&self) -> PathBuf {
        if let Ok(explicit) = std::env::var("TART_HOME") {
            return PathBuf::from(explicit);
        }
        self.home_dir()
            .unwrap_or_else(|| PathBuf::from("/"))
            .join(".tart")
    }

    // ---------------------------------------------------------------- derived

    /// `[(name, state)]` for local VMs, parsed from `tart list`.
    fn tart_list(&self) -> OpResult<Vec<(String, String)>> {
        let output = self.tart(&["list".to_string()], true, 120)?;
        let text = String::from_utf8_lossy(&output.stdout);
        let mut rows = Vec::new();
        for line in crate::python::splitlines(&text).into_iter().skip(1) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 5 && parts[0] == "local" {
                rows.push((parts[1].to_string(), parts[parts.len() - 1].to_string()));
            }
        }
        Ok(rows)
    }

    /// Whether a VM name exists locally.
    fn vm_exists(&self, name: &str) -> OpResult<bool> {
        Ok(self
            .tart_list()?
            .iter()
            .any(|(candidate, _)| candidate == name))
    }

    /// Whether a VM exists locally and is running.
    fn vm_running(&self, name: &str) -> OpResult<bool> {
        Ok(self
            .tart_list()?
            .iter()
            .any(|(candidate, state)| candidate == name && state == "running"))
    }

    /// The first reported IP for a VM, or `None`.
    fn vm_ip(&self, name: &str) -> Option<String> {
        let output = self
            .tart(&["ip".to_string(), name.to_string()], false, 30)
            .ok()?;
        crate::python::splitlines(&String::from_utf8_lossy(&output.stdout))
            .into_iter()
            .next()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
    }

    /// The log of `vm`'s `tart run`, empty when it cannot be read.
    fn run_log(&self, vm: &str) -> String {
        std::fs::read_to_string(crate::tart::run_log_path(vm)).unwrap_or_default()
    }

    /// Detect a guest the hypervisor refused to start within the settle window.
    ///
    /// A `tart run` process that exits during the window is a refusal; a process
    /// that survives the window is booting.
    fn boot_refused(
        &self,
        process: &mut dyn BootProcess,
        vm: &str,
        settle_s: Option<f64>,
    ) -> OpResult<()> {
        let settle = settle_s.unwrap_or_else(|| self.boot_settle_s());
        let deadline = Instant::now() + Duration::from_secs_f64(settle.max(0.0));
        while Instant::now() < deadline {
            if let Some(code) = process.poll() {
                // The hypervisor's own refusal for the host's macOS VM limit,
                // such as when another program's guests hold the slots, is
                // machine-service's capacity refusal, so a client may wait (DC-18).
                let log = self.run_log(vm);
                if let Some(line) = log
                    .lines()
                    .find(|line| line.contains("exceeds the system limit"))
                {
                    return Err(OpError::new(format!(
                        "macOS VM limit reached (the host refused to start {vm}: {}); \
release one first, or wait for another program's macOS guest to stop",
                        line.trim()
                    )));
                }
                return Err(OpError::new(format!(
                    "{vm} refused to start (boot process exited rc={code} within {}s) — \
for macOS images the host-wide 2-VM limit may be consumed by another \
Virtualization.framework user; check 'GET /images' capacity and /tmp/tart-run-{vm}.log",
                    py_number(settle)
                )));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            std::thread::sleep(remaining.min(Duration::from_millis(500)));
        }
        Ok(())
    }
}

/// Format a number the way Python renders a float default and an int default.
///
/// `10.0` prints as `10` and `0.3` as `0.3`, matching the Python message that
/// interpolates an `int` settle window and the tests' `float` overrides.
fn py_number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// The production host: real Tart, real SSH, real filesystem.
#[derive(Debug, Clone)]
pub struct RealHost {
    config: Config,
}

impl RealHost {
    /// Bind a host to a resolved configuration.
    pub fn new(config: Config) -> Self {
        Self { config }
    }
}

impl Host for RealHost {
    fn tart(&self, args: &[String], check: bool, timeout_s: u64) -> OpResult<TartOutput> {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = crate::tart::tart(&self.config, &refs, check, timeout_s)?;
        Ok(TartOutput {
            code: output.status.code().unwrap_or(-1),
            stdout: output.stdout,
            stderr: output.stderr,
        })
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
        crate::ssh::ssh(
            &self.config,
            ip,
            user,
            key_dir,
            remote_cmd,
            stdin,
            timeout_s,
        )
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
        crate::ssh::scp(&self.config, ip, user, key_dir, src, dst, timeout_s)
    }

    fn open_guest_channel(
        &self,
        ip: &str,
        user: &str,
        key_dir: &Path,
        remote_cmd: &str,
    ) -> OpResult<GuestChannel> {
        crate::guest_link::ssh_channel(ip, user, key_dir, remote_cmd)
    }

    fn wait_ip(&self, name: &str, timeout_s: u64) -> Option<String> {
        crate::tart::wait_ip(&self.config, name, timeout_s)
    }

    fn wait_ssh(&self, ip: &str, user: &str, key_dir: &Path, timeout_s: u64) -> bool {
        crate::ssh::wait_ssh(&self.config, ip, user, key_dir, timeout_s)
    }

    fn verify_transfer(&self, ip: &str, user: &str, key_dir: &Path) -> OpResult<()> {
        crate::ssh::verify_transfer(&self.config, ip, user, key_dir, &self.config.state_dir)
    }

    fn bootstrap(
        &self,
        ip: &str,
        user: &str,
        password: &str,
        key_dir: &Path,
        timeout_s: f64,
    ) -> OpResult<()> {
        lease_keys::bootstrap(ip, user, password, key_dir, timeout_s)
            .map_err(|error| OpError::new(error.to_string()))
    }

    fn spawn_run(&self, vm: &str, network: GuestNetwork) -> OpResult<Box<dyn BootProcess>> {
        let child = crate::tart::spawn_run(&self.config, vm, network)?;
        Ok(Box::new(ChildBootProcess { child }))
    }

    fn softnet(&self) -> Result<PathBuf, String> {
        crate::network::softnet_executable(crate::tart::tart_path(&self.config).as_deref())
    }

    fn guest_firewall(&self) -> Result<crate::isolation::GuestFirewall, String> {
        let variable = std::env::var(crate::isolation::ANCHOR_FILE_VARIABLE).ok();
        crate::isolation::check_guest_firewall(
            &crate::isolation::anchor_file(variable.as_deref()),
            &self.firewall_table(),
        )
    }

    fn open_probe(&self) -> OpResult<Box<dyn ProbeListener>> {
        let listener = std::net::TcpListener::bind(("0.0.0.0", 0))?;
        listener.set_nonblocking(true)?;
        Ok(Box::new(TcpProbeListener { listener }))
    }

    fn tart_executable(&self) -> Result<PathBuf, String> {
        crate::tart::find_executable(&self.config)
    }

    fn boot_settle_s(&self) -> f64 {
        crate::tart::BOOT_SETTLE_S as f64
    }

    fn host_macos_guests(&self) -> Option<usize> {
        crate::tart::host_macos_guests()
    }

    fn tart_store_root(&self) -> PathBuf {
        if let Some(environment) = &self.config.environment {
            if let Some(path) = environment
                .get("profile")
                .and_then(|profile| profile.get("tartHome"))
                .and_then(Value::as_str)
            {
                return PathBuf::from(path);
            }
        }
        if let Ok(explicit) = std::env::var("TART_HOME") {
            return PathBuf::from(explicit);
        }
        self.home_dir()
            .unwrap_or_else(|| PathBuf::from("/"))
            .join(".tart")
    }
}
