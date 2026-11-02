//! Read-only application catalog; no commands, leases, or inventory generation.
//!
//! Publication document (closed objects throughout):
//!  Portable `images/<image>/applications.json` documents contain the closed fields
//!  `{schemaVersion:1,image,inventory,provenance}`. Host-local associations contain
//!  `{schemaVersion:2,image,base,inventorySha256}`. There is no legacy fallback.
//!  `inventory: {schemaVersion:1,os,architecture,collectedAt,sources,applications}`
//!  `sources: [{id, status:'available'|'unavailable', command?:[str], roots?:[str]}]`.
//!  IDs unique, <=16 sources; optional provenance fields only for available sources.
//!  `applications: [{id,name,aliases:[str],version:str|null}]`.
//!
//! `fingerprint_base` returns `{base_vm,path,files:{filename:{st_dev,st_ino,st_size,
//!  st_mtime_ns}}}`. `path` is the canonical absolute local base directory beneath
//!  `base_root` (default `~/.tart/vms`). `config.json` and `disk.img` are required;
//!  `nvram.bin` is required on macOS and optional on Linux. Only these selected
//!  names participate (`control.sock` and other ancillary files are ignored).
//!  All present selected files must be regular, not symlinks. Root/base symlinks
//!  are rejected. Validation compares the entire selected file set and the
//!  `st_ino`, `st_size` and `st_mtime_ns` fields exactly, plus `base_vm`/`path`.
//!  `st_dev` is recorded but not compared, because macOS renumbers a volume each
//!  time it mounts it (DC-17). This is local staleness
//!  detection, NOT a hash or attestation. Publication must compare fingerprints
//!  before and after association and abort if they differ.
//!
//! `load_catalog(pilot_repo, lines, *, base_root=None, diagnostic=None)` accepts
//!  `discover_lines`' image -> config mapping (`kind`/`base_vm` required; other config
//!  keys ignored). Diagnostics identify exclusions and report each missing member's
//!  exact path. Other exclusion messages are bounded to 512 characters.
//!  Malformed existing documents always raise `CatalogError`, even for stale bases.
//!  Output serialization budget matches `json.dumps(result, indent=2).encode()+b'\n'`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{CString, OsString};
use std::fmt;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;
use serde::Deserializer;
use serde_json::{json, Map, Number, Value};
use sha2::{Digest, Sha256};

/// Maximum serialized catalog size and maximum input document size.
pub const MAX_BYTES: usize = 4 * 1024 * 1024;
/// Maximum number of images accepted by [`load_catalog`].
pub const MAX_IMAGES: usize = 64;
/// Maximum number of applications across a catalog.
pub const MAX_APPLICATIONS: usize = 20000;

const NAMES: [&str; 3] = ["config.json", "disk.img", "nvram.bin"];
const STAT_FIELDS: [&str; 4] = ["st_dev", "st_ino", "st_size", "st_mtime_ns"];

/// Invalid or unavailable catalog; caller must not return a partial result.
///
/// Callers that mirror the Python module catch both Python `OSError` and
/// `CatalogError`; [`CatalogError::Io`] preserves an underlying
/// [`std::io::Error`] for that case.
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    /// A validation or availability failure with the Python exception's text.
    #[error("{0}")]
    Message(String),
    /// An unreadable-filesystem failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

fn msg(message: &str) -> CatalogError {
    CatalogError::Message(message.to_string())
}

/// Internal distinction between a base that is unavailable/unsafe and a
/// programming-level argument error. Mirrors Python's private `_BaseUnavailable`.
enum FingerprintFailure {
    Invalid(CatalogError),
    Unavailable,
}

/// Per-call limits. The public entry points use [`Limits::default`]; the unit
/// tests in this crate substitute the Python tests' patched module constants.
#[derive(Clone, Copy)]
struct Limits {
    max_bytes: usize,
    max_images: usize,
    max_applications: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_bytes: MAX_BYTES,
            max_images: MAX_IMAGES,
            max_applications: MAX_APPLICATIONS,
        }
    }
}

// ---------------------------------------------------------------- text checks

/// Exact ECMAScript whitespace set; Python `strip`/`isspace` differ (NEL, FEFF).
fn is_text_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'
            | '\u{000a}'
            | '\u{000b}'
            | '\u{000c}'
            | '\u{000d}'
            | '\u{0020}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

fn is_ascii_alnum(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}

fn object(value: &Value, keys: &[&str]) -> Result<(), CatalogError> {
    match value {
        Value::Object(map) => {
            if map.len() == keys.len() && keys.iter().all(|k| map.contains_key(*k)) {
                Ok(())
            } else {
                Err(msg("invalid object fields"))
            }
        }
        _ => Err(msg("invalid object fields")),
    }
}

fn text(value: &Value, limit: usize, nullable: bool) -> Result<(), CatalogError> {
    if nullable && value.is_null() {
        return Ok(());
    }
    match value {
        Value::String(s) => {
            let count = s.chars().count();
            if count == 0 || count > limit {
                return Err(msg("invalid text field"));
            }
            if !s.chars().any(|c| !is_text_whitespace(c)) {
                return Err(msg("invalid text field"));
            }
            if s.chars()
                .any(|c| (c as u32) < 32 || (0xD800..=0xDFFF).contains(&(c as u32)))
            {
                return Err(msg("invalid text field"));
            }
            Ok(())
        }
        _ => Err(msg("invalid text field")),
    }
}

fn key(value: &Value) -> Result<(), CatalogError> {
    text(value, 128, false)?;
    let s = value.as_str().expect("key() proves a string");
    let bytes = s.as_bytes();
    let ok = !bytes.is_empty()
        && is_ascii_alnum(bytes[0])
        && bytes[1..]
            .iter()
            .all(|b| is_ascii_alnum(*b) || *b == b'_' || *b == b'.' || *b == b'-');
    if !ok {
        return Err(msg("invalid image or base key"));
    }
    Ok(())
}

fn os_check(value: &Value) -> Result<(), CatalogError> {
    match value.as_str() {
        Some("linux") | Some("macos") => Ok(()),
        _ => Err(msg("invalid OS")),
    }
}

fn hash_check(value: &Value) -> Result<(), CatalogError> {
    match value.as_str() {
        Some(s)
            if s.len() == 64
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) =>
        {
            Ok(())
        }
        _ => Err(msg("invalid SHA256")),
    }
}

fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

// ------------------------------------------------------------------- paths

fn home_dir() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(v) => PathBuf::from(v),
        None => PathBuf::from("/"),
    }
}

fn expand_user(path: &Path) -> PathBuf {
    if let Some(s) = path.to_str() {
        if s == "~" {
            return home_dir();
        }
        if let Some(rest) = s.strip_prefix("~/") {
            return home_dir().join(rest);
        }
    }
    path.to_path_buf()
}

/// Python `Path.absolute()`: make absolute without normalizing or resolving.
fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    }
}

fn push_target_components(target: &Path, queue: &mut VecDeque<OsString>) {
    let mut front: VecDeque<OsString> = VecDeque::new();
    for c in target.components() {
        match c {
            Component::Normal(s) => front.push_back(s.to_os_string()),
            Component::ParentDir => front.push_back(OsString::from("..")),
            _ => {}
        }
    }
    front.append(queue);
    *queue = front;
}

/// Python `Path.resolve()` (strict=False) on POSIX: resolve symlinks as far as
/// possible, normalize `.`/`..`, and keep nonexistent trailing components.
fn resolve_path(path: &Path) -> PathBuf {
    let abs = absolute_path(path);
    let root = PathBuf::from("/");
    let mut current = root.clone();
    let mut queue: VecDeque<OsString> = VecDeque::new();
    for c in abs.components() {
        match c {
            Component::Normal(s) => queue.push_back(s.to_os_string()),
            Component::ParentDir => queue.push_back(OsString::from("..")),
            _ => {}
        }
    }
    let mut seen: HashMap<PathBuf, PathBuf> = HashMap::new();
    let mut expansions = 0usize;
    while let Some(comp) = queue.pop_front() {
        let bytes = comp.as_bytes();
        if bytes == b"." {
            continue;
        }
        if bytes == b".." {
            if current != root {
                current.pop();
            }
            continue;
        }
        let candidate = current.join(&comp);
        match std::fs::symlink_metadata(&candidate) {
            Ok(md) if md.file_type().is_symlink() => {
                expansions += 1;
                if expansions > 40 {
                    return current;
                }
                if let Some(prev) = seen.get(&candidate).cloned() {
                    current = prev;
                    continue;
                }
                seen.insert(candidate.clone(), current.clone());
                match std::fs::read_link(&candidate) {
                    Ok(target) => {
                        if target.is_absolute() {
                            current = root.clone();
                        }
                        push_target_components(&target, &mut queue);
                    }
                    Err(_) => current.push(&comp),
                }
            }
            _ => current.push(&comp),
        }
    }
    current
}

fn no_symlinks(path: &Path) -> Result<(), CatalogError> {
    let mut cur = Some(path);
    while let Some(part) = cur {
        if let Ok(md) = std::fs::symlink_metadata(part) {
            if md.file_type().is_symlink() {
                return Err(msg("symlink path refused"));
            }
        }
        cur = part.parent();
    }
    Ok(())
}

/// Locate host-local state by canonical VM-store root, without creating it.
pub fn association_path(base_root: &Path, image: &str) -> Result<PathBuf, CatalogError> {
    key(&Value::String(image.to_string()))?;
    let state = match std::env::var_os("PILOT_IMAGES_STATE_DIR") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => match std::env::var_os("XDG_STATE_HOME") {
            Some(v) if !v.is_empty() => expand_user(&PathBuf::from(v)).join("pilot-images"),
            _ => home_dir().join(".local/state/pilot-images"),
        },
    };
    let store = resolve_path(&expand_user(base_root));
    let namespace = sha256_hex(store.as_os_str().as_bytes());
    Ok(absolute_path(&expand_user(&state))
        .join("stores")
        .join(namespace)
        .join("base")
        .join(format!("{image}.json")))
}

// ------------------------------------------------------------------- stats

struct StatFields {
    st_dev: u64,
    st_ino: u64,
    st_size: u64,
    st_mtime_ns: i64,
    is_regular: bool,
}

// The explicit `i64` conversions are identity conversions on 64-bit targets
// but are required for 32-bit ones, where `time_t`/`c_long` are 32-bit. The
// `libc` crate names the fields `st_mtime` and `st_mtime_nsec` on macOS and
// Linux alike.
#[allow(clippy::useless_conversion)]
fn stat_mtime_ns(st: &libc::stat) -> i64 {
    i64::from(st.st_mtime) * 1_000_000_000 + i64::from(st.st_mtime_nsec)
}

fn lstat_fields(path: &Path) -> std::io::Result<StatFields> {
    let c = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL"))?;
    // SAFETY: `c` is a valid NUL-terminated path and `st` is a zeroed `libc::stat`
    // owned by this frame; `lstat` writes it on success.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::lstat(c.as_ptr(), &mut st) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let is_regular = (st.st_mode & libc::S_IFMT) == libc::S_IFREG;
    Ok(StatFields {
        st_dev: st.st_dev as u64,
        st_ino: st.st_ino as u64,
        st_size: st.st_size as u64,
        st_mtime_ns: stat_mtime_ns(&st),
        is_regular,
    })
}

impl StatFields {
    fn to_json(&self) -> Value {
        json!({
            "st_dev": self.st_dev,
            "st_ino": self.st_ino,
            "st_size": self.st_size,
            "st_mtime_ns": self.st_mtime_ns,
        })
    }
}

/// Return the exact local selected-file identity; raise `CatalogError` on failure.
pub fn fingerprint_base(base_root: &Path, base_vm: &str, os: &str) -> Result<Value, CatalogError> {
    match fingerprint_base_inner(base_root, base_vm, os) {
        Ok(value) => Ok(value),
        Err(FingerprintFailure::Invalid(e)) => Err(e),
        Err(FingerprintFailure::Unavailable) => Err(msg("base unavailable or unsafe")),
    }
}

fn fingerprint_base_inner(
    base_root: &Path,
    base_vm: &str,
    os: &str,
) -> Result<Value, FingerprintFailure> {
    key(&Value::String(base_vm.to_string())).map_err(FingerprintFailure::Invalid)?;
    os_check(&Value::String(os.to_string())).map_err(FingerprintFailure::Invalid)?;
    let root = absolute_path(&expand_user(base_root));
    let directory = root.join(base_vm);
    let outcome: Result<Value, CatalogError> = (|| {
        no_symlinks(&directory)?;
        if !directory.is_dir() {
            return Err(msg("base directory missing"));
        }
        let mut files = Map::new();
        for name in NAMES {
            let file = directory.join(name);
            let stat = match lstat_fields(&file) {
                Ok(stat) => stat,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    if name == "nvram.bin" && os == "linux" {
                        continue;
                    }
                    return Err(msg("required base file missing"));
                }
                Err(e) => return Err(CatalogError::Io(e)),
            };
            if !stat.is_regular {
                return Err(msg("nonregular base file refused"));
            }
            files.insert(name.to_string(), stat.to_json());
        }
        let resolved = std::fs::canonicalize(&directory)?;
        Ok(json!({
            "base_vm": base_vm,
            "path": resolved.to_string_lossy(),
            "files": Value::Object(files),
        }))
    })();
    outcome.map_err(|_| FingerprintFailure::Unavailable)
}

/// Whether a base fingerprint matches the recorded one. `st_dev` is recorded
/// but not compared: macOS numbers a volume when it mounts it, so a reboot
/// gives an unchanged base a new `st_dev` (DC-17). The canonical `path`
/// already names the volume.
fn same_base(current: &Value, recorded: &Value) -> bool {
    fn without_device(base: &Value) -> Value {
        let mut base = base.clone();
        if let Some(files) = base.get_mut("files").and_then(Value::as_object_mut) {
            for stat in files.values_mut().filter_map(Value::as_object_mut) {
                stat.remove("st_dev");
            }
        }
        base
    }
    without_device(current) == without_device(recorded)
}

fn default_fingerprint(root: &Path, vm: &str, os: &str) -> Result<Value, CatalogError> {
    fingerprint_base(root, vm, os)
}

// --------------------------------------------------------------- inventory

fn days_in_month(year: i32, month: i32) -> i32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Validate the collection timestamp regex plus calendar fields, mirroring
/// Python `re.fullmatch(...)` followed by `datetime.datetime.fromisoformat`.
fn valid_timestamp(s: &str) -> bool {
    let b = s.as_bytes();
    let dig = |i: usize| i < b.len() && b[i].is_ascii_digit();
    if b.len() < 20 {
        return false;
    }
    if !(dig(0)
        && dig(1)
        && dig(2)
        && dig(3)
        && b[4] == b'-'
        && dig(5)
        && dig(6)
        && b[7] == b'-'
        && dig(8)
        && dig(9)
        && b[10] == b'T'
        && dig(11)
        && dig(12)
        && b[13] == b':'
        && dig(14)
        && dig(15)
        && b[16] == b':'
        && dig(17)
        && dig(18))
    {
        return false;
    }
    let mut i = 19;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        let frac = i - start;
        if frac == 0 || frac > 6 {
            return false;
        }
    }
    if i >= b.len() {
        return false;
    }
    if b[i] == b'Z' {
        i += 1;
    } else if b[i] == b'+' || b[i] == b'-' {
        if i + 6 != b.len() {
            return false;
        }
        if !(dig(i + 1) && dig(i + 2) && b[i + 3] == b':' && dig(i + 4) && dig(i + 5)) {
            return false;
        }
        let oh = (b[i + 1] - b'0') * 10 + (b[i + 2] - b'0');
        let om = (b[i + 4] - b'0') * 10 + (b[i + 5] - b'0');
        if oh > 23 || om > 59 {
            return false;
        }
        i += 6;
    } else {
        return false;
    }
    if i != b.len() {
        return false;
    }
    let two =
        |start: usize| -> i32 { (b[start] - b'0') as i32 * 10 + (b[start + 1] - b'0') as i32 };
    let year = (b[0] - b'0') as i32 * 1000
        + (b[1] - b'0') as i32 * 100
        + (b[2] - b'0') as i32 * 10
        + (b[3] - b'0') as i32;
    let month = two(5);
    let day = two(8);
    let hour = two(11);
    let minute = two(14);
    let second = two(17);
    if !(1..=12).contains(&month) {
        return false;
    }
    if day < 1 || day > days_in_month(year, month) {
        return false;
    }
    if hour > 23 || minute > 59 || second > 59 {
        return false;
    }
    true
}

/// Validate collector schema; return a detached, deterministically sorted copy.
pub fn validate_inventory(value: &Value) -> Result<Value, CatalogError> {
    validate_inventory_with(value, MAX_APPLICATIONS)
}

fn validate_inventory_with(value: &Value, max_applications: usize) -> Result<Value, CatalogError> {
    object(
        value,
        &[
            "schemaVersion",
            "os",
            "architecture",
            "collectedAt",
            "sources",
            "applications",
        ],
    )?;
    let map = value.as_object().expect("object() proves an object");
    if map["schemaVersion"].as_i64() != Some(1) {
        return Err(msg("invalid schema version"));
    }
    os_check(&map["os"])?;
    match map["architecture"].as_str() {
        Some("arm64") | Some("x86_64") => {}
        _ => return Err(msg("invalid architecture")),
    }
    text(&map["collectedAt"], 64, false)?;
    if !valid_timestamp(map["collectedAt"].as_str().expect("text() proves a string")) {
        return Err(msg("invalid collection timestamp"));
    }

    let sources = match &map["sources"] {
        Value::Array(a) if !a.is_empty() && a.len() <= 16 => a,
        _ => return Err(msg("invalid sources")),
    };
    let mut seen_source_ids: HashSet<&str> = HashSet::new();
    let mut new_sources: Vec<Value> = Vec::new();
    for source in sources {
        let source_map = match source {
            Value::Object(o) => o,
            _ => return Err(msg("invalid source fields")),
        };
        if !source_map.contains_key("id") || !source_map.contains_key("status") {
            return Err(msg("invalid source fields"));
        }
        if !source_map
            .keys()
            .all(|k| matches!(k.as_str(), "id" | "status" | "command" | "roots"))
        {
            return Err(msg("invalid source fields"));
        }
        text(&source_map["id"], 128, false)?;
        match source_map["status"].as_str() {
            Some("available") | Some("unavailable") => {}
            _ => return Err(msg("invalid source status")),
        }
        for field in ["command", "roots"] {
            if let Some(value) = source_map.get(field) {
                if source_map["status"].as_str() != Some("available") {
                    return Err(msg("unavailable source has provenance"));
                }
                let items = match value {
                    Value::Array(a) if !a.is_empty() && a.len() <= 32 => a,
                    _ => return Err(msg("invalid source provenance")),
                };
                for item in items {
                    match item {
                        Value::String(s) => {
                            let count = s.chars().count();
                            if count == 0
                                || count > 4096
                                || s.contains('\u{0}')
                                || s.chars().any(|c| (0xD800..=0xDFFF).contains(&(c as u32)))
                            {
                                return Err(msg("invalid source provenance text"));
                            }
                        }
                        _ => return Err(msg("invalid source provenance text")),
                    }
                }
            }
        }
        if source_map.contains_key("command") && source_map.contains_key("roots") {
            return Err(msg("ambiguous source provenance"));
        }
        let id = source_map["id"].as_str().expect("text() proves a string");
        if !seen_source_ids.insert(id) {
            return Err(msg("duplicate source"));
        }
        let mut copied = Map::new();
        for (k, v) in source_map {
            copied.insert(k.clone(), v.clone());
        }
        new_sources.push(Value::Object(copied));
    }

    let applications = match &map["applications"] {
        Value::Array(a) if a.len() <= max_applications => a,
        _ => return Err(msg("too many applications")),
    };
    let mut seen_app_ids: HashSet<String> = HashSet::new();
    let mut result_apps: Vec<Value> = Vec::new();
    for app in applications {
        object(app, &["id", "name", "aliases", "version"])?;
        let app_map = app.as_object().expect("object() proves an object");
        text(&app_map["id"], 128, false)?;
        text(&app_map["name"], 256, false)?;
        text(&app_map["version"], 256, true)?;
        let aliases = match &app_map["aliases"] {
            Value::Array(a) if a.len() <= 16 => a,
            _ => return Err(msg("invalid aliases")),
        };
        let mut alias_list: Vec<String> = Vec::new();
        for alias in aliases {
            text(alias, 256, false)?;
            alias_list.push(alias.as_str().expect("text() proves a string").to_string());
        }
        let id = app_map["id"].as_str().expect("text() proves a string");
        if !seen_app_ids.insert(id.to_string()) {
            return Err(msg("duplicate application ID"));
        }
        alias_list.sort();
        alias_list.dedup();
        let mut copied = app_map.clone();
        copied.insert(
            "aliases".to_string(),
            Value::Array(alias_list.into_iter().map(Value::String).collect()),
        );
        result_apps.push(Value::Object(copied));
    }
    result_apps.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));

    let mut out = map.clone();
    out.insert("sources".to_string(), Value::Array(new_sources));
    out.insert("applications".to_string(), Value::Array(result_apps));
    Ok(Value::Object(out))
}

fn validate_base(base: &Value) -> Result<(), CatalogError> {
    object(base, &["base_vm", "path", "files"])?;
    let map = base.as_object().expect("object() proves an object");
    key(&map["base_vm"])?;
    text(&map["path"], 4096, false)?;
    if !Path::new(map["path"].as_str().expect("text() proves a string")).is_absolute() {
        return Err(msg("base path must be absolute"));
    }
    let files = match &map["files"] {
        Value::Object(f) => {
            let has_all = f.len() == 3
                && f.contains_key("config.json")
                && f.contains_key("disk.img")
                && f.contains_key("nvram.bin");
            let has_required =
                f.len() == 2 && f.contains_key("config.json") && f.contains_key("disk.img");
            if has_all || has_required {
                f
            } else {
                return Err(msg("invalid base file set"));
            }
        }
        _ => return Err(msg("invalid base file set")),
    };
    for fields in files.values() {
        object(fields, &STAT_FIELDS)?;
        let field_map = fields.as_object().expect("object() proves an object");
        for (field, number) in field_map {
            let numeric = match number {
                Value::Number(n) => n,
                _ => return Err(msg("invalid base stat")),
            };
            let value = if let Some(i) = numeric.as_i64() {
                i as i128
            } else if let Some(u) = numeric.as_u64() {
                u as i128
            } else {
                return Err(msg("invalid base stat"));
            };
            let in_range = value >= -(2i128.pow(63)) && value < 2i128.pow(64);
            if !in_range || (field != "st_mtime_ns" && value < 0) {
                return Err(msg("invalid base stat"));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- documents

fn portable(doc: &Value, image: &str, kind: &str) -> Result<Value, CatalogError> {
    object(doc, &["schemaVersion", "image", "inventory", "provenance"])?;
    let map = doc.as_object().expect("object() proves an object");
    if map["schemaVersion"].as_i64() != Some(1) {
        return Err(msg("invalid schema version"));
    }
    if map["image"].as_str() != Some(image) {
        return Err(msg("inventory image mismatch"));
    }
    let inventory = validate_inventory(&map["inventory"])?;
    if inventory["os"].as_str() != Some(kind) {
        return Err(msg("inventory OS mismatch"));
    }
    let provenance = &map["provenance"];
    object(
        provenance,
        &[
            "extractionMode",
            "evidenceId",
            "rawSha256",
            "collectorSha256",
            "aliasesSha256",
        ],
    )?;
    let prov = provenance.as_object().expect("object() proves an object");
    match prov["extractionMode"].as_str() {
        Some("disposable-clone") | Some("work") | Some("base-maintenance") => {}
        _ => return Err(msg("invalid extraction mode")),
    }
    key(&prov["evidenceId"])?;
    for name in ["rawSha256", "collectorSha256", "aliasesSha256"] {
        hash_check(&prov[name])?;
    }
    Ok(inventory)
}

fn association(doc: &Value, image: &str, kind: &str) -> Result<(), CatalogError> {
    object(doc, &["schemaVersion", "image", "base", "inventorySha256"])?;
    let map = doc.as_object().expect("object() proves an object");
    if map["schemaVersion"].as_i64() != Some(2) {
        return Err(msg("invalid association schema"));
    }
    if map["image"].as_str() != Some(image) {
        return Err(msg("association image mismatch"));
    }
    validate_base(&map["base"])?;
    hash_check(&map["inventorySha256"])?;
    if kind != "linux" {
        let files = map["base"]["files"]
            .as_object()
            .expect("validate_base() proves files is an object");
        if !(files.len() == 3
            && files.contains_key("config.json")
            && files.contains_key("disk.img")
            && files.contains_key("nvram.bin"))
        {
            return Err(msg("missing macOS NVRAM fingerprint"));
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ JSON

struct DupValue(Value);

impl<'de> Deserialize<'de> for DupValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(DupVisitor).map(DupValue)
    }
}

struct DupVisitor;

impl<'de> Visitor<'de> for DupVisitor {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "a JSON value")
    }

    fn visit_bool<E>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }

    fn visit_i64<E>(self, v: i64) -> Result<Value, E> {
        Ok(Value::Number(v.into()))
    }

    fn visit_u64<E>(self, v: u64) -> Result<Value, E> {
        Ok(Value::Number(v.into()))
    }

    fn visit_f64<E>(self, v: f64) -> Result<Value, E> {
        Ok(Number::from_f64(v)
            .map(Value::Number)
            .unwrap_or(Value::Null))
    }

    fn visit_str<E>(self, v: &str) -> Result<Value, E> {
        Ok(Value::String(v.to_string()))
    }

    fn visit_string<E>(self, v: String) -> Result<Value, E> {
        Ok(Value::String(v))
    }

    fn visit_none<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        DupValue::deserialize(deserializer).map(|v| v.0)
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut out = Vec::new();
        while let Some(element) = seq.next_element::<DupValue>()? {
            out.push(element.0);
        }
        Ok(Value::Array(out))
    }

    fn visit_map<A>(self, mut access: A) -> Result<Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut map = Map::new();
        while let Some((k, v)) = access.next_entry::<String, DupValue>()? {
            if map.insert(k, v.0).is_some() {
                return Err(de::Error::custom("duplicate JSON key"));
            }
        }
        Ok(Value::Object(map))
    }
}

/// Decode raw UTF-8 JSON bytes, rejecting duplicate keys and JSON constants.
fn decode(raw: &[u8]) -> Result<Value, CatalogError> {
    let mut deserializer = serde_json::Deserializer::from_slice(raw);
    let value = DupValue::deserialize(&mut deserializer).map_err(|e| msg(&e.to_string()))?;
    deserializer.end().map_err(|e| msg(&e.to_string()))?;
    Ok(value.0)
}

// ----------------------------------------------------------------- reading

fn read_member(path: &Path, max_bytes: usize) -> Result<Vec<u8>, CatalogError> {
    no_symlinks(path)?;
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(msg("nonregular inventory refused"));
    }
    if metadata.len() > max_bytes as u64 {
        return Err(msg("inventory exceeds byte limit"));
    }
    let file = std::fs::File::open(path)?;
    let mut raw = Vec::new();
    file.take(max_bytes as u64 + 1).read_to_end(&mut raw)?;
    if raw.len() > max_bytes {
        return Err(msg("inventory exceeds byte limit"));
    }
    Ok(raw)
}

// ------------------------------------------------------------------ loading

fn malformed_error(image: &str, error: &CatalogError) -> CatalogError {
    msg(&format!(
        "malformed inventory pair for {}: {}",
        image,
        truncate_chars(&error.to_string(), 256)
    ))
}

/// Load a complete catalog or raise `CatalogError`; filesystem metadata reads only.
pub fn load_catalog(
    pilot_repo: &Path,
    lines: &Value,
    base_root: Option<&Path>,
    diagnostic: Option<&dyn Fn(&str)>,
) -> Result<Value, CatalogError> {
    load_catalog_impl(
        pilot_repo,
        lines,
        base_root,
        diagnostic,
        &Limits::default(),
        &default_fingerprint,
    )
}

fn load_catalog_impl(
    pilot_repo: &Path,
    lines: &Value,
    base_root: Option<&Path>,
    diagnostic: Option<&dyn Fn(&str)>,
    limits: &Limits,
    fingerprint: &dyn Fn(&Path, &str, &str) -> Result<Value, CatalogError>,
) -> Result<Value, CatalogError> {
    let lines_map = match lines {
        Value::Object(m) if m.len() <= limits.max_images => m,
        _ => return Err(msg("invalid image mapping")),
    };
    let root: PathBuf = match base_root {
        Some(path) => path.to_path_buf(),
        None => {
            let tart_home = match std::env::var_os("TART_HOME") {
                Some(v) => PathBuf::from(v),
                None => home_dir().join(".tart"),
            };
            expand_user(&tart_home).join("vms")
        }
    };
    for image in lines_map.keys() {
        key(&Value::String(image.clone()))?;
    }
    let mut sorted: Vec<&String> = lines_map.keys().collect();
    sorted.sort();

    let mut images: Vec<Value> = Vec::new();
    let mut missing_paths: Vec<String> = Vec::new();
    let mut count: usize = 0;

    for image in sorted {
        let cfg = &lines_map[image];
        let cfg_map = match cfg {
            Value::Object(m) if m.contains_key("kind") && m.contains_key("base_vm") => m,
            _ => return Err(msg("invalid line config")),
        };
        key(&cfg_map["base_vm"])?;
        os_check(&cfg_map["kind"])?;
        let kind = cfg_map["kind"]
            .as_str()
            .expect("os_check proves a string")
            .to_string();
        let base_vm = cfg_map["base_vm"]
            .as_str()
            .expect("key() proves a string")
            .to_string();

        let path = absolute_path(pilot_repo)
            .join("images")
            .join(image)
            .join("applications.json");
        let excluded = |reason: &str| {
            if let Some(diagnostic) = diagnostic {
                let message = format!("{}: {}", image, reason);
                diagnostic(&truncate_chars(&message, 512));
            }
        };
        let local_path = association_path(&root, image)?;

        let mut pair: Vec<Option<(Vec<u8>, Value, Value)>> = Vec::new();
        for (member, is_portable) in [(&path, true), (&local_path, false)] {
            match read_member(member, limits.max_bytes) {
                Ok(raw) => {
                    let parsed = decode(&raw).and_then(|decoded| {
                        if is_portable {
                            portable(&decoded, image, &kind)
                                .map(|validated| (decoded.clone(), validated))
                        } else {
                            association(&decoded, image, &kind)
                                .map(|()| (decoded.clone(), Value::Null))
                        }
                    });
                    match parsed {
                        Ok((decoded, validated)) => pair.push(Some((raw, decoded, validated))),
                        Err(error) => return Err(malformed_error(image, &error)),
                    }
                }
                Err(CatalogError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                    pair.push(None);
                    missing_paths.push(member.to_string_lossy().into_owned());
                    if let Some(diagnostic) = diagnostic {
                        diagnostic(&format!(
                            "{}: inventory pair member missing: {}",
                            image,
                            member.to_string_lossy()
                        ));
                    }
                }
                Err(error) => return Err(malformed_error(image, &error)),
            }
        }
        if pair.iter().any(|member| member.is_none()) {
            continue;
        }
        let (raw, _portable_decoded, inventory) = pair[0]
            .as_ref()
            .expect("checked that both members are present");
        let (association_raw, association_doc, _) = pair[1]
            .as_ref()
            .expect("checked that both members are present");

        if sha256_hex(raw) != association_doc["inventorySha256"].as_str().unwrap_or("") {
            excluded("inventory digest changed");
            continue;
        }
        let current = match fingerprint(&root, &base_vm, &kind) {
            Ok(value) => value,
            Err(_) => {
                excluded("base missing or unsafe");
                continue;
            }
        };
        if !same_base(&current, &association_doc["base"]) {
            excluded("base fingerprint changed");
            continue;
        }

        // Recheck reads with Python's short-circuit order.
        enum Recheck {
            Read(CatalogError),
            Base,
        }
        let recheck: Result<bool, Recheck> = (|| {
            let portable_now = read_member(&path, limits.max_bytes).map_err(Recheck::Read)?;
            if portable_now != *raw {
                return Ok(true);
            }
            let association_now =
                read_member(&local_path, limits.max_bytes).map_err(Recheck::Read)?;
            if association_now != *association_raw {
                return Ok(true);
            }
            let base_now = fingerprint(&root, &base_vm, &kind).map_err(|_| Recheck::Base)?;
            Ok(base_now != current)
        })();
        match recheck {
            Ok(true) => {
                excluded("inventory pair or base changed during read");
                continue;
            }
            Ok(false) => {}
            Err(Recheck::Read(CatalogError::Io(e))) if e.kind() == std::io::ErrorKind::NotFound => {
                excluded("inventory pair or base disappeared during read");
                continue;
            }
            Err(Recheck::Read(CatalogError::Message(m))) => {
                return Err(CatalogError::Message(m));
            }
            Err(Recheck::Read(CatalogError::Io(_))) => {
                return Err(msg("inventory recheck failed"));
            }
            Err(Recheck::Base) => {
                excluded("inventory pair or base disappeared during read");
                continue;
            }
        }

        count += inventory["applications"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0);
        if count > limits.max_applications {
            return Err(msg("catalog exceeds application limit"));
        }
        let inventory_map = inventory
            .as_object()
            .expect("validated inventory is an object");
        let mut entry = Map::new();
        entry.insert("image".to_string(), Value::String(image.clone()));
        for field in ["os", "architecture", "applications"] {
            entry.insert(field.to_string(), inventory_map[field].clone());
        }
        images.push(Value::Object(entry));
    }

    if images.is_empty() {
        let mut message = String::from("catalog unavailable: no usable inventories");
        if !missing_paths.is_empty() {
            message.push_str("; missing paths: ");
            message.push_str(&missing_paths.join(", "));
        }
        return Err(msg(&message));
    }

    let result = json!({"schemaVersion": 1, "images": images});
    if serialized_bytes(&result) > limits.max_bytes {
        return Err(msg("catalog exceeds byte limit"));
    }
    Ok(result)
}

// ------------------------------------------------------- python JSON sizing

fn write_python_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (c as u32) >= 0x7f => {
                let code = c as u32;
                if code <= 0xFFFF {
                    out.push_str(&format!("\\u{code:04x}"));
                } else {
                    let v = code - 0x10000;
                    let hi = 0xD800 + (v >> 10);
                    let lo = 0xDC00 + (v & 0x3FF);
                    out.push_str(&format!("\\u{hi:04x}\\u{lo:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Serialize exactly like Python `json.dumps(value, indent=2)` with default
/// `ensure_ascii=True`; used only for the output byte budget.
fn write_python_json(value: &Value, depth: usize, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => write_python_string(s, out),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push('\n');
                for _ in 0..(depth + 1) * 2 {
                    out.push(' ');
                }
                write_python_json(item, depth + 1, out);
            }
            out.push('\n');
            for _ in 0..depth * 2 {
                out.push(' ');
            }
            out.push(']');
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (index, (k, v)) in map.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push('\n');
                for _ in 0..(depth + 1) * 2 {
                    out.push(' ');
                }
                write_python_string(k, out);
                out.push_str(": ");
                write_python_json(v, depth + 1, out);
            }
            out.push('\n');
            for _ in 0..depth * 2 {
                out.push(' ');
            }
            out.push('}');
        }
    }
}

/// Length of `json.dumps(value, indent=2).encode() + b"\n"`.
fn serialized_bytes(value: &Value) -> usize {
    let mut out = String::new();
    write_python_json(value, 0, &mut out);
    out.len() + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::sync::{Mutex, MutexGuard};
    use tempfile::TempDir;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn env_guard() -> MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    struct Fixture {
        _guard: MutexGuard<'static, ()>,
        _tmp: TempDir,
        root: PathBuf,
        pilot: PathBuf,
        inventories: PathBuf,
        state: PathBuf,
        bases: PathBuf,
        base: PathBuf,
        lines: Value,
        doc: Value,
    }

    impl Fixture {
        fn new() -> Fixture {
            let guard = env_guard();
            let tmp = TempDir::new().unwrap();
            // Resolve macOS /var -> /private/var, not a fixture symlink under test.
            let root = tmp.path().canonicalize().unwrap();
            let pilot = root.join("pilot");
            let inventories = pilot.join("images");
            std::fs::create_dir_all(&inventories).unwrap();
            let state = root.join("state");
            let bases = root.join("vms");
            let base = bases.join("base");
            std::fs::create_dir_all(&base).unwrap();
            for name in NAMES {
                std::fs::write(base.join(name), b"base").unwrap();
            }
            std::env::set_var("PILOT_IMAGES_STATE_DIR", &state);
            std::env::set_var("HOME", &root);
            std::env::remove_var("TART_HOME");
            std::env::remove_var("XDG_STATE_HOME");

            let lines = json!({"linux1": {"kind": "linux", "base_vm": "base"}});
            let mut doc = json!({
                "schemaVersion": 1,
                "image": "linux1",
                "inventory": {
                    "schemaVersion": 1,
                    "os": "linux",
                    "architecture": "arm64",
                    "collectedAt": "2026-09-12T00:00:00Z",
                    "sources": [{"id": "dpkg", "status": "available"}],
                    "applications": []
                }
            });
            doc["base"] = fingerprint_base(&bases, "base", "linux").unwrap();
            let fixture = Fixture {
                _guard: guard,
                _tmp: tmp,
                root,
                pilot,
                inventories,
                state,
                bases,
                base,
                lines,
                doc,
            };
            fixture.save(None);
            fixture
        }

        fn save(&self, doc: Option<&Value>) {
            let mut doc = doc.cloned().unwrap_or_else(|| self.doc.clone());
            let base = doc
                .as_object_mut()
                .expect("fixture document is an object")
                .remove("base")
                .expect("fixture document has a base");
            doc["provenance"] = json!({
                "extractionMode": "work",
                "evidenceId": "fixture",
                "rawSha256": "0".repeat(64),
                "collectorSha256": "1".repeat(64),
                "aliasesSha256": "2".repeat(64),
            });
            let raw = serde_json::to_vec(&doc).unwrap();
            let image = doc["image"].as_str().unwrap().to_string();
            let portable = self.inventories.join(&image).join("applications.json");
            std::fs::create_dir_all(portable.parent().unwrap()).unwrap();
            std::fs::write(&portable, &raw).unwrap();

            let base_path = base["path"].as_str().unwrap();
            let local = association_path(Path::new(base_path).parent().unwrap(), &image).unwrap();
            std::fs::create_dir_all(local.parent().unwrap()).unwrap();
            let association_doc = json!({
                "schemaVersion": 2,
                "image": image,
                "base": base,
                "inventorySha256": sha256_hex(&raw),
            });
            std::fs::write(&local, serde_json::to_vec(&association_doc).unwrap()).unwrap();
        }

        fn load(&self, diagnostic: Option<&dyn Fn(&str)>) -> Result<Value, CatalogError> {
            load_catalog(&self.pilot, &self.lines, Some(&self.bases), diagnostic)
        }

        fn association(&self) -> PathBuf {
            association_path(&self.bases, "linux1").unwrap()
        }

        fn portable(&self) -> PathBuf {
            self.inventories.join("linux1").join("applications.json")
        }
    }

    fn snapshot(root: &Path) -> std::collections::BTreeMap<PathBuf, (Vec<u8>, i64)> {
        use std::os::unix::fs::MetadataExt;
        let mut out = std::collections::BTreeMap::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let entries: Vec<_> = std::fs::read_dir(&dir)
                .unwrap()
                .map(|entry| entry.unwrap())
                .collect();
            for entry in entries {
                let path = entry.path();
                let metadata = std::fs::metadata(&path).unwrap();
                if metadata.is_dir() {
                    stack.push(path);
                } else if metadata.is_file() {
                    let bytes = std::fs::read(&path).unwrap();
                    let ns = metadata.mtime() * 1_000_000_000 + metadata.mtime_nsec();
                    out.insert(path.strip_prefix(root).unwrap().to_path_buf(), (bytes, ns));
                }
            }
        }
        out
    }

    #[test]
    fn state_resolution_and_full_canonical_store_hash() {
        let fixture = Fixture::new();
        let store = fixture.root.join("vm-store-界");
        let canonical = resolve_path(&store);
        let digest = sha256_hex(canonical.as_os_str().as_bytes());
        let suffix = Path::new("stores")
            .join(&digest)
            .join("base")
            .join("linux1.json");
        std::env::set_var("XDG_STATE_HOME", fixture.root.join("xdg"));

        assert_eq!(
            association_path(&store, "linux1").unwrap(),
            fixture.state.join(&suffix)
        );
        assert_eq!(
            association_path(&store.join("../vm-store-界"), "linux1").unwrap(),
            fixture.state.join(&suffix)
        );
        let link = fixture.root.join("store-link");
        std::os::unix::fs::symlink(&store, &link).unwrap();
        assert_eq!(
            association_path(&link, "linux1").unwrap(),
            fixture.state.join(&suffix)
        );
        std::env::remove_var("PILOT_IMAGES_STATE_DIR");
        assert_eq!(
            association_path(&store, "linux1").unwrap(),
            fixture.root.join("xdg/pilot-images").join(&suffix)
        );
        std::env::remove_var("XDG_STATE_HOME");
        assert_eq!(
            association_path(&store, "linux1").unwrap(),
            fixture.root.join(".local/state/pilot-images").join(&suffix)
        );
        assert!(!store.exists());
    }

    #[test]
    fn default_vm_store_honors_tart_home() {
        let fixture = Fixture::new();
        std::env::set_var("TART_HOME", &fixture.root);
        assert_eq!(
            load_catalog(&fixture.pilot, &fixture.lines, None, None).unwrap()["schemaVersion"],
            1
        );
    }

    #[test]
    fn state_is_store_scoped_not_repository_or_base_scoped() {
        let fixture = Fixture::new();
        let other_repo = fixture.root.join("other-pilot");
        std::fs::rename(&fixture.pilot, &other_repo).unwrap();
        assert_eq!(
            load_catalog(&other_repo, &fixture.lines, Some(&fixture.bases), None).unwrap()
                ["schemaVersion"],
            1
        );
        assert_ne!(
            association_path(&fixture.bases, "linux1").unwrap(),
            association_path(&fixture.root.join("other-store"), "linux1").unwrap()
        );
        assert_ne!(
            association_path(&fixture.bases, "linux1").unwrap(),
            association_path(&fixture.base, "linux1").unwrap()
        );
    }

    #[test]
    fn missing_paths_are_exact_and_legacy_layout_is_not_used() {
        let fixture = Fixture::new();
        let legacy = fixture.pilot.join("inventories/local/base");
        std::fs::create_dir_all(&legacy).unwrap();
        let association = fixture.association();
        std::fs::rename(&association, legacy.join("linux1.json")).unwrap();
        let portable = fixture.portable();
        std::fs::rename(&portable, fixture.pilot.join("inventories/linux1.json")).unwrap();

        let messages: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let error = {
            let diagnostic = |message: &str| messages.borrow_mut().push(message.to_string());
            fixture.load(Some(&diagnostic)).unwrap_err()
        };
        let messages = messages.into_inner();
        for path in [&portable, &association] {
            let display = path.to_string_lossy().to_string();
            assert!(error.to_string().contains(&display));
            assert!(messages.iter().any(|message| message.contains(&display)));
        }
        assert!(!portable.exists());
        assert!(!association.exists());
    }

    #[test]
    fn missing_image_paths_are_reported_when_another_image_is_usable() {
        let fixture = Fixture::new();
        let mut lines = fixture.lines.clone();
        lines["missing"] = fixture.lines["linux1"].clone();
        let messages: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let result = {
            let diagnostic = |message: &str| messages.borrow_mut().push(message.to_string());
            load_catalog(
                &fixture.pilot,
                &lines,
                Some(&fixture.bases),
                Some(&diagnostic),
            )
            .unwrap()
        };
        let messages = messages.into_inner();
        let image_names: Vec<&str> = result["images"]
            .as_array()
            .unwrap()
            .iter()
            .map(|image| image["image"].as_str().unwrap())
            .collect();
        assert_eq!(image_names, vec!["linux1"]);
        for path in [
            fixture.inventories.join("missing/applications.json"),
            association_path(&fixture.bases, "missing").unwrap(),
        ] {
            let display = path.to_string_lossy().to_string();
            assert!(messages.iter().any(|message| message.contains(&display)));
            assert!(!path.exists());
        }
    }

    #[test]
    fn association_state_symlink_ancestor_is_refused() {
        let fixture = Fixture::new();
        let link = fixture.root.join("linked-state");
        std::os::unix::fs::symlink(&fixture.state, &link).unwrap();
        std::env::set_var("PILOT_IMAGES_STATE_DIR", &link);
        let error = fixture.load(None).unwrap_err();
        assert!(error.to_string().contains("symlink"));
    }

    #[test]
    fn association_symlink_and_malformed_orphan_are_errors() {
        let fixture = Fixture::new();
        let association = fixture.association();
        let raw = std::fs::read(&association).unwrap();
        std::fs::remove_file(&association).unwrap();
        std::os::unix::fs::symlink(fixture.root.join("missing"), &association).unwrap();
        assert!(fixture
            .load(None)
            .unwrap_err()
            .to_string()
            .contains("symlink"));

        std::fs::remove_file(&association).unwrap();
        std::fs::write(&association, "{}").unwrap();
        std::fs::remove_file(fixture.portable()).unwrap();
        assert!(fixture
            .load(None)
            .unwrap_err()
            .to_string()
            .contains("malformed"));
        std::fs::write(&association, &raw).unwrap();
    }

    #[test]
    fn blank_application_metadata_is_rejected() {
        let mut fixture = Fixture::new();
        let base_app = json!({"id": "firefox", "name": "Firefox", "aliases": [], "version": "1"});
        for field in ["id", "name", "version", "aliases"] {
            let invalid = if field == "aliases" {
                json!([" "])
            } else {
                json!(" ")
            };
            let mut app = base_app.clone();
            app[field] = invalid;
            fixture.doc["inventory"]["applications"] = json!([app]);
            fixture.save(None);
            assert!(fixture.load(None).is_err(), "field {field} accepted");
        }
    }

    #[test]
    fn empty_usable_and_two_positional_arguments() {
        let mut fixture = Fixture::new();
        assert_eq!(
            fixture.load(None).unwrap()["images"][0]["applications"],
            json!([])
        );

        let tart = fixture.root.join(".tart");
        std::fs::create_dir_all(&tart).unwrap();
        let tart_vms = tart.join("vms");
        std::fs::rename(&fixture.bases, &tart_vms).unwrap();
        fixture.doc["base"] = fingerprint_base(&tart_vms, "base", "linux").unwrap();
        fixture.save(None);
        assert_eq!(
            load_catalog(&fixture.pilot, &fixture.lines, None, None).unwrap()["schemaVersion"],
            1
        );
    }

    #[test]
    fn missing_inventory_and_all_missing() {
        let fixture = Fixture::new();
        let portable = fixture.portable();
        std::fs::remove_file(&portable).unwrap();
        let messages: RefCell<Vec<String>> = RefCell::new(Vec::new());
        {
            let diagnostic = |message: &str| messages.borrow_mut().push(message.to_string());
            let error = fixture.load(Some(&diagnostic)).unwrap_err();
            assert!(error.to_string().contains("no usable"));
        }
        assert_eq!(
            messages.into_inner(),
            vec![format!(
                "linux1: inventory pair member missing: {}",
                portable.to_string_lossy()
            )]
        );
    }

    #[test]
    fn changed_base_and_bounded_diagnostics() {
        let fixture = Fixture::new();
        std::fs::write(fixture.base.join("disk.img"), b"changed").unwrap();
        let messages: RefCell<Vec<String>> = RefCell::new(Vec::new());
        {
            let diagnostic = |message: &str| messages.borrow_mut().push(message.to_string());
            assert!(fixture.load(Some(&diagnostic)).is_err());
        }
        let messages = messages.into_inner();
        assert!(messages[0].contains("changed"));
        assert!(messages[0].chars().count() <= 512);
    }

    #[test]
    fn missing_base() {
        let fixture = Fixture::new();
        std::fs::remove_file(fixture.base.join("config.json")).unwrap();
        assert!(fixture
            .load(None)
            .unwrap_err()
            .to_string()
            .contains("no usable"));
    }

    #[test]
    fn exact_optional_nvram_set_and_ancillary_ignored() {
        let mut fixture = Fixture::new();
        std::fs::create_dir_all(fixture.base.join("control.sock")).unwrap();
        assert_eq!(fixture.load(None).unwrap()["images"][0]["image"], "linux1");

        std::fs::remove_file(fixture.base.join("nvram.bin")).unwrap();
        assert!(fixture.load(None).is_err());

        fixture.doc["base"] = fingerprint_base(&fixture.bases, "base", "linux").unwrap();
        fixture.save(None);
        fixture.load(None).unwrap();
        assert!(fingerprint_base(&fixture.bases, "base", "macos").is_err());

        std::fs::write(fixture.base.join("nvram.bin"), b"new").unwrap();
        assert!(fixture.load(None).is_err());
    }

    #[test]
    fn symlinks_and_nonregular_files() {
        let fixture = Fixture::new();
        let target = fixture.root.join("target");
        std::fs::write(&target, b"x").unwrap();
        for name in NAMES {
            let file = fixture.base.join(name);
            std::fs::remove_file(&file).unwrap();
            std::os::unix::fs::symlink(&target, &file).unwrap();
            assert!(fingerprint_base(&fixture.bases, "base", "linux").is_err());
            std::fs::remove_file(&file).unwrap();
            std::fs::write(&file, b"base").unwrap();
        }
        let linked = fixture.root.join("linked");
        std::os::unix::fs::symlink(&fixture.bases, &linked).unwrap();
        assert!(fingerprint_base(&linked, "base", "linux").is_err());

        let disk = fixture.base.join("disk.img");
        std::fs::remove_file(&disk).unwrap();
        std::fs::create_dir_all(&disk).unwrap();
        assert!(fingerprint_base(&fixture.bases, "base", "linux").is_err());
    }

    #[test]
    fn inventory_symlink_is_error_not_missing() {
        let fixture = Fixture::new();
        let portable = fixture.portable();
        std::fs::remove_file(&portable).unwrap();
        std::os::unix::fs::symlink(fixture.root.join("absent"), &portable).unwrap();
        assert!(fixture
            .load(None)
            .unwrap_err()
            .to_string()
            .contains("symlink"));
    }

    #[test]
    fn invalid_envelopes_and_fields_even_with_missing_base() {
        let fixture = Fixture::new();
        std::fs::remove_file(fixture.base.join("disk.img")).unwrap();
        type Mutation = Box<dyn Fn(&mut Value)>;
        let mutations: Vec<Mutation> = vec![
            Box::new(|d| d["unknown"] = json!(1)),
            Box::new(|d| d["schemaVersion"] = json!(true)),
            Box::new(|d| d["inventory"]["architecture"] = json!("wat")),
            Box::new(|d| d["inventory"]["os"] = json!("windows")),
            Box::new(|d| d["inventory"]["collectedAt"] = json!("yesterday")),
            Box::new(|d| d["inventory"]["sources"] = json!([{"id": "dpkg", "status": "failed"}])),
            Box::new(|d| d["base"]["files"]["disk.img"]["st_size"] = json!(true)),
            Box::new(|d| d["inventory"]["applications"] = json!([{"id": "x"}])),
        ];
        for mutate in mutations {
            let mut doc = fixture.doc.clone();
            mutate(&mut doc);
            fixture.save(Some(&doc));
            let error = fixture.load(None).unwrap_err();
            assert!(error.to_string().contains("malformed"), "{error}");
        }
    }

    #[test]
    fn bad_json_duplicate_keys_and_oversized_input() {
        let fixture = Fixture::new();
        let portable = fixture.portable();
        let cases: Vec<String> = vec![
            "{".to_string(),
            "{\"schemaVersion\":1,\"schemaVersion\":1}".to_string(),
            " ".repeat(MAX_BYTES + 1),
        ];
        for raw in &cases {
            std::fs::write(&portable, raw).unwrap();
            assert!(fixture.load(None).is_err(), "accepted {}", raw.len());
        }
    }

    #[test]
    fn traversal_rejected() {
        let fixture = Fixture::new();
        for key in ["../outside", "/tmp/x", ".", "..", "a/b", "a\\b"] {
            let mut lines = Map::new();
            lines.insert(key.to_string(), fixture.lines["linux1"].clone());
            assert!(load_catalog(
                &fixture.pilot,
                &Value::Object(lines),
                Some(&fixture.bases),
                None
            )
            .is_err());
            assert!(fingerprint_base(&fixture.bases, key, "linux").is_err());
        }
    }

    #[test]
    fn deterministic_order_and_no_state_mutations() {
        let mut fixture = Fixture::new();
        fixture.doc["inventory"]["applications"] = json!([
            {"id": "z", "name": "z", "aliases": ["z", "a"], "version": null},
            {"id": "a", "name": "a", "aliases": ["z", "a"], "version": null}
        ]);
        fixture.save(None);

        let mut second = fixture.doc.clone();
        second["image"] = json!("aaa");
        fixture.save(Some(&second));
        fixture.lines["aaa"] = fixture.lines["linux1"].clone();

        let before_lines = fixture.lines.clone();
        let before = snapshot(&fixture.root);
        let result = fixture.load(None).unwrap();

        let images: Vec<&str> = result["images"]
            .as_array()
            .unwrap()
            .iter()
            .map(|image| image["image"].as_str().unwrap())
            .collect();
        assert_eq!(images, vec!["aaa", "linux1"]);
        let ids: Vec<&str> = result["images"][0]["applications"]
            .as_array()
            .unwrap()
            .iter()
            .map(|app| app["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["a", "z"]);
        assert_eq!(snapshot(&fixture.root), before);
        assert_eq!(fixture.lines, before_lines);
    }

    #[test]
    fn output_budget_uses_ascii_indented_json_with_newline() {
        let mut fixture = Fixture::new();
        fixture.doc["inventory"]["applications"] = json!([
            {"id": "x", "name": "界".repeat(256), "aliases": [], "version": null}
        ]);
        fixture.save(None);
        let result = fixture.load(None).unwrap();
        let size = serialized_bytes(&result);

        let exact = Limits {
            max_bytes: size,
            ..Limits::default()
        };
        assert!(load_catalog_impl(
            &fixture.pilot,
            &fixture.lines,
            Some(&fixture.bases),
            None,
            &exact,
            &default_fingerprint,
        )
        .is_ok());

        let under = Limits {
            max_bytes: size - 1,
            ..Limits::default()
        };
        let error = load_catalog_impl(
            &fixture.pilot,
            &fixture.lines,
            Some(&fixture.bases),
            None,
            &under,
            &default_fingerprint,
        )
        .unwrap_err();
        assert!(error.to_string().contains("catalog exceeds byte"));
    }

    #[test]
    fn missing_association_excludes_but_malformed_portable_errors() {
        let fixture = Fixture::new();
        std::fs::remove_file(fixture.association()).unwrap();
        assert!(fixture
            .load(None)
            .unwrap_err()
            .to_string()
            .contains("no usable"));
        std::fs::write(fixture.portable(), "{}").unwrap();
        assert!(fixture
            .load(None)
            .unwrap_err()
            .to_string()
            .contains("malformed"));
    }

    #[test]
    fn complete_byte_digest_and_legacy_refusal() {
        let fixture = Fixture::new();
        let path = fixture.portable();
        let mut data = std::fs::read(&path).unwrap();
        data.push(b' ');
        std::fs::write(&path, &data).unwrap();
        let messages: RefCell<Vec<String>> = RefCell::new(Vec::new());
        {
            let diagnostic = |message: &str| messages.borrow_mut().push(message.to_string());
            assert!(fixture
                .load(Some(&diagnostic))
                .unwrap_err()
                .to_string()
                .contains("no usable"));
        }
        let messages = messages.into_inner();
        assert!(messages[0].contains("digest changed"));

        std::fs::write(&path, serde_json::to_vec(&fixture.doc).unwrap()).unwrap();
        assert!(fixture
            .load(None)
            .unwrap_err()
            .to_string()
            .contains("malformed"));
    }

    #[test]
    fn pair_replacement_during_fingerprint_excludes() {
        let fixture = Fixture::new();
        let association = fixture.association();
        let changing = move |root: &Path, vm: &str, os: &str| -> Result<Value, CatalogError> {
            let result = fingerprint_base(root, vm, os)?;
            let mut data = std::fs::read(&association).unwrap();
            data.push(b' ');
            std::fs::write(&association, &data).unwrap();
            Ok(result)
        };
        let error = load_catalog_impl(
            &fixture.pilot,
            &fixture.lines,
            Some(&fixture.bases),
            None,
            &Limits::default(),
            &changing,
        )
        .unwrap_err();
        assert!(error.to_string().contains("no usable"));
    }

    /// macOS numbers a volume when it mounts it, so after a reboot every
    /// unchanged base file reports a new `st_dev` (DC-17).
    #[test]
    fn renumbered_volume_keeps_the_inventory() {
        let mut fixture = Fixture::new();
        for stat in fixture.doc["base"]["files"]
            .as_object_mut()
            .unwrap()
            .values_mut()
        {
            stat["st_dev"] = json!(stat["st_dev"].as_u64().unwrap() + 3);
        }
        fixture.save(None);
        let result = fixture.load(None).unwrap();
        assert_eq!(result["images"].as_array().unwrap().len(), 1);

        // Any other stat field still marks the base as changed.
        for field in ["st_ino", "st_size", "st_mtime_ns"] {
            let mut doc = fixture.doc.clone();
            let stat = &mut doc["base"]["files"]["disk.img"];
            stat[field] = json!(stat[field].as_u64().unwrap() + 1);
            fixture.save(Some(&doc));
            let messages = RefCell::new(Vec::<String>::new());
            let record = |message: &str| messages.borrow_mut().push(message.to_string());
            assert!(fixture
                .load(Some(&record))
                .unwrap_err()
                .to_string()
                .contains("no usable"));
            assert!(messages.borrow()[0].contains("base fingerprint changed"));
        }
    }

    #[test]
    fn closed_provenance() {
        let fixture = Fixture::new();
        let path = fixture.portable();
        for change in [
            json!({"extra": true}),
            json!({"rawSha256": "A".repeat(64)}),
            json!({"evidenceId": "../escape"}),
            json!({"extractionMode": "legacy"}),
        ] {
            fixture.save(None);
            let text = std::fs::read_to_string(&path).unwrap();
            let mut doc: Value = serde_json::from_str(&text).unwrap();
            let provenance = doc["provenance"].as_object_mut().unwrap();
            for (k, v) in change.as_object().unwrap() {
                provenance.insert(k.clone(), v.clone());
            }
            std::fs::write(&path, serde_json::to_vec(&doc).unwrap()).unwrap();
            assert!(fixture
                .load(None)
                .unwrap_err()
                .to_string()
                .contains("malformed"));
        }
    }

    #[test]
    fn record_and_image_limits() {
        let mut fixture = Fixture::new();
        let image_limit = Limits {
            max_images: 0,
            ..Limits::default()
        };
        assert!(load_catalog_impl(
            &fixture.pilot,
            &fixture.lines,
            Some(&fixture.bases),
            None,
            &image_limit,
            &default_fingerprint,
        )
        .is_err());

        fixture.doc["inventory"]["applications"] =
            json!([{"id": "x", "name": "x", "aliases": [], "version": "1"}]);
        fixture.save(None);
        let app_limit = Limits {
            max_applications: 0,
            ..Limits::default()
        };
        assert!(load_catalog_impl(
            &fixture.pilot,
            &fixture.lines,
            Some(&fixture.bases),
            None,
            &app_limit,
            &default_fingerprint,
        )
        .is_err());
    }

    /// Port of the non-live portion of `test_collector_contract.py`: a document
    /// in the collector's canonical shape round-trips through `validate_inventory`.
    #[test]
    fn collector_document_shape_validates_unchanged() {
        let document = json!({
            "schemaVersion": 1,
            "os": "linux",
            "architecture": "arm64",
            "collectedAt": "2026-09-12T00:00:00Z",
            "sources": [
                {"id": "dpkg", "status": "available"},
                {"id": "packages", "status": "unavailable"}
            ],
            "applications": [
                {"id": "fixture-app", "name": "fixture-app", "aliases": [], "version": "1.2"}
            ]
        });
        assert_eq!(validate_inventory(&document).unwrap(), document);
        assert_eq!(document["applications"][0]["version"], "1.2");
        assert_eq!(document["sources"][1]["status"], "unavailable");
    }

    /// The live check requires the external `pilot-images` collector module
    /// named by `PILOT_INVENTORY_COLLECTOR`; the Python suite exercises it.
    #[test]
    #[ignore = "test_actual_collector_output_validates: requires PILOT_INVENTORY_COLLECTOR \
                pointing at the external pilot-images collector"]
    fn actual_collector_output_validates() {}
}
