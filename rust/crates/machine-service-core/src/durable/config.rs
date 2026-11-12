//! Durable-machine configuration: exposure, the instance ID and the paths a
//! durable lease uses (`docs/arch/machines.md` MM-1, MM-2;
//! `docs/arch/durable-machine.md` DM-1, DM-2; `docs/arch/agent-keys.md` AK-1).
//!
//! Every value is resolved once at startup into a [`DurableConfig`], so the
//! lease logic takes explicit inputs and tests build an isolated instance
//! without process-global mutation.

use std::ffi::CStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The instance's local exposure setting (DC-39). Unset or `off` keeps the
/// Mac unexposed; `on` exposes it as a durable machine.
pub const EXPOSE_VARIABLE: &str = "MACHINE_SERVICE_EXPOSE_DURABLE";
/// The instance's configuration folder, `~/.config/machine-service` by
/// default (AK-1). A second instance for tests names its own.
pub const CONFIG_DIR_VARIABLE: &str = "MACHINE_SERVICE_CONFIG_DIR";
/// A debug-build-only relocation of the host-wide lease lock, so the
/// component tests of `cargo test --workspace` never share the real lock
/// with one another or with an installed instance. A release build refuses
/// to start while it is set, because the lock must be host-wide (DM-2).
pub const LOCK_DIR_VARIABLE: &str = "MACHINE_SERVICE_DURABLE_LOCK_DIR";
/// The free-disk warning level in GiB (HY-6, DC-76). Unset means
/// [`DEFAULT_DISK_WARN_GIB`]; `0` turns the warning off; anything that is
/// not a number stops the daemon, so a typo never silences the warning.
pub const DISK_WARN_VARIABLE: &str = "MACHINE_SERVICE_DISK_WARN_GB";
/// The default free-disk warning level: 30 GiB, about the space one engine
/// install takes on a 228 GiB system volume (HY-6, DC-76).
pub const DEFAULT_DISK_WARN_GIB: f64 = 30.0;
/// The folder of the host-wide lease lock that every instance on the Mac
/// shares, whatever its state directory (DM-2).
pub const HOST_LOCK_DIR: &str = "/var/tmp/com.wezzard.machine-service";

/// Time a lease's processes get between `SIGTERM` and `SIGKILL` (DM-7, HY-3).
pub const STOP_GRACE: Duration = Duration::from_secs(10);
/// Time each inventory part gets (HY-4).
pub const INVENTORY_TIMEOUT: Duration = Duration::from_secs(60);
/// Time each read-only fact command gets (MM-4).
pub const FACT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a released lease's record stays readable (DM-7, HY-4).
pub const RECORD_RETENTION_S: f64 = 30.0 * 24.0 * 3600.0;
/// How long a kept workspace stays before GC removes it (DM-7).
pub const KEPT_WORKSPACE_RETENTION_S: f64 = 7.0 * 24.0 * 3600.0;
/// How often the expensive facts are refreshed without a lease event (MM-4).
pub const DAILY_REFRESH_S: f64 = 24.0 * 3600.0;

/// The resolved durable-machine configuration of one instance.
#[derive(Debug, Clone)]
pub struct DurableConfig {
    /// Whether this instance exposes its Mac as a durable machine (DC-39).
    pub exposed: bool,
    /// The instance ID, which is also the durable machine's name (MM-2).
    pub instance: String,
    /// The service account's home folder. A session keeps it as `HOME`
    /// (DM-1).
    pub home: PathBuf,
    /// The service account's user name.
    pub user: String,
    /// The service account's numeric user ID.
    pub uid: u32,
    /// The service state directory; leases keep their records under
    /// `leases/<lease_id>/` here (DM-1).
    pub state_dir: PathBuf,
    /// The service log.
    pub log_file: PathBuf,
    /// The instance's configuration folder (AK-1).
    pub config_dir: PathBuf,
    /// The folder of the host-wide lease lock (DM-2).
    pub lock_dir: PathBuf,
    /// The folder that holds every lease's workspace (DM-1, HY-2).
    pub workspace_root: PathBuf,
    /// The login shell whose environment a session starts from (DM-4).
    pub login_shell: PathBuf,
    /// The grace period after TTL expiry, in hours (DM-6).
    pub grace_hours: f64,
    /// Time between `SIGTERM` and `SIGKILL` at release (DM-7).
    pub stop_grace: Duration,
    /// Time each inventory part gets (HY-4).
    pub inventory_timeout: Duration,
    /// Time each fact command gets (MM-4).
    pub fact_timeout: Duration,
    /// The command that probes the GPU and the privacy permissions, which
    /// must run as a child of the service so macOS attributes the grants to
    /// it (MM-3). `None` reports both sections as unknown.
    pub probe: Option<Vec<String>>,
    /// The CI runner's folder, which hygiene never touches (HY-5).
    pub ci_root: PathBuf,
    /// Free bytes below which the machine record warns `low free disk`, or
    /// `None` when the warning is off (HY-6).
    pub disk_warn_bytes: Option<u64>,
}

impl DurableConfig {
    /// Resolve the configuration from the process environment.
    ///
    /// `probe` is the host-probe command the daemon offers (its own
    /// executable with `--probe-host`).
    pub fn from_environment(
        state_dir: &Path,
        log_file: &Path,
        probe: Option<Vec<String>>,
    ) -> Result<Self, String> {
        let exposed = parse_exposure(std::env::var(EXPOSE_VARIABLE).ok().as_deref())?;
        let (user, uid, password_home) = account();
        let home = std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or(password_home)
            .unwrap_or_else(|| PathBuf::from("/"));
        let config_dir = std::env::var_os(CONFIG_DIR_VARIABLE)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config/machine-service"));
        let lock_dir = lock_dir_from(std::env::var_os(LOCK_DIR_VARIABLE).map(PathBuf::from))?;
        // A test build touches no installed folder and never the host-wide
        // lease lock that an installed instance holds (DC-251).
        service_layout::test_build::owned_path(CONFIG_DIR_VARIABLE, &config_dir)?;
        if service_layout::test_build::is_isolated() && lock_dir == Path::new(HOST_LOCK_DIR) {
            return Err(format!(
                "this is a test build, which never takes the host-wide lease lock {HOST_LOCK_DIR}; a test names its own {LOCK_DIR_VARIABLE}"
            ));
        }
        let disk_warn_bytes = parse_disk_warn(std::env::var(DISK_WARN_VARIABLE).ok().as_deref())?;
        Ok(Self {
            exposed,
            instance: read_instance_name()?,
            workspace_root: home.join("machine-service/leases"),
            ci_root: home.join("actions-runner"),
            home,
            user,
            uid,
            state_dir: state_dir.to_path_buf(),
            log_file: log_file.to_path_buf(),
            config_dir,
            lock_dir,
            login_shell: service_layout::tools::path(service_layout::tools::Tool::Zsh)?,
            grace_hours: crate::ops::parse_grace_hours(
                std::env::var(crate::ops::GRACE_HOURS_VARIABLE)
                    .ok()
                    .as_deref(),
            )?,
            stop_grace: STOP_GRACE,
            inventory_timeout: INVENTORY_TIMEOUT,
            fact_timeout: FACT_TIMEOUT,
            probe,
            disk_warn_bytes,
        })
    }

    /// The folder that holds the agent key files and `endpoints.json`
    /// (AK-1, AK-2).
    pub fn agents_dir(&self) -> PathBuf {
        self.config_dir.join("agents")
    }

    /// The service-state folder of one lease (DM-1).
    pub fn lease_dir(&self, lease_id: &str) -> PathBuf {
        self.state_dir.join("leases").join(lease_id)
    }

    /// The workspace of one lease (DM-1, HY-2).
    pub fn workspace(&self, lease_id: &str) -> PathBuf {
        self.workspace_root.join(lease_id)
    }
}

/// Parse the exposure setting: unset, empty or `off` is off, `on` is on, and
/// any other value is refused so a typo never exposes a Mac by accident or
/// hides one silently (DC-39).
pub fn parse_exposure(value: Option<&str>) -> Result<bool, String> {
    match value {
        None | Some("") | Some("off") => Ok(false),
        Some("on") => Ok(true),
        Some(other) => Err(format!(
            "{EXPOSE_VARIABLE} must be on or off; got {other:?}"
        )),
    }
}

/// Parse the free-disk warning level (HY-6, DC-76): unset or empty is the
/// default, `0` is off, a positive number of GiB is that level, and
/// anything else is refused.
pub fn parse_disk_warn(value: Option<&str>) -> Result<Option<u64>, String> {
    let gib = match value.map(str::trim) {
        None | Some("") => DEFAULT_DISK_WARN_GIB,
        Some(text) => match text.parse::<f64>() {
            Ok(number) if number.is_finite() && number >= 0.0 => number,
            _ => {
                return Err(format!(
                    "{DISK_WARN_VARIABLE} must be a number of GiB, or 0 to turn the warning off; got {text:?}"
                ))
            }
        },
    };
    Ok((gib > 0.0).then_some((gib * 1024.0 * 1024.0 * 1024.0) as u64))
}

/// The lock folder: the host-wide one, or, in a debug build only, the
/// relocation a component test names.
fn lock_dir_from(requested: Option<PathBuf>) -> Result<PathBuf, String> {
    match requested.filter(|path| !path.as_os_str().is_empty()) {
        None => Ok(PathBuf::from(HOST_LOCK_DIR)),
        Some(path) => {
            if cfg!(debug_assertions) {
                Ok(path)
            } else {
                Err(format!(
                    "{LOCK_DIR_VARIABLE} is honoured only by debug builds; the durable lease lock \
is host-wide at {HOST_LOCK_DIR}"
                ))
            }
        }
    }
}

/// Read the live label from the single name-source seam, or macOS (MI-2).
/// An unreadable or empty test source never falls back to the real Mac's name.
pub fn read_instance_name() -> Result<String, String> {
    if let Some(path) =
        std::env::var_os("MACHINE_SERVICE_NAME_FILE").filter(|path| !path.is_empty())
    {
        let name = std::fs::read_to_string(&path)
            .map_err(|error| format!("cannot read MACHINE_SERVICE_NAME_FILE: {error}"))?;
        let name = name.trim();
        if name.is_empty() || name.contains(['/', '\n', '\r', '\0']) {
            return Err(
                "MACHINE_SERVICE_NAME_FILE must contain one nonempty host name".to_string(),
            );
        }
        return Ok(name.to_string());
    }
    // A test build never reads the Mac's own name, which another Mac on
    // the network could match (DC-255).
    if service_layout::test_build::is_isolated() {
        return Err(
            "this is a test build, which reads its name only from MACHINE_SERVICE_NAME_FILE"
                .to_string(),
        );
    }
    Ok(instance_id())
}

/// The Mac's local host name (MM-2), falling back to the short host name.
pub fn instance_id() -> String {
    let scutil = service_layout::tools::command(service_layout::tools::Tool::Scutil).and_then(
        |mut command| {
            command
                .args(["--get", "LocalHostName"])
                .stdin(std::process::Stdio::null())
                .output()
                .map_err(|error| error.to_string())
        },
    );
    if let Ok(output) = scutil {
        let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if output.status.success() && !name.is_empty() {
            return name;
        }
    }
    let mut buffer = [0 as libc::c_char; 256];
    // SAFETY: `gethostname` writes at most `buffer.len()` bytes into the
    // caller-provided buffer, which is zeroed so the result is terminated.
    let rc = unsafe { libc::gethostname(buffer.as_mut_ptr(), buffer.len() - 1) };
    if rc == 0 {
        // SAFETY: the buffer is NUL-terminated (zeroed, written short).
        let name = unsafe { CStr::from_ptr(buffer.as_ptr()) }
            .to_string_lossy()
            .to_string();
        let short = name.split('.').next().unwrap_or_default().to_string();
        if !short.is_empty() {
            return short;
        }
    }
    "localhost".to_string()
}

/// The effective user's name, numeric ID and password-database home.
pub fn account() -> (String, u32, Option<PathBuf>) {
    // SAFETY: `geteuid` only reads process state.
    let uid = unsafe { libc::geteuid() };
    // SAFETY: `getpwuid` returns a pointer into static storage that is read
    // immediately and never retained.
    unsafe {
        let entry = libc::getpwuid(uid);
        if entry.is_null() {
            return (uid.to_string(), uid, None);
        }
        let name = if (*entry).pw_name.is_null() {
            uid.to_string()
        } else {
            CStr::from_ptr((*entry).pw_name)
                .to_string_lossy()
                .to_string()
        };
        let home = if (*entry).pw_dir.is_null() {
            None
        } else {
            Some(PathBuf::from(
                CStr::from_ptr((*entry).pw_dir)
                    .to_string_lossy()
                    .to_string(),
            ))
        };
        (name, uid, home)
    }
}

/// The user name of a numeric user ID, or the number itself.
pub fn user_name(uid: u32) -> String {
    // SAFETY: as in `account`.
    unsafe {
        let entry = libc::getpwuid(uid);
        if entry.is_null() || (*entry).pw_name.is_null() {
            return uid.to_string();
        }
        CStr::from_ptr((*entry).pw_name)
            .to_string_lossy()
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// HY-6 (TL-1): unset is 30 GiB, 0 is off, and a typo is refused.
    #[test]
    fn hy6_disk_warning_level() {
        assert_eq!(parse_disk_warn(None), Ok(Some(30 * 1024 * 1024 * 1024)));
        assert_eq!(parse_disk_warn(Some("")), Ok(Some(30 * 1024 * 1024 * 1024)));
        assert_eq!(parse_disk_warn(Some("0")), Ok(None));
        assert_eq!(parse_disk_warn(Some("1.5")), Ok(Some(1_610_612_736)));
        for bad in ["thirty", "-1", "nan", "inf"] {
            let error = parse_disk_warn(Some(bad)).unwrap_err();
            assert!(error.contains(DISK_WARN_VARIABLE), "{error}");
        }
    }

    /// AC-43: exposure is off unless the operator turns it on, and a value
    /// that is neither on nor off is refused rather than guessed.
    #[test]
    fn ac43_exposure_is_off_unless_turned_on() {
        assert_eq!(parse_exposure(None), Ok(false));
        assert_eq!(parse_exposure(Some("")), Ok(false));
        assert_eq!(parse_exposure(Some("off")), Ok(false));
        assert_eq!(parse_exposure(Some("on")), Ok(true));
        let error = parse_exposure(Some("yes")).unwrap_err();
        assert!(error.contains("MACHINE_SERVICE_EXPOSE_DURABLE"), "{error}");
    }

    /// DM-2: without a relocation the lock is the host-wide one.
    #[test]
    fn dm2_lock_is_host_wide_by_default() {
        assert_eq!(lock_dir_from(None).unwrap(), PathBuf::from(HOST_LOCK_DIR));
        assert_eq!(
            lock_dir_from(Some(PathBuf::new())).unwrap(),
            PathBuf::from(HOST_LOCK_DIR)
        );
        // Tests run debug builds, where the relocation is honoured.
        assert_eq!(
            lock_dir_from(Some(PathBuf::from("/tmp/x"))).unwrap(),
            PathBuf::from("/tmp/x")
        );
    }

    #[test]
    fn instance_id_is_never_empty() {
        assert!(!instance_id().is_empty());
    }
}
