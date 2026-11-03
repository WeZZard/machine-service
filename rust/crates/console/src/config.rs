//! Trusted startup-only console configuration; discovery never touches the host.
//!
//! This is the Rust port of `bin/console_config.py`. The selected configuration
//! is read once, validated against a fixed schema, and detached into a snapshot.
//! `availability` reads only that snapshot; it never stats, probes, or launches
//! anything.

use std::collections::HashMap;
use std::fmt;
use std::fs::{Metadata, OpenOptions};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;

/// The selected console configuration or viewer is not trusted.
#[derive(Debug, Error)]
#[error("{0}")]
pub struct ConsoleConfigError(String);

impl ConsoleConfigError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    fn io(error: &io::Error) -> Self {
        Self(format!("invalid console configuration: {error}"))
    }

    fn invalid(error: impl fmt::Display) -> Self {
        Self(format!("invalid console configuration: {error}"))
    }
}

/// Standard macOS Screen Sharing bundles, in precedence order.
pub const DEFAULT_MACOS_VIEWERS: [&str; 2] = [
    "/System/Applications/Utilities/Screen Sharing.app",
    "/System/Library/CoreServices/Applications/Screen Sharing.app",
];

const KEYS: [&str; 4] = ["schemaVersion", "enabled", "linux_viewer", "macos_viewer"];

/// A detached, validated console configuration snapshot.
#[derive(Debug, Clone)]
pub struct ConsoleConfig {
    /// The configuration schema version; always `1` for an accepted snapshot.
    pub schema_version: i64,
    /// Whether console support is enabled.
    pub enabled: bool,
    /// The validated TurboVNC executable, if configured.
    pub linux_viewer: Option<String>,
    /// The validated Screen Sharing bundle, if configured.
    pub macos_viewer: Option<String>,
}

impl ConsoleConfig {
    /// The inert configuration used when no trusted configuration was selected.
    pub fn disabled() -> Self {
        Self {
            schema_version: 1,
            enabled: false,
            linux_viewer: None,
            macos_viewer: None,
        }
    }

    /// The configured viewer path for a console kind.
    pub fn viewer(&self, kind: &str) -> Option<&str> {
        match kind {
            "linux" => self.linux_viewer.as_deref(),
            "macos" => self.macos_viewer.as_deref(),
            _ => None,
        }
    }
}

static SNAPSHOT: OnceLock<Mutex<Option<ConsoleConfig>>> = OnceLock::new();

fn snapshot_mutex() -> &'static Mutex<Option<ConsoleConfig>> {
    SNAPSHOT.get_or_init(|| Mutex::new(None))
}

fn lock_snapshot() -> std::sync::MutexGuard<'static, Option<ConsoleConfig>> {
    snapshot_mutex()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn clear_snapshot() {
    *lock_snapshot() = None;
}

fn store_snapshot(config: &ConsoleConfig) {
    *lock_snapshot() = Some(config.clone());
}

#[cfg(test)]
thread_local! {
    static UID_OVERRIDE: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
}

fn current_uid() -> u32 {
    #[cfg(test)]
    {
        if let Some(uid) = UID_OVERRIDE.with(|cell| cell.get()) {
            return uid;
        }
    }
    // SAFETY: `getuid` takes no arguments and only reads process state.
    unsafe { libc::getuid() }
}

fn is_executable(path: &Path) -> bool {
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `c_path` is a valid NUL-terminated path.
    unsafe { libc::access(c_path.as_ptr(), libc::X_OK) == 0 }
}

/// Validate an absolute, control-free path without `..` components.
fn absolute(value: &str, label: &str) -> Result<PathBuf, ConsoleConfigError> {
    let invalid = value.is_empty()
        || !value.starts_with('/')
        || value.chars().any(|c| (c as u32) < 32 || c == '\u{7f}')
        || Path::new(value)
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir));
    if invalid {
        return Err(ConsoleConfigError::new(format!(
            "{label} must be an absolute path without control characters or traversal"
        )));
    }
    Ok(PathBuf::from(value))
}

/// Reject any parent directory that is a symlink, group/other writable, or
/// owned by another user.
fn check_parents(path: &Path) -> Result<(), ConsoleConfigError> {
    for parent in path.ancestors().skip(1) {
        let info = std::fs::symlink_metadata(parent).map_err(|e| ConsoleConfigError::io(&e))?;
        if !info.file_type().is_dir() || info.mode() & 0o022 != 0 || !trusted_owner(info.uid()) {
            return Err(ConsoleConfigError::new(format!(
                "untrusted parent directory: {}",
                parent.display()
            )));
        }
    }
    Ok(())
}

fn trusted_owner(uid: u32) -> bool {
    uid == 0 || uid == current_uid()
}

fn viewer(value: &Value, kind: &str) -> Result<String, ConsoleConfigError> {
    let label = format!("{kind}_viewer");
    let raw = value.as_str().unwrap_or("");
    let path = absolute(raw, &label)?;
    check_parents(&path)?;
    // Standard package-manager links are allowed only if both locations are
    // trusted.
    let path = std::fs::canonicalize(&path).map_err(|e| ConsoleConfigError::io(&e))?;
    check_parents(&path)?;
    let info = std::fs::metadata(&path).map_err(|e| ConsoleConfigError::io(&e))?;
    if !trusted_owner(info.uid()) || info.mode() & 0o022 != 0 {
        return Err(ConsoleConfigError::new(format!("untrusted {kind} viewer")));
    }
    if kind == "linux" {
        if !info.file_type().is_file() || !is_executable(&path) {
            return Err(ConsoleConfigError::new(
                "linux_viewer must be an executable regular TurboVNC file",
            ));
        }
    } else {
        if path.extension().map(|e| e != "app").unwrap_or(true) || !info.file_type().is_dir() {
            return Err(ConsoleConfigError::new(
                "macos_viewer must be an installed .app directory",
            ));
        }
        let executable = path.join("Contents/MacOS/Screen Sharing");
        check_parents(&executable)?;
        let binary =
            std::fs::symlink_metadata(&executable).map_err(|e| ConsoleConfigError::io(&e))?;
        if !binary.file_type().is_file()
            || binary.mode() & 0o022 != 0
            || !trusted_owner(binary.uid())
            || !is_executable(&executable)
        {
            return Err(ConsoleConfigError::new(
                "macos_viewer must contain the Screen Sharing executable",
            ));
        }
    }
    Ok(path.to_string_lossy().into_owned())
}

fn read_bounded(path: &Path, info: &Metadata) -> Result<Vec<u8>, ConsoleConfigError> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| ConsoleConfigError::io(&e))?;
    let mut opened: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `file` owns a valid descriptor and `opened` is a valid out-pointer.
    if unsafe { libc::fstat(file.as_raw_fd(), &mut opened) } != 0 {
        return Err(ConsoleConfigError::io(&io::Error::last_os_error()));
    }
    if opened.st_dev as u64 != info.dev()
        || opened.st_ino as u64 != info.ino()
        || opened.st_mode as u32 != info.mode()
        || opened.st_uid as u32 != info.uid()
    {
        return Err(ConsoleConfigError::new(
            "console configuration changed while opening",
        ));
    }
    let mut data = Vec::new();
    (&file)
        .take(65537)
        .read_to_end(&mut data)
        .map_err(|e| ConsoleConfigError::io(&e))?;
    if data.len() > 65536 {
        return Err(ConsoleConfigError::new(
            "console configuration exceeds 65536 bytes",
        ));
    }
    Ok(data)
}

/// A JSON value that rejects duplicate object keys, mirroring Python's
/// `object_pairs_hook=_unique`.
struct UniqueValue(Value);

impl<'de> Deserialize<'de> for UniqueValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(UniqueVisitor)
    }
}

struct UniqueVisitor;

impl<'de> Visitor<'de> for UniqueVisitor {
    type Value = UniqueValue;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Bool(value)))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Number(value.into())))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Number(value.into())))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        serde_json::Number::from_f64(value)
            .map(|n| UniqueValue(Value::Number(n)))
            .ok_or_else(|| de::Error::custom("invalid JSON number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::String(value.to_owned())))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::String(value)))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Null))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Null))
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(UniqueValue(value)) = seq.next_element()? {
            values.push(value);
        }
        Ok(UniqueValue(Value::Array(values)))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = serde_json::Map::new();
        while let Some((key, UniqueValue(value))) = map.next_entry::<String, UniqueValue>()? {
            if values.contains_key(&key) {
                return Err(de::Error::custom(format!(
                    "duplicate console configuration key: {key}"
                )));
            }
            values.insert(key, value);
        }
        Ok(UniqueValue(Value::Object(values)))
    }
}

fn strip_position(message: &str) -> &str {
    match message.find(" at line ") {
        Some(index) => &message[..index],
        None => message,
    }
}

fn parse_unique(data: &[u8]) -> Result<Value, String> {
    let text = std::str::from_utf8(data).map_err(|e| e.to_string())?;
    let parsed: UniqueValue =
        serde_json::from_str(text).map_err(|e| strip_position(&e.to_string()).to_string())?;
    Ok(parsed.0)
}

fn load_config_with(
    path: Option<&str>,
    environ: Option<&HashMap<String, String>>,
    defaults: &[&str],
) -> Result<ConsoleConfig, ConsoleConfigError> {
    clear_snapshot();
    let env_map: HashMap<String, String> = match environ {
        Some(map) => map.clone(),
        None => std::env::vars().collect(),
    };
    let selected = match path {
        Some(value) => Some(value.to_string()),
        None => env_map.get("MACHINE_SERVICE_CONSOLE_CONFIG").cloned(),
    };
    let explicit = selected.is_some();
    let selected = match selected {
        Some(value) => value,
        None => {
            let home = env_map
                .get("HOME")
                .cloned()
                .or_else(|| std::env::var("HOME").ok())
                .unwrap_or_default();
            PathBuf::from(home)
                .join(".config/machine-service/console.json")
                .to_string_lossy()
                .into_owned()
        }
    };
    let filename = absolute(&selected, "console configuration")?;
    let mut result = ConsoleConfig::disabled();
    let info = match std::fs::symlink_metadata(&filename) {
        Ok(info) => info,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if explicit {
                return Err(ConsoleConfigError::new(
                    "explicit console configuration does not exist",
                ));
            }
            store_snapshot(&result);
            return Ok(result);
        }
        Err(error) => return Err(ConsoleConfigError::io(&error)),
    };
    check_parents(&filename)?;
    let mode = info.mode() & 0o777;
    if !info.file_type().is_file()
        || info.uid() != current_uid()
        || !(mode == 0o600 || mode == 0o644)
    {
        return Err(ConsoleConfigError::new(
            "console configuration must be an owned nonsymlink regular file with mode 0600 or 0644",
        ));
    }
    let data = read_bounded(&filename, &info)?;
    let raw = parse_unique(&data).map_err(ConsoleConfigError::invalid)?;
    let object = match raw {
        Value::Object(object) => object,
        _ => {
            return Err(ConsoleConfigError::new(
                "console configuration contains unsupported keys",
            ))
        }
    };
    if object.keys().any(|key| !KEYS.contains(&key.as_str())) {
        return Err(ConsoleConfigError::new(
            "console configuration contains unsupported keys",
        ));
    }
    if object.get("schemaVersion").and_then(Value::as_i64) != Some(1) {
        return Err(ConsoleConfigError::new(
            "console schemaVersion must be integer 1",
        ));
    }
    let enabled = match object.get("enabled") {
        None => false,
        Some(Value::Bool(value)) => *value,
        Some(_) => return Err(ConsoleConfigError::new("console enabled must be a boolean")),
    };
    result.enabled = enabled;
    if let Some(value) = object.get("linux_viewer") {
        result.linux_viewer = Some(viewer(value, "linux")?);
    }
    if let Some(value) = object.get("macos_viewer") {
        result.macos_viewer = Some(viewer(value, "macos")?);
    } else {
        for candidate in defaults {
            // Do not skip an installed but invalid candidate or dangling link.
            if std::fs::symlink_metadata(candidate).is_ok() {
                result.macos_viewer =
                    Some(viewer(&Value::String((*candidate).to_string()), "macos")?);
                break;
            }
        }
    }
    store_snapshot(&result);
    Ok(result)
}

/// Return a detached validated startup snapshot, or raise `ConsoleConfigError`.
///
/// Selection is explicit path, `MACHINE_SERVICE_CONSOLE_CONFIG`, then the standard
/// user configuration. Only an absent unselected default is disabled silently.
/// Availability means host configuration, not guest readiness or rendered
/// pixels.
pub fn load_config(
    path: Option<&str>,
    environ: Option<&HashMap<String, String>>,
) -> Result<Option<ConsoleConfig>, ConsoleConfigError> {
    load_config_with(path, environ, &DEFAULT_MACOS_VIEWERS).map(Some)
}

/// Read a startup snapshot only. This never stats, probes, or launches anything.
pub fn availability(kind: &str, config: Option<&ConsoleConfig>) -> bool {
    if kind != "linux" && kind != "macos" {
        return false;
    }
    let snapshot = if config.is_none() {
        Some(lock_snapshot().clone())
    } else {
        None
    };
    let selected = config.or_else(|| snapshot.as_ref().and_then(|snapshot| snapshot.as_ref()));
    match selected {
        Some(config) => {
            config.enabled
                && config
                    .viewer(kind)
                    .map(|viewer| !viewer.is_empty())
                    .unwrap_or(false)
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    static SERIAL: Mutex<()> = Mutex::new(());

    struct Fixture {
        _guard: std::sync::MutexGuard<'static, ()>,
        _dir: tempfile::TempDir,
        root: PathBuf,
        file: PathBuf,
        viewer: PathBuf,
        raw: serde_json::Map<String, Value>,
        defaults: Vec<String>,
    }

    impl Fixture {
        fn new() -> Self {
            let guard = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
            // A temporary folder of the test's own, never under `HOME`
            // (DC-1xx-ISO-7).
            // Resolved, since the trust check refuses a linked parent such
            // as macOS's `/var`.
            let temp = std::env::temp_dir()
                .canonicalize()
                .expect("temporary folder");
            let dir = tempfile::Builder::new()
                .prefix(".console-test-")
                .tempdir_in(temp)
                .expect("temporary directory");
            let root = dir.path().to_path_buf();
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
            let file = root.join("console.json");
            let viewer = root.join("vncviewer");
            std::fs::write(&viewer, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&viewer, std::fs::Permissions::from_mode(0o755)).unwrap();
            let mut raw = serde_json::Map::new();
            raw.insert("schemaVersion".into(), Value::Number(1.into()));
            raw.insert("enabled".into(), Value::Bool(true));
            raw.insert(
                "linux_viewer".into(),
                Value::String(viewer.to_string_lossy().into_owned()),
            );
            let defaults = vec![root.join("absent.app").to_string_lossy().into_owned()];
            let mut fixture = Self {
                _guard: guard,
                _dir: dir,
                root,
                file,
                viewer,
                raw,
                defaults,
            };
            fixture.save();
            fixture
        }

        fn save(&mut self) {
            std::fs::write(
                &self.file,
                serde_json::to_vec(&Value::Object(self.raw.clone())).unwrap(),
            )
            .unwrap();
            std::fs::set_permissions(&self.file, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        fn load(&self) -> Result<ConsoleConfig, ConsoleConfigError> {
            let defaults: Vec<&str> = self.defaults.iter().map(String::as_str).collect();
            load_config_with(Some(self.file.to_str().unwrap()), None, &defaults)
        }

        fn load_env(
            &self,
            env: &HashMap<String, String>,
        ) -> Result<ConsoleConfig, ConsoleConfigError> {
            let defaults: Vec<&str> = self.defaults.iter().map(String::as_str).collect();
            load_config_with(None, Some(env), &defaults)
        }
    }

    #[test]
    fn missing_default_disabled_and_explicit_missing_refused() {
        let fixture = Fixture::new();
        let mut env = HashMap::new();
        env.insert(
            "HOME".to_string(),
            fixture.root.to_string_lossy().into_owned(),
        );
        let result = fixture.load_env(&env).unwrap();
        assert!(!result.enabled);
        assert!(!availability("linux", None));
        assert!(load_config_with(
            Some(fixture.root.join("missing").to_str().unwrap()),
            Some(&env),
            &["absent"]
        )
        .is_err());
        let mut empty = HashMap::new();
        empty.insert("MACHINE_SERVICE_CONSOLE_CONFIG".to_string(), String::new());
        drop(fixture);
        assert!(load_config_with(None, Some(&empty), &["absent"]).is_err());
    }

    #[test]
    fn snapshot_no_discovery_work() {
        let mut fixture = Fixture::new();
        let before = collect_files(&fixture.root);
        let mut result = fixture.load().unwrap();
        assert_eq!(before, collect_files(&fixture.root));
        std::fs::remove_file(&fixture.viewer).unwrap();
        std::fs::remove_file(&fixture.file).unwrap();
        assert!(availability("linux", Some(&result)));
        assert!(availability("linux", None));
        assert!(!availability("macos", None));
        assert!(!availability("invalid", None));
        result.enabled = false;
        assert!(availability("linux", None));
        fixture.raw.clear();
    }

    #[test]
    fn modes_and_default_enabled() {
        let mut fixture = Fixture::new();
        fixture.raw.remove("linux_viewer");
        fixture.raw.remove("enabled");
        fixture.save();
        for mode in [0o600, 0o644] {
            std::fs::set_permissions(&fixture.file, std::fs::Permissions::from_mode(mode)).unwrap();
            assert!(!fixture.load().unwrap().enabled);
        }
        for mode in [0o400, 0o640, 0o660, 0o666, 0o755] {
            std::fs::set_permissions(&fixture.file, std::fs::Permissions::from_mode(mode)).unwrap();
            assert!(fixture.load().is_err(), "mode {mode:o} accepted");
        }
    }

    #[test]
    fn invalid_schema() {
        let mut fixture = Fixture::new();
        let viewer = fixture.viewer.to_string_lossy().into_owned();
        let cases: Vec<(&str, Value)> = vec![
            ("schemaVersion", Value::Bool(true)),
            ("schemaVersion", Value::from(1.0)),
            ("schemaVersion", Value::Number(2.into())),
            ("enabled", Value::Number(1.into())),
            ("enabled", Value::String("true".into())),
            ("secret", Value::String("no".into())),
            ("linux_viewer", Value::Null),
            ("linux_viewer", Value::String("relative".into())),
            ("linux_viewer", Value::String(format!("{viewer}\n"))),
        ];
        for (field, value) in cases {
            let mut raw = serde_json::Map::new();
            raw.insert("schemaVersion".into(), Value::Number(1.into()));
            raw.insert(field.into(), value);
            fixture.raw = raw;
            fixture.save();
            assert!(fixture.load().is_err(), "{field} accepted");
        }
        for data in [
            "[]",
            "{\"enabled\":false}",
            "{\"schemaVersion\":1,\"schemaVersion\":1}",
        ] {
            std::fs::write(&fixture.file, data).unwrap();
            assert!(fixture.load().is_err(), "{data} accepted");
        }
        std::fs::write(&fixture.file, "x".repeat(65537)).unwrap();
        assert!(fixture.load().is_err());
    }

    #[test]
    fn nonsymlink_regular_owned_file() {
        let mut fixture = Fixture::new();
        let target = fixture.root.join("target.json");
        std::fs::rename(&fixture.file, &target).unwrap();
        symlink(&target, &fixture.file).unwrap();
        assert!(fixture.load().is_err());
        std::fs::remove_file(&fixture.file).unwrap();
        std::fs::create_dir(&fixture.file).unwrap();
        assert!(fixture.load().is_err());
        std::fs::remove_dir(&fixture.file).unwrap();
        fixture.save();
        UID_OVERRIDE.with(|cell| cell.set(Some(current_uid().wrapping_add(1))));
        let result = fixture.load();
        UID_OVERRIDE.with(|cell| cell.set(None));
        assert!(result.is_err());
    }

    #[test]
    fn untrusted_parent_and_viewer() {
        let fixture = Fixture::new();
        std::fs::set_permissions(&fixture.root, std::fs::Permissions::from_mode(0o770)).unwrap();
        assert!(fixture.load().is_err());
        std::fs::set_permissions(&fixture.root, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&fixture.viewer, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(fixture.load().is_err());
        std::fs::set_permissions(&fixture.viewer, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(fixture.load().is_err());
    }

    #[test]
    fn screen_sharing_bundle() {
        let mut fixture = Fixture::new();
        let app = fixture.root.join("Screen Sharing.app");
        let binary = app.join("Contents/MacOS/Screen Sharing");
        std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        std::fs::write(&binary, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        fixture.raw.insert(
            "macos_viewer".into(),
            Value::String(app.to_string_lossy().into_owned()),
        );
        fixture.save();
        assert_eq!(
            fixture.load().unwrap().macos_viewer.unwrap(),
            app.to_string_lossy()
        );
        std::fs::remove_file(&binary).unwrap();
        assert!(fixture.load().is_err());
    }

    #[test]
    fn fixed_screen_sharing_candidates_and_precedence() {
        let mut fixture = Fixture::new();
        let modern = fixture.root.join("Utilities/Screen Sharing.app");
        let legacy = fixture.root.join("CoreServices/Screen Sharing.app");
        for app in [&modern, &legacy] {
            let binary = app.join("Contents/MacOS/Screen Sharing");
            std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
            std::fs::write(&binary, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        fixture.defaults = vec![
            modern.to_string_lossy().into_owned(),
            legacy.to_string_lossy().into_owned(),
        ];
        assert_eq!(
            fixture.load().unwrap().macos_viewer.unwrap(),
            modern.to_string_lossy()
        );
        let moved = fixture.root.join("moved.app");
        std::fs::rename(&modern, &moved).unwrap();
        assert_eq!(
            fixture.load().unwrap().macos_viewer.unwrap(),
            legacy.to_string_lossy()
        );
        std::fs::set_permissions(&legacy, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(fixture.load().is_err());
        std::fs::set_permissions(&legacy, std::fs::Permissions::from_mode(0o755)).unwrap();
        fixture.raw.insert(
            "macos_viewer".into(),
            Value::String(moved.to_string_lossy().into_owned()),
        );
        fixture.save();
        assert_eq!(
            fixture.load().unwrap().macos_viewer.unwrap(),
            moved.to_string_lossy()
        );
    }

    #[test]
    fn environment_selection_and_explicit_precedence() {
        let fixture = Fixture::new();
        let mut env = HashMap::new();
        env.insert(
            "MACHINE_SERVICE_CONSOLE_CONFIG".to_string(),
            fixture.file.to_string_lossy().into_owned(),
        );
        let defaults: Vec<&str> = fixture.defaults.iter().map(String::as_str).collect();
        assert!(
            load_config_with(None, Some(&env), &defaults)
                .unwrap()
                .enabled
        );
        let mut override_env = HashMap::new();
        override_env.insert(
            "MACHINE_SERVICE_CONSOLE_CONFIG".to_string(),
            "/missing".to_string(),
        );
        assert!(
            load_config_with(
                Some(fixture.file.to_str().unwrap()),
                Some(&override_env),
                &defaults
            )
            .unwrap()
            .enabled
        );
    }

    fn collect_files(root: &Path) -> Vec<PathBuf> {
        let mut result = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(path) = stack.pop() {
            if let Ok(entries) = std::fs::read_dir(&path) {
                for entry in entries.flatten() {
                    let child = entry.path();
                    result.push(child.clone());
                    if child.is_dir() {
                        stack.push(child);
                    }
                }
            }
        }
        result.sort();
        result
    }
}
