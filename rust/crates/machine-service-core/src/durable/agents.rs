//! Agent keys and gateway endpoints for durable-machine sessions
//! (`docs/arch/agent-keys.md`, AK-1 to AK-5).
//!
//! The owner writes one key file per agent and one `endpoints.json` under the
//! instance's configuration folder. An agent is loaded only when both its key
//! and its endpoint are present and valid (AK-2), because a key without an
//! endpoint would be sent to its vendor's public endpoint.
//!
//! A key's value lives only in a [`Secret`], whose `Debug` prints
//! `[redacted]` and which has no `Display` or serialization, so no record,
//! log line, response or error can carry it by accident (AK-5). It reaches a
//! command only through that command's environment.

use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

/// The text that replaces a key's value in exec output (AK-5).
pub const REDACTED: &str = "[redacted]";
/// The endpoints file beside the key files (AK-2).
pub const ENDPOINTS_FILE: &str = "endpoints.json";
/// The largest key or endpoints file the service reads.
const MAX_FILE_BYTES: u64 = 64 * 1024;
/// The Codex model provider the per-session `config.toml` selects (AK-4).
pub const CODEX_PROVIDER: &str = "machine-service-litellm";

/// An agent that a durable session can carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    /// Claude Code (AK-3).
    ClaudeCode,
    /// Codex (AK-4).
    Codex,
}

impl Agent {
    /// Every agent, in report order.
    pub const ALL: [Agent; 2] = [Agent::ClaudeCode, Agent::Codex];

    /// The key in the `agents` capability and the lease record.
    pub fn report_key(self) -> &'static str {
        match self {
            Agent::ClaudeCode => "claude_code",
            Agent::Codex => "codex",
        }
    }

    /// The key file's name (AK-1).
    pub fn file(self) -> &'static str {
        match self {
            Agent::ClaudeCode => "claude-code.env",
            Agent::Codex => "codex.env",
        }
    }

    /// The one variable the key file assigns (AK-1).
    pub fn variable(self) -> &'static str {
        match self {
            Agent::ClaudeCode => "ANTHROPIC_API_KEY",
            Agent::Codex => "LITELLM_CODEX_KEY",
        }
    }

    /// The agent's entry in `endpoints.json` (AK-2).
    pub fn endpoint_key(self) -> &'static str {
        match self {
            Agent::ClaudeCode => "claude-code",
            Agent::Codex => "codex",
        }
    }
}

/// A key file's state (AK-1, MM-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyState {
    /// The file exists, is safe and holds exactly its one assignment.
    Present,
    /// The file does not exist.
    Absent,
    /// The file or its folder has group or other permission bits, or
    /// belongs to another user.
    UnsafeMode,
    /// The file exists but is not a regular file with exactly its one
    /// assignment.
    Invalid,
}

impl KeyState {
    /// The reported word.
    pub fn as_str(self) -> &'static str {
        match self {
            KeyState::Present => "present",
            KeyState::Absent => "absent",
            KeyState::UnsafeMode => "unsafe-mode",
            KeyState::Invalid => "invalid",
        }
    }
}

/// An endpoint's state (AK-2, MM-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointState {
    /// `endpoints.json` names a valid base URL for the agent.
    Present,
    /// The file, or the agent's entry in it, does not exist.
    Absent,
    /// The file is unsafe or malformed, or the agent's entry is not a valid
    /// base URL.
    Invalid,
}

impl EndpointState {
    /// The reported word.
    pub fn as_str(self) -> &'static str {
        match self {
            EndpointState::Present => "present",
            EndpointState::Absent => "absent",
            EndpointState::Invalid => "invalid",
        }
    }
}

/// A key's value. It has no `Display` and no serialization, and its `Debug`
/// prints `[redacted]`, so it cannot reach a record, a log or a response by
/// formatting (AK-5).
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// A value to keep out of logs and answers.
    pub fn new(value: impl Into<String>) -> Secret {
        Secret(value.into())
    }

    /// The value, for a command's environment and for redaction only.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(REDACTED)
    }
}

/// One agent's key and endpoint as found on disk.
#[derive(Debug, Clone)]
pub struct AgentStatus {
    /// The agent.
    pub agent: Agent,
    /// The key file's state.
    pub key: KeyState,
    /// The endpoint's state.
    pub endpoint: EndpointState,
    /// The key file's path.
    pub key_path: PathBuf,
    secret: Option<Secret>,
    base_url: Option<String>,
}

impl AgentStatus {
    /// Whether the agent is loaded: both key and endpoint are present (AK-2).
    pub fn loaded(&self) -> bool {
        self.key == KeyState::Present
            && self.endpoint == EndpointState::Present
            && self.secret.is_some()
            && self.base_url.is_some()
    }

    /// The configured base URL, exactly as written (AK-2).
    pub fn base_url(&self) -> Option<&str> {
        self.base_url.as_deref()
    }
}

/// Every agent's key and endpoint state on this Mac.
#[derive(Debug, Clone)]
pub struct AgentKeys {
    /// The agents folder.
    pub dir: PathBuf,
    /// The endpoints file's path.
    pub endpoints_path: PathBuf,
    /// One status per agent, in [`Agent::ALL`] order.
    pub agents: Vec<AgentStatus>,
}

impl AgentKeys {
    /// The status of one agent.
    pub fn status(&self, agent: Agent) -> Option<&AgentStatus> {
        self.agents.iter().find(|status| status.agent == agent)
    }

    /// The `agents` capability section: states only, never a key (MM-3).
    pub fn capability(&self) -> Value {
        let mut out = Map::new();
        for status in &self.agents {
            out.insert(
                status.agent.report_key().to_string(),
                json!({"key": status.key.as_str(), "endpoint": status.endpoint.as_str()}),
            );
        }
        Value::Object(out)
    }

    /// The lease record's agent states: state words and the key file's path,
    /// never a key (AK-5).
    pub fn record(&self) -> Value {
        let mut out = Map::new();
        for status in &self.agents {
            out.insert(
                status.agent.report_key().to_string(),
                json!({
                    "key": status.key.as_str(),
                    "endpoint": status.endpoint.as_str(),
                    "loaded": status.loaded(),
                    "key_path": status.key_path.to_string_lossy(),
                }),
            );
        }
        Value::Object(out)
    }

    /// The session variables of the loaded agents (AK-3, AK-4).
    /// `CODEX_HOME` is set by the session itself (DM-1).
    pub fn environment(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for status in self.agents.iter().filter(|status| status.loaded()) {
            let (Some(secret), Some(base_url)) = (&status.secret, &status.base_url) else {
                continue;
            };
            match status.agent {
                Agent::ClaudeCode => {
                    out.push(("ANTHROPIC_BASE_URL".to_string(), base_url.clone()));
                    out.push((
                        status.agent.variable().to_string(),
                        secret.expose().to_string(),
                    ));
                }
                Agent::Codex => {
                    out.push((
                        status.agent.variable().to_string(),
                        secret.expose().to_string(),
                    ));
                }
            }
        }
        out
    }

    /// The per-session Codex `config.toml` when Codex is loaded (AK-4). It
    /// names the key's variable through `env_key` and never the key.
    pub fn codex_config(&self) -> Option<String> {
        let status = self.status(Agent::Codex).filter(|status| status.loaded())?;
        let base_url = status.base_url.as_deref()?;
        Some(format!(
            "model_provider = \"{CODEX_PROVIDER}\"\n\n\
[model_providers.{CODEX_PROVIDER}]\n\
name = \"LiteLLM (machine-service)\"\n\
base_url = \"{}\"\n\
env_key = \"{}\"\n\
wire_api = \"responses\"\n",
            toml_escape(base_url),
            Agent::Codex.variable()
        ))
    }

    /// The loaded keys' values, for output redaction only (AK-5).
    pub fn secrets(&self) -> Vec<Secret> {
        self.agents
            .iter()
            .filter(|status| status.loaded())
            .filter_map(|status| status.secret.clone())
            .collect()
    }
}

/// Read every agent's key and endpoint from `agents_dir`, checking modes and
/// ownership against `uid` (AK-1, AK-2). Never fails: every problem is a
/// state.
pub fn inspect(agents_dir: &Path, uid: u32) -> AgentKeys {
    let endpoints_path = agents_dir.join(ENDPOINTS_FILE);
    let dir_state = folder_state(agents_dir, uid);
    let endpoints = read_endpoints(&endpoints_path, uid, dir_state);
    let agents = Agent::ALL
        .iter()
        .map(|agent| {
            let key_path = agents_dir.join(agent.file());
            let (key, secret) = read_key(&key_path, *agent, uid, dir_state);
            let (endpoint, base_url) = match &endpoints {
                Endpoints::Absent => (EndpointState::Absent, None),
                Endpoints::Invalid => (EndpointState::Invalid, None),
                Endpoints::Read(map) => match map.get(agent.endpoint_key()) {
                    None => (EndpointState::Absent, None),
                    Some(entry) => match valid_base_url(entry) {
                        Some(url) => (EndpointState::Present, Some(url)),
                        None => (EndpointState::Invalid, None),
                    },
                },
            };
            AgentStatus {
                agent: *agent,
                key,
                endpoint,
                key_path,
                secret,
                base_url,
            }
        })
        .collect();
    AgentKeys {
        dir: agents_dir.to_path_buf(),
        endpoints_path,
        agents,
    }
}

/// Replace every occurrence of each secret in `text` with `[redacted]`
/// (AK-5). Longer secrets go first, so a key that contains another is
/// replaced whole.
pub fn redact(text: &str, secrets: &[Secret]) -> String {
    let mut ordered: Vec<&str> = secrets
        .iter()
        .map(Secret::expose)
        .filter(|value| !value.is_empty())
        .collect();
    ordered.sort_by_key(|value| std::cmp::Reverse(value.len()));
    let mut out = text.to_string();
    for value in ordered {
        if out.contains(value) {
            out = out.replace(value, REDACTED);
        }
    }
    out
}

/// [`redact`] over a stream that arrives in pieces (DC-221): every
/// occurrence of a value becomes [`REDACTED`], also one split between two
/// pieces. It holds back only the end of a piece that could begin a value,
/// so output that cannot hold one passes at once.
pub struct Redactor {
    /// The values, longest first, so a value that begins another loses.
    values: Vec<Vec<u8>>,
    /// Whether some value begins with the byte.
    first: [bool; 256],
    held: Vec<u8>,
}

impl Redactor {
    /// A redactor of `values`; empty values are ignored.
    pub fn new<V: AsRef<[u8]>>(values: impl IntoIterator<Item = V>) -> Redactor {
        let mut values: Vec<Vec<u8>> = values
            .into_iter()
            .map(|value| value.as_ref().to_vec())
            .filter(|value| !value.is_empty())
            .collect();
        values.sort_by_key(|value| std::cmp::Reverse(value.len()));
        values.dedup();
        let mut first = [false; 256];
        for value in &values {
            first[value[0] as usize] = true;
        }
        Redactor {
            values,
            first,
            held: Vec::new(),
        }
    }

    /// Whether there is nothing to redact.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// The redacted bytes of `piece` that can go out now.
    pub fn push(&mut self, piece: &[u8]) -> Vec<u8> {
        self.held.extend_from_slice(piece);
        self.scan(false)
    }

    /// The rest, at the end of the stream.
    pub fn finish(mut self) -> Vec<u8> {
        self.scan(true)
    }

    fn scan(&mut self, end: bool) -> Vec<u8> {
        let bytes = std::mem::take(&mut self.held);
        let mut out = Vec::with_capacity(bytes.len());
        let (mut start, mut at) = (0, 0);
        while at < bytes.len() {
            if !self.first[bytes[at] as usize] {
                at += 1;
                continue;
            }
            let rest = &bytes[at..];
            // A longer value that the next piece may complete is waited
            // for before a shorter one that matches already.
            let begins = |value: &Vec<u8>| value.len() > rest.len() && value.starts_with(rest);
            if !end && self.values.iter().any(begins) {
                out.extend_from_slice(&bytes[start..at]);
                self.held = rest.to_vec();
                return out;
            }
            if let Some(value) = self.values.iter().find(|value| rest.starts_with(value)) {
                out.extend_from_slice(&bytes[start..at]);
                out.extend_from_slice(REDACTED.as_bytes());
                at += value.len();
                start = at;
                continue;
            }
            at += 1;
        }
        out.extend_from_slice(&bytes[start..]);
        out
    }
}

/// The agents folder's state for the files inside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FolderState {
    Missing,
    Safe,
    Unsafe,
    NotAFolder,
}

fn folder_state(dir: &Path, uid: u32) -> FolderState {
    match std::fs::symlink_metadata(dir) {
        Err(_) => FolderState::Missing,
        Ok(metadata) if !metadata.is_dir() => FolderState::NotAFolder,
        Ok(metadata) if metadata.uid() != uid || metadata.mode() & 0o077 != 0 => {
            FolderState::Unsafe
        }
        Ok(_) => FolderState::Safe,
    }
}

/// Open a regular file without following a symlink, checking that the
/// opened file is the one that was inspected.
fn open_regular(path: &Path, expected: &std::fs::Metadata) -> Option<Vec<u8>> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .ok()?;
    let opened = file.metadata().ok()?;
    if opened.ino() != expected.ino() || opened.dev() != expected.dev() || !opened.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return None;
    }
    Some(bytes)
}

fn read_key(
    path: &Path,
    agent: Agent,
    uid: u32,
    folder: FolderState,
) -> (KeyState, Option<Secret>) {
    if folder == FolderState::Missing {
        return (KeyState::Absent, None);
    }
    let metadata = match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return (KeyState::Absent, None)
        }
        Err(_) => return (KeyState::Invalid, None),
        Ok(metadata) => metadata,
    };
    if folder == FolderState::NotAFolder || !metadata.is_file() {
        return (KeyState::Invalid, None);
    }
    if folder == FolderState::Unsafe || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return (KeyState::UnsafeMode, None);
    }
    let Some(bytes) = open_regular(path, &metadata) else {
        return (KeyState::Invalid, None);
    };
    match parse_key_file(&bytes, agent.variable()) {
        Some(secret) => (KeyState::Present, Some(secret)),
        None => (KeyState::Invalid, None),
    }
}

/// Parse a key file: exactly one `VARIABLE=value` line, plus blank lines and
/// lines starting with `#` (AK-1). The value may be wrapped in one pair of
/// matching quotes, and must be non-empty without whitespace or control
/// characters.
pub fn parse_key_file(bytes: &[u8], variable: &str) -> Option<Secret> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut found: Option<Secret> = None;
    for raw in text.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let (name, value) = line.split_once('=')?;
        if name != variable || found.is_some() {
            return None;
        }
        let value = unquote(value);
        if value.is_empty()
            || value
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || c == '"' || c == '\'')
        {
            return None;
        }
        found = Some(Secret(value.to_string()));
    }
    found
}

fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if value.len() >= 2 && value.starts_with(quote) && value.ends_with(quote) {
            return &value[1..value.len() - 1];
        }
    }
    value
}

enum Endpoints {
    Absent,
    Invalid,
    Read(Map<String, Value>),
}

fn read_endpoints(path: &Path, uid: u32, folder: FolderState) -> Endpoints {
    if folder == FolderState::Missing {
        return Endpoints::Absent;
    }
    let metadata = match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Endpoints::Absent,
        Err(_) => return Endpoints::Invalid,
        Ok(metadata) => metadata,
    };
    // The file holds no secret, but it decides where a key is sent, so it
    // must be the owner's and not writable by group or other (AK-2).
    if folder == FolderState::NotAFolder
        || !metadata.is_file()
        || metadata.uid() != uid
        || metadata.mode() & 0o022 != 0
    {
        return Endpoints::Invalid;
    }
    let Some(bytes) = open_regular(path, &metadata) else {
        return Endpoints::Invalid;
    };
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(map)) => Endpoints::Read(map),
        _ => Endpoints::Invalid,
    }
}

/// A valid `{"base_url": "..."}` entry: an `http` or `https` URL with a host,
/// no whitespace, control characters, quotes or backslashes. It is used
/// exactly as written (AK-2).
fn valid_base_url(entry: &Value) -> Option<String> {
    let url = entry.get("base_url")?.as_str()?;
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))?;
    if rest.is_empty()
        || rest.starts_with('/')
        || url
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '"' || c == '\\' || c == '\'')
    {
        return None;
    }
    Some(url.to_string())
}

fn toml_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// DC-221: a stream is redacted as a whole would be, however
    /// it is split, and only a possible beginning of a value is held back.
    #[test]
    fn a_stream_is_redacted_however_it_is_split() {
        let values = ["sk-secret-123", "sk-secret-123456", "zz"];
        let text = b"a sk-secret-123456 b sk-secret-12 sk-secret-123 zzz s";
        let want = b"a [redacted] b sk-secret-12 [redacted] [redacted]z s".to_vec();
        for size in 1..text.len() {
            let mut redactor = Redactor::new(values);
            let mut out = Vec::new();
            for piece in text.chunks(size) {
                out.extend(redactor.push(piece));
            }
            out.extend(redactor.finish());
            assert_eq!(
                String::from_utf8_lossy(&out),
                String::from_utf8_lossy(&want),
                "pieces of {size}"
            );
        }
        let mut redactor = Redactor::new(values);
        assert_eq!(redactor.push(b"plain text\n"), b"plain text\n");
        assert_eq!(redactor.push(b"key sk-sec"), b"key ");
        assert_eq!(redactor.push(b"ond"), b"sk-second");
        assert!(Redactor::new([""]).is_empty());
    }

    const CLAUDE_KEY: &str = "sk-fake-claude-0123456789";
    const CODEX_KEY: &str = "sk-fake-codex-9876543210";

    fn uid() -> u32 {
        // SAFETY: `geteuid` only reads process state.
        unsafe { libc::geteuid() }
    }

    fn write(path: &Path, content: &str, mode: u32) {
        std::fs::write(path, content).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    fn folder(mode: u32) -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("agents");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode)).unwrap();
        (root, dir)
    }

    fn both_keys(dir: &Path) {
        write(
            &dir.join("claude-code.env"),
            &format!("# Claude Code\nANTHROPIC_API_KEY={CLAUDE_KEY}\n"),
            0o600,
        );
        write(
            &dir.join("codex.env"),
            &format!("LITELLM_CODEX_KEY=\"{CODEX_KEY}\"\n\n"),
            0o600,
        );
    }

    fn endpoints(dir: &Path, mode: u32) {
        write(
            &dir.join(ENDPOINTS_FILE),
            r#"{"claude-code": {"base_url": "http://<gateway-host>"},
                "codex": {"base_url": "http://<gateway-host>/v1"}}"#,
            mode,
        );
    }

    /// AC-24, AC-25 (TL-1): both keys and both endpoints load, and the
    /// session variables carry the endpoints exactly as written.
    #[test]
    fn ac24_ac25_keys_and_endpoints_load() {
        let (_root, dir) = folder(0o700);
        both_keys(&dir);
        endpoints(&dir, 0o644);
        let keys = inspect(&dir, uid());
        assert!(keys.agents.iter().all(AgentStatus::loaded));
        let env: std::collections::HashMap<_, _> = keys.environment().into_iter().collect();
        assert_eq!(env["ANTHROPIC_BASE_URL"], "http://<gateway-host>");
        assert_eq!(env["ANTHROPIC_API_KEY"], CLAUDE_KEY);
        assert_eq!(env["LITELLM_CODEX_KEY"], CODEX_KEY);
        assert!(!env.contains_key("ANTHROPIC_AUTH_TOKEN"));
        let config = keys.codex_config().unwrap();
        assert!(config.contains("base_url = \"http://<gateway-host>/v1\""));
        assert!(config.contains("env_key = \"LITELLM_CODEX_KEY\""));
        assert!(config.contains("model_provider = \"machine-service-litellm\""));
        assert!(!config.contains(CODEX_KEY));
        assert!(!config.contains("model ="), "the file pins no model");
    }

    /// AC-26 (TL-1): no report, record or debug print carries a key, and
    /// redaction replaces each occurrence.
    #[test]
    fn ac26_keys_never_reach_reports_and_are_redacted() {
        let (_root, dir) = folder(0o700);
        both_keys(&dir);
        endpoints(&dir, 0o600);
        let keys = inspect(&dir, uid());
        for text in [
            keys.capability().to_string(),
            keys.record().to_string(),
            format!("{keys:?}"),
        ] {
            assert!(!text.contains(CLAUDE_KEY), "{text}");
            assert!(!text.contains(CODEX_KEY), "{text}");
        }
        let output = format!("{CLAUDE_KEY} {CODEX_KEY}\nagain {CLAUDE_KEY}");
        let redacted = redact(&output, &keys.secrets());
        assert_eq!(redacted, "[redacted] [redacted]\nagain [redacted]");
    }

    /// AC-27 (TL-1): a key with mode 644 is unsafe-mode and a missing one is
    /// absent; neither is loaded.
    #[test]
    fn ac27_unsafe_and_missing_keys_are_reported_not_loaded() {
        let (_root, dir) = folder(0o700);
        write(
            &dir.join("codex.env"),
            &format!("LITELLM_CODEX_KEY={CODEX_KEY}\n"),
            0o644,
        );
        endpoints(&dir, 0o600);
        let keys = inspect(&dir, uid());
        assert_eq!(
            keys.capability(),
            json!({
                "claude_code": {"key": "absent", "endpoint": "present"},
                "codex": {"key": "unsafe-mode", "endpoint": "present"},
            })
        );
        assert!(keys.environment().is_empty());
        assert!(keys.secrets().is_empty());
        assert!(keys.codex_config().is_none());
    }

    /// AC-27 (TL-1): a folder with group or other bits makes every key file
    /// in it unsafe-mode.
    #[test]
    fn ac27_unsafe_folder_makes_keys_unsafe() {
        let (_root, dir) = folder(0o755);
        both_keys(&dir);
        endpoints(&dir, 0o600);
        let keys = inspect(&dir, uid());
        for status in &keys.agents {
            assert_eq!(status.key, KeyState::UnsafeMode);
            assert!(!status.loaded());
        }
    }

    /// AC-44 (TL-1): a key without an endpoint is reported present with an
    /// absent endpoint, and is not loaded.
    #[test]
    fn ac44_key_without_endpoint_is_not_loaded() {
        let (_root, dir) = folder(0o700);
        both_keys(&dir);
        let keys = inspect(&dir, uid());
        assert_eq!(
            keys.capability(),
            json!({
                "claude_code": {"key": "present", "endpoint": "absent"},
                "codex": {"key": "present", "endpoint": "absent"},
            })
        );
        assert!(keys.environment().is_empty());
    }

    /// AK-2: an endpoints file writable by group or other, or a bad URL, is
    /// invalid and loads nothing.
    #[test]
    fn ak2_unsafe_or_malformed_endpoints_are_invalid() {
        let (_root, dir) = folder(0o700);
        both_keys(&dir);
        endpoints(&dir, 0o664);
        let keys = inspect(&dir, uid());
        assert!(keys
            .agents
            .iter()
            .all(|status| status.endpoint == EndpointState::Invalid && !status.loaded()));

        write(
            &dir.join(ENDPOINTS_FILE),
            r#"{"claude-code": {"base_url": "ftp://x"}, "codex": {"base_url": "http://h\"x"}}"#,
            0o600,
        );
        let keys = inspect(&dir, uid());
        assert!(keys
            .agents
            .iter()
            .all(|status| status.endpoint == EndpointState::Invalid));
        write(&dir.join(ENDPOINTS_FILE), "not json", 0o600);
        assert!(inspect(&dir, uid())
            .agents
            .iter()
            .all(|status| status.endpoint == EndpointState::Invalid));
    }

    /// AK-1: anything but exactly one assignment of the expected variable is
    /// invalid.
    #[test]
    fn ak1_key_file_grammar() {
        let ok = |text: &str| parse_key_file(text.as_bytes(), "ANTHROPIC_API_KEY");
        assert_eq!(ok("ANTHROPIC_API_KEY=abc\n").unwrap().expose(), "abc");
        assert_eq!(
            ok("\n# c\n  \nANTHROPIC_API_KEY='abc'\r\n")
                .unwrap()
                .expose(),
            "abc"
        );
        assert!(ok("").is_none());
        assert!(ok("ANTHROPIC_API_KEY=\n").is_none());
        assert!(ok("ANTHROPIC_API_KEY=a\nANTHROPIC_API_KEY=b\n").is_none());
        assert!(ok("export ANTHROPIC_API_KEY=a\n").is_none());
        assert!(ok("OTHER=a\n").is_none());
        assert!(ok("ANTHROPIC_API_KEY=a b\n").is_none());
        assert!(ok("ANTHROPIC_API_KEY=a\nstray\n").is_none());
    }

    /// AK-1: a missing folder reports everything absent; a symlinked key is
    /// invalid and never followed.
    #[test]
    fn ak1_missing_folder_and_symlinked_key() {
        let root = tempfile::tempdir().unwrap();
        let keys = inspect(&root.path().join("agents"), uid());
        assert!(keys.agents.iter().all(
            |status| status.key == KeyState::Absent && status.endpoint == EndpointState::Absent
        ));

        let (_root, dir) = folder(0o700);
        let target = dir.join("real");
        write(&target, &format!("ANTHROPIC_API_KEY={CLAUDE_KEY}\n"), 0o600);
        std::os::unix::fs::symlink(&target, dir.join("claude-code.env")).unwrap();
        let keys = inspect(&dir, uid());
        assert_eq!(
            keys.status(Agent::ClaudeCode).unwrap().key,
            KeyState::Invalid
        );
    }
}
