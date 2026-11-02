//! Resolved service configuration and logging.
//!
//! The Python daemon keeps module-level globals that `configure_environment`
//! replaces before serving. The Rust port carries the same values in one
//! `Config` value so tests can build an isolated service without process-global
//! mutation.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

/// A resolved configuration for one service process.
#[derive(Debug, Clone)]
pub struct Config {
    /// The selected environment bundle, or `None` for the legacy defaults.
    pub environment: Option<Value>,
    /// The Tart executable path.
    pub tart_executable: String,
    /// The full environment for `tart` when a selected environment is active.
    pub subprocess_env: Option<Vec<(String, String)>>,
    /// The pilot-images checkout, the instance's image source. `None` when
    /// none is configured: the instance then has no images (IC-7, DC-94).
    pub pilot: Option<PathBuf>,
    /// The service state directory.
    pub state_dir: PathBuf,
    /// `state_dir/state.json`.
    pub state_file: PathBuf,
    /// `state_dir/state.lock`.
    pub lock_file: PathBuf,
    /// `state_dir/service.log`.
    pub log_file: PathBuf,
    /// The bind address.
    pub host: String,
    /// The bind port.
    pub port: u16,
    /// How exec, push and pull reach the guest.
    pub guest_link: GuestLink,
}

/// How exec, push and pull reach the guest (`docs/arch/guest-session.md`,
/// GS-2), chosen by `MACHINE_SERVICE_GUEST_LINK`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GuestLink {
    /// One `ssh` or `scp` process per operation.
    #[default]
    Ssh,
    /// One guest session per lease, carried by `ssh` standard input and
    /// output.
    SshSession,
    /// The relay agent in the guest's desktop session, through this
    /// instance's broker (`docs/arch/relay-agent.md`, `docs/arch/broker.md`);
    /// a lease whose relay agent does not register falls back to [`Ssh`]
    /// (RA-8).
    ///
    /// [`Ssh`]: GuestLink::Ssh
    Relay,
}

impl GuestLink {
    /// The accepted configuration values.
    pub const NAMES: [&'static str; 3] = ["ssh", "ssh-session", "relay"];

    /// Parse a configuration value.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "ssh" => Ok(GuestLink::Ssh),
            "ssh-session" => Ok(GuestLink::SshSession),
            "relay" => Ok(GuestLink::Relay),
            other => Err(format!(
                "MACHINE_SERVICE_GUEST_LINK must be one of {}; got {other:?}",
                Self::NAMES.join(", ")
            )),
        }
    }

    /// Read `MACHINE_SERVICE_GUEST_LINK`; unset or empty selects [`GuestLink::Ssh`].
    pub fn from_environment() -> Result<Self, String> {
        match std::env::var("MACHINE_SERVICE_GUEST_LINK") {
            Ok(value) if !value.is_empty() => Self::parse(&value),
            Ok(_) | Err(std::env::VarError::NotPresent) => Ok(GuestLink::Ssh),
            Err(std::env::VarError::NotUnicode(_)) => {
                Err("MACHINE_SERVICE_GUEST_LINK is not valid UTF-8".to_string())
            }
        }
    }

    /// The configuration value naming this link.
    pub fn name(self) -> &'static str {
        match self {
            GuestLink::Ssh => "ssh",
            GuestLink::SshSession => "ssh-session",
            GuestLink::Relay => "relay",
        }
    }
}

impl Config {
    /// Resolve the legacy defaults from the ambient environment, exactly as the
    /// Python module did at import time.
    pub fn legacy() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"));
        let pilot = std::env::var_os("PILOT_REPO")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let state_dir = std::env::var_os("MACHINE_SERVICE_STATE")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/state/machine-service"));
        let state_dir = state_dir
            .canonicalize()
            .unwrap_or_else(|_| absolute_lexical(&state_dir));
        let port = std::env::var("MACHINE_SERVICE_PORT")
            .ok()
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(6240);
        let host =
            std::env::var("MACHINE_SERVICE_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
        Self {
            environment: None,
            tart_executable: "tart".to_string(),
            subprocess_env: None,
            pilot,
            state_file: state_dir.join("state.json"),
            lock_file: state_dir.join("state.lock"),
            log_file: state_dir.join("service.log"),
            state_dir,
            host,
            port,
            guest_link: GuestLink::Ssh,
        }
    }

    /// Resolve a selected environment bundle returned by
    /// `environment::load_environment`, mirroring `configure_environment`.
    pub fn from_selected(bundle: &Value) -> Self {
        let empty = Value::Object(serde_json::Map::new());
        let exports = bundle.get("environment").unwrap_or(&empty);
        let profile = bundle.get("profile").unwrap_or(&empty);
        let get = |value: &Value, key: &str| -> String {
            value
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let state_dir = PathBuf::from(get(profile, "serviceStateDir"));
        let mut subprocess_env: Vec<(String, String)> = std::env::vars().collect();
        if let Some(map) = exports.as_object() {
            for (key, value) in map {
                if let Some(text) = value.as_str() {
                    subprocess_env.retain(|(k, _)| k != key);
                    subprocess_env.push((key.clone(), text.to_string()));
                }
            }
        }
        let port = get(exports, "MACHINE_SERVICE_PORT")
            .parse::<u16>()
            .unwrap_or(6240);
        Self {
            environment: Some(bundle.clone()),
            tart_executable: get(profile, "tartPath"),
            subprocess_env: Some(subprocess_env),
            pilot: Some(get(profile, "imageRepository"))
                .filter(|path| !path.is_empty())
                .map(PathBuf::from),
            state_file: state_dir.join("state.json"),
            lock_file: state_dir.join("state.lock"),
            log_file: state_dir.join("service.log"),
            state_dir,
            host: get(exports, "MACHINE_SERVICE_HOST"),
            port,
            guest_link: GuestLink::Ssh,
        }
    }

    /// The selected environment fingerprint, when a profile is active.
    pub fn fingerprint(&self) -> Option<String> {
        self.environment
            .as_ref()
            .and_then(|bundle| bundle.get("identity"))
            .and_then(|identity| identity.get("fingerprint"))
            .and_then(Value::as_str)
            .map(str::to_string)
    }
}

/// Make a path absolute without resolving symlinks, matching Python
/// `Path.absolute()`.
pub fn absolute_lexical(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    }
}

/// Append one timestamped line to the service log, ignoring log-path failures
/// exactly as the Python `log` helper does.
pub fn log_to_file(log_file: &Path, message: &str) {
    let stamp = format_local_timestamp();
    let _ = std::fs::create_dir_all(log_file.parent().unwrap_or(Path::new(".")));
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_file)
    {
        use std::io::Write;
        let _ = writeln!(file, "[{stamp}] {message}");
    }
}

/// Format the current local time as `%Y-%m-%dT%H:%M:%S%z`.
fn format_local_timestamp() -> String {
    // SAFETY: `localtime_r` writes into a caller-provided `tm`; the `time_t`
    // value is valid for the call. No Rust aliasing is involved.
    unsafe {
        let now: libc::time_t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&now, &mut tm).is_null() {
            return String::new();
        }
        let mut buffer = [0i8; 64];
        let format = std::ffi::CString::new("%Y-%m-%dT%H:%M:%S%z").expect("literal format");
        let written = libc::strftime(buffer.as_mut_ptr(), buffer.len(), format.as_ptr(), &tm);
        let bytes: Vec<u8> = buffer[..written].iter().map(|byte| *byte as u8).collect();
        String::from_utf8(bytes).unwrap_or_default()
    }
}

/// The current time in Unix seconds as a float, matching Python `time.time()`.
pub fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0)
}

/// A process-local mutex wrapper used where Python used `threading.Lock`.
pub type ThreadLock<T> = Mutex<T>;
