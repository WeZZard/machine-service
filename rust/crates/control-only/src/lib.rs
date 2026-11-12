//! Offline control-only contract and command generation.
//!
//! Port of `bin/control_only.py`.
//!
//! Not a deployment switch. The production host attachment/privilege auditor
//! has not been implemented; acquisition MUST fail before clone/boot until it
//! is reviewed. No client JSON, pid alone, guest `id` output, or digest alone
//! enables capability.

use std::collections::BTreeSet;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

/// Capability name advertised by this contract.
pub const CAPABILITY: &str = "vm-service.control-only.v1";
/// Reviewed Softnet profile identifier.
pub const PROFILE: &str = "softnet-0.23.0.ssh22-dhcp-arp.no-bootpd-write.v1";
/// Pinned upstream source commit.
pub const SOURCE: &str = "e5fd48cf033ed0ec376710607187d761e60c2374";
/// SHA-256 over [`policy`] serialized with sorted keys and no spaces.
pub const POLICY_SHA256: &str = "f56feb327ec84db945e3328fe95431b4e4978b607abb6ea03ca877c94eef8e0b";
/// Accepted acquisition request field names.
pub const FIELDS: [&str; 17] = [
    "purpose",
    "image",
    "line",
    "env",
    "pack",
    "lane",
    "ttl_hours",
    "cpu",
    "memory_mb",
    "disk_gb",
    "wait",
    "network",
    "profile",
    "source",
    "expected_source_fingerprint",
    "vnc",
    "egress",
];

/// Host binding fields a [`LeaseGuard`] requires.
pub const REQUIRED: [&str; 14] = [
    "clone",
    "lease",
    "mac",
    "ip",
    "attachment",
    "helper_pid",
    "helper_start",
    "boot_start",
    "binary_sha256",
    "source_commit",
    "patch_sha256",
    "policy_sha256",
    "daemon_generation",
    "runtime_kernel_identity",
];

/// The reviewed control-only policy record.
pub fn policy() -> Value {
    json!({
        "network": "control-only",
        "env": "none",
        "housekeeping": "validated-dhcp-arp",
        "hostAdmission": "tcp22-exact-return",
        "bootpdWrites": false,
        "profile": PROFILE,
        "ipv6": false,
        "fragments": false,
        "shares": false,
        "expose": false,
        "alternateNICs": false,
    })
}

/// Error raised when a request, host binding, or helper path is rejected.
///
/// The `Display` text is exactly the `Rejected` text raised by the Python
/// implementation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Rejected {
    /// A single human-readable rejection reason.
    #[error("{0}")]
    Message(String),
}

impl Rejected {
    /// Build a rejection with the given Python-compatible message.
    pub fn new(message: impl Into<String>) -> Self {
        Rejected::Message(message.into())
    }
}

/// Error returned by [`LeaseGuard::release`].
#[derive(Debug)]
pub enum GuardError {
    /// The guard rejected the release transition.
    Rejected(Rejected),
    /// A host-supplied callback failed; the failure propagates unchanged.
    Callback(Box<dyn std::error::Error + Send + Sync>),
}

impl std::fmt::Display for GuardError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GuardError::Rejected(rejected) => rejected.fmt(formatter),
            GuardError::Callback(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for GuardError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            GuardError::Rejected(rejected) => Some(rejected),
            GuardError::Callback(error) => Some(error.as_ref()),
        }
    }
}

impl From<Rejected> for GuardError {
    fn from(rejected: Rejected) -> Self {
        GuardError::Rejected(rejected)
    }
}

impl From<Box<dyn std::error::Error + Send + Sync>> for GuardError {
    fn from(error: Box<dyn std::error::Error + Send + Sync>) -> Self {
        GuardError::Callback(error)
    }
}

/// Validate an acquisition request and return the network it selects.
pub fn validate_request(body: &Value) -> Result<String, Rejected> {
    let object = body
        .as_object()
        .ok_or_else(|| Rejected::new("acquisition must be an object"))?;

    let known: BTreeSet<&str> = FIELDS.iter().copied().collect();
    let mut unknown: Vec<&str> = object
        .keys()
        .filter(|key| !known.contains(key.as_str()))
        .map(String::as_str)
        .collect();
    if !unknown.is_empty() {
        unknown.sort_unstable();
        return Err(Rejected::new(format!(
            "unknown acquisition fields: {}",
            unknown.join(", ")
        )));
    }

    for names in [&["image", "line"][..], &["env", "pack", "lane"][..]] {
        let present = names
            .iter()
            .filter(|name| object.contains_key(**name))
            .count();
        if present > 1 {
            return Err(Rejected::new(format!(
                "ambiguous aliases: {}",
                names.join(", ")
            )));
        }
    }

    let source = match object.get("source") {
        None => Some("base"),
        Some(value) => value.as_str(),
    };
    if source != Some("base") && source != Some("work") {
        return Err(Rejected::new("source must be base or work"));
    }
    if source == Some("work") && !value_is(object.get("env"), "none") {
        return Err(Rejected::new("work source requires explicit env:none"));
    }
    if object.contains_key("expected_source_fingerprint")
        && (source != Some("work")
            || !object
                .get("expected_source_fingerprint")
                .is_some_and(Value::is_object))
    {
        return Err(Rejected::new(
            "expected_source_fingerprint requires source=work and an object",
        ));
    }

    let network = match object.get("network") {
        None => Some("nat"),
        Some(value) => value.as_str(),
    };
    if network != Some("nat") && network != Some("control-only") {
        return Err(Rejected::new("network must be nat or control-only"));
    }
    if network == Some("control-only") {
        if !value_is(object.get("env"), "none")
            || object.contains_key("pack")
            || object.contains_key("lane")
        {
            return Err(Rejected::new(
                "control-only requires explicit env:none (not an alias)",
            ));
        }
        let profile_ok = match object.get("profile") {
            None => true,
            Some(value) => value_is(Some(value), PROFILE),
        };
        if !profile_ok {
            return Err(Rejected::new("unsupported control-only profile"));
        }
    } else if object.contains_key("profile") {
        return Err(Rejected::new("profile is only valid for control-only"));
    }

    if let Some(wait) = object.get("wait") {
        if !wait.is_boolean() {
            return Err(Rejected::new("wait must be a boolean"));
        }
    }
    if let Some(vnc) = object.get("vnc") {
        if !vnc.is_boolean() {
            return Err(Rejected::new("vnc must be a boolean"));
        }
    }

    Ok(network
        .expect("network was validated as nat or control-only")
        .to_string())
}

/// Report that control-only acquisition is not available.
pub fn require_available() -> Result<(), Rejected> {
    Err(Rejected::new(
        "control-only unavailable: private helper and host-authoritative runtime/attachment audit pending; no NAT fallback",
    ))
}

/// SHA-256 of the bytes at `path`, lowercase hex.
///
/// Python raises `OSError` here; the contract exposes a `Rejected` result, so
/// an I/O failure is surfaced as the operating-system message.
pub fn digest(path: &Path) -> Result<String, Rejected> {
    let bytes = std::fs::read(path).map_err(|error| Rejected::new(error.to_string()))?;
    Ok(hex_sha256(&bytes))
}

/// Describe the control-only capability surface.
pub fn descriptor() -> Value {
    let patch = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("..")
        .join("patches")
        .join("softnet-control-only.patch");
    let patch_sha256 = if patch.is_file() {
        digest(&patch).map(Value::String).unwrap_or(Value::Null)
    } else {
        Value::Null
    };
    json!({
        "capabilities": [],
        "unavailable": [{
            "capability": CAPABILITY,
            "profile": PROFILE,
            "sourceCommit": SOURCE,
            "policySha256": POLICY_SHA256,
            "patchSha256": patch_sha256,
            "binarySha256": null,
            "available": false,
            "reason": "offline source only: helper build, host attachment auditor and isolated runtime provisioning not accepted",
        }],
    })
}

/// Pure plan for review, not permission to boot. Does not execute any command.
///
/// A reviewed installer supplies a root-owned nonsymlink directory and a pinned
/// setuid executable named `softnet`, compiled with `vm-service-control-only`.
/// Reject path aliases and writable ancestors, not just an executable hash.
pub fn boot_plan(vm: &str, private_bin: &Path, binary_sha256: &str) -> Result<Value, Rejected> {
    if !valid_clone_name(vm) {
        return Err(Rejected::new("invalid clone name"));
    }
    if !valid_sha256(binary_sha256) {
        return Err(Rejected::new("missing reviewed binary digest"));
    }
    if !private_bin.is_absolute()
        || private_bin
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(Rejected::new("private helper directory must be absolute"));
    }
    let helper = private_bin.join("softnet");
    for path in helper.ancestors() {
        let metadata =
            std::fs::symlink_metadata(path).map_err(|error| Rejected::new(error.to_string()))?;
        if metadata.file_type().is_symlink() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0
        {
            return Err(Rejected::new("unsafe private helper ownership/path"));
        }
    }
    let metadata = std::fs::metadata(&helper).map_err(|error| Rejected::new(error.to_string()))?;
    if !metadata.is_file() || metadata.mode() & 0o7777 != 0o4755 || metadata.nlink() != 1 {
        return Err(Rejected::new(
            "private helper must be reviewed root-owned setuid regular file",
        ));
    }
    if digest(&helper)? != binary_sha256 {
        return Err(Rejected::new("private helper binary digest mismatch"));
    }

    let directory = private_bin.to_string_lossy();
    let home = home_dir().to_string_lossy().into_owned();
    Ok(json!({
        "argv": [
            "/opt/homebrew/bin/tart",
            "run",
            vm,
            "--no-graphics",
            "--no-audio",
            "--no-clipboard",
            "--net-softnet-block=0.0.0.0/0",
        ],
        "env": {
            "PATH": format!("{directory}:/usr/bin:/bin"),
            "HOME": home,
            "LC_ALL": "C",
        },
        "policySha256": POLICY_SHA256,
        "binarySha256": binary_sha256,
    }))
}

/// Offline lifecycle core driven ONLY by host-owned observation callbacks.
///
/// `observe` is an internal host auditor, never HTTP input. Binding includes
/// clone storage identity, MAC/IP, attachment/socket identity, helper
/// executable digest, helper PID *and start identity*, boot process identity,
/// daemon generation, source/patch/policy digests, and kernel-verified
/// launcher/runtime identity. Shipping the callback implementation and
/// continuous watchdog is still pending.
pub struct LeaseGuard<O>
where
    O: Fn() -> Value,
{
    binding: Map<String, Value>,
    observe: O,
    state: GuardState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GuardState {
    Provisioning,
    Running,
    Releasing,
    Released,
}

impl GuardState {
    fn as_str(self) -> &'static str {
        match self {
            GuardState::Provisioning => "provisioning",
            GuardState::Running => "running",
            GuardState::Releasing => "releasing",
            GuardState::Released => "released",
        }
    }
}

impl<O> LeaseGuard<O>
where
    O: Fn() -> Value,
{
    /// Construct a guard in the `provisioning` state.
    pub fn new(binding: Map<String, Value>, observe: O) -> Result<Self, Rejected> {
        if binding.len() != REQUIRED.len() || !REQUIRED.iter().all(|key| binding.contains_key(*key))
        {
            return Err(Rejected::new("incomplete host binding"));
        }
        if binding
            .values()
            .any(|value| value.is_null() || matches!(value, Value::String(text) if text.is_empty()))
        {
            return Err(Rejected::new("incomplete host binding"));
        }
        if !value_is(binding.get("source_commit"), SOURCE)
            || !value_is(binding.get("policy_sha256"), POLICY_SHA256)
        {
            return Err(Rejected::new("wrong source/policy binding"));
        }
        Ok(Self {
            binding,
            observe,
            state: GuardState::Provisioning,
        })
    }

    /// Current lifecycle state, as the Python attribute string.
    pub fn state(&self) -> &'static str {
        self.state.as_str()
    }

    /// The host binding held by this guard.
    pub fn binding(&self) -> &Map<String, Value> {
        &self.binding
    }

    /// Verify the host still owns enforcement.
    pub fn check(&mut self) -> Result<(), Rejected> {
        if !matches!(self.state, GuardState::Provisioning | GuardState::Running) {
            return Err(Rejected::new("lease is releasing or released"));
        }
        let observed = (self.observe)();
        if observed.as_object() != Some(&self.binding) {
            self.state = GuardState::Releasing;
            return Err(Rejected::new("host enforcement ownership lost"));
        }
        Ok(())
    }

    /// Mark the lease ready after a successful check.
    pub fn ready(&mut self) -> Result<(), Rejected> {
        self.check()?;
        self.state = GuardState::Running;
        Ok(())
    }

    /// Run an operation, verifying ownership both before and after it.
    pub fn operate<T>(&mut self, operation: impl FnOnce() -> T) -> Result<T, Rejected> {
        if self.state != GuardState::Running {
            return Err(Rejected::new("lease is not ready"));
        }
        self.check()?;
        let result = operation();
        self.check()?;
        Ok(result)
    }

    /// Release the lease, deleting only a clone whose ownership is established.
    ///
    /// Loss of helper ownership must not prevent removal of OUR clone, nor
    /// authorize killing a process or clone that merely reused its name/PID.
    pub fn release(
        &mut self,
        owned_clone: impl Fn(&Map<String, Value>) -> bool,
        destroy: impl Fn(&Map<String, Value>) -> Result<(), Box<dyn std::error::Error + Send + Sync>>,
        absent: impl Fn(&Map<String, Value>) -> bool,
    ) -> Result<(), GuardError> {
        self.state = GuardState::Releasing;
        if !owned_clone(&self.binding) {
            return Err(GuardError::Rejected(Rejected::new(
                "clone ownership cannot be established; retain record",
            )));
        }
        destroy(&self.binding)?;
        if !absent(&self.binding) {
            return Err(GuardError::Rejected(Rejected::new(
                "absence not proven; retain releasing record",
            )));
        }
        self.state = GuardState::Released;
        Ok(())
    }
}

/// Compare a JSON value to an exact string.
fn value_is(value: Option<&Value>, expected: &str) -> bool {
    matches!(value, Some(Value::String(text)) if text == expected)
}

/// Lowercase hex SHA-256 of `bytes`.
fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// `[a-zA-Z0-9][a-zA-Z0-9_.-]{0,127}` full match.
fn valid_clone_name(vm: &str) -> bool {
    let bytes = vm.as_bytes();
    let Some((&first, rest)) = bytes.split_first() else {
        return false;
    };
    if !first.is_ascii_alphanumeric() || bytes.len() > 128 {
        return false;
    }
    rest.iter()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
}

/// `[a-f0-9]{64}` full match.
fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Python `Path.home()`: `HOME` when set, otherwise empty.
fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    fn binding() -> Map<String, Value> {
        let mut map = Map::new();
        for key in REQUIRED {
            map.insert(
                key.to_string(),
                Value::String(format!("host-measured-{key}")),
            );
        }
        map.insert("source_commit".into(), Value::String(SOURCE.into()));
        map.insert("policy_sha256".into(), Value::String(POLICY_SHA256.into()));
        map
    }

    fn observe_binding() -> (Rc<RefCell<Value>>, impl Fn() -> Value) {
        let observed = Rc::new(RefCell::new(Value::Object(binding())));
        let handle = observed.clone();
        let observe = move || handle.borrow().clone();
        (observed, observe)
    }

    #[test]
    fn policy_sha256_matches_policy() {
        // The digest is a cross-language constant, so serialize with keys in
        // ascending order regardless of the `serde_json` `preserve_order`
        // feature that another workspace crate enables.
        let value = policy();
        let sorted: std::collections::BTreeMap<String, Value> = value
            .as_object()
            .expect("policy is an object")
            .iter()
            .map(|(key, item)| (key.clone(), item.clone()))
            .collect();
        let serialized = serde_json::to_string(&sorted).unwrap();
        assert_eq!(
            serialized,
            "{\"alternateNICs\":false,\"bootpdWrites\":false,\"env\":\"none\",\"expose\":false,\"fragments\":false,\"hostAdmission\":\"tcp22-exact-return\",\"housekeeping\":\"validated-dhcp-arp\",\"ipv6\":false,\"network\":\"control-only\",\"profile\":\"softnet-0.23.0.ssh22-dhcp-arp.no-bootpd-write.v1\",\"shares\":false}"
        );
        assert_eq!(hex_sha256(serialized.as_bytes()), POLICY_SHA256);
    }

    // --- ProfileTests (pure subset) -------------------------------------

    #[test]
    fn discovery_does_not_advertise_source_as_enforcement() {
        let descriptor = descriptor();
        assert_eq!(descriptor["capabilities"], json!([]));
        let entry = &descriptor["unavailable"][0];
        assert_eq!(entry["available"], json!(false));
        assert!(entry["binarySha256"].is_null());
        assert_eq!(
            entry["policySha256"].as_str().map(str::len),
            Some(64),
            "policySha256 must be a 64-character digest"
        );
        assert_eq!(entry["capability"], json!(CAPABILITY));
        assert_eq!(entry["profile"], json!(PROFILE));
        assert_eq!(entry["sourceCommit"], json!(SOURCE));
        assert!(entry["patchSha256"].is_string());
        assert_eq!(
            entry["patchSha256"].as_str(),
            Some("c1ce073c3b036cb966cd1ded4efc14ba30e84f94a94674cf917c8f23b5524dfa")
        );
    }

    #[test]
    fn unknown_networks_and_acquisition_fields() {
        for field in [
            "shares",
            "expose",
            "nic",
            "verified",
            "receipt",
            "helper_pid",
            "runtime_uid",
            "typo",
        ] {
            let mut body = Map::new();
            body.insert("purpose".into(), json!("x"));
            body.insert(field.to_string(), Value::Null);
            assert!(
                validate_request(&Value::Object(body)).is_err(),
                "field should be rejected: {field}"
            );
        }
        for network in [
            json!("host"),
            json!("bridged"),
            json!(""),
            json!(null),
            json!({}),
            json!(["nat"]),
        ] {
            let body = json!({"network": network});
            assert!(
                validate_request(&body).is_err(),
                "network should be rejected: {network}"
            );
        }
        for body in [
            json!({"network": "control-only"}),
            json!({"network": "control-only", "pack": "none"}),
            json!({"network": "control-only", "env": "default"}),
            json!({"env": "none", "pack": "none"}),
            json!({"network": "control-only", "env": "none", "profile": "typo"}),
            json!({"profile": PROFILE}),
            json!({"wait": "false"}),
            json!([]),
            json!(null),
        ] {
            assert!(
                validate_request(&body).is_err(),
                "body should be rejected: {body}"
            );
        }
        assert_eq!(
            validate_request(&json!({"network": "control-only", "env": "none"})).unwrap(),
            "control-only"
        );
    }

    #[test]
    fn unknown_field_error_text_is_sorted() {
        let err =
            validate_request(&json!({"purpose": "x", "typo": null, "abc": null})).unwrap_err();
        assert_eq!(err.to_string(), "unknown acquisition fields: abc, typo");
    }

    #[test]
    fn ambiguous_alias_error_text() {
        let err = validate_request(&json!({"image": "a", "line": "b"})).unwrap_err();
        assert_eq!(err.to_string(), "ambiguous aliases: image, line");
        let err = validate_request(&json!({"env": "a", "pack": "b"})).unwrap_err();
        assert_eq!(err.to_string(), "ambiguous aliases: env, pack, lane");
    }

    #[test]
    fn request_validation_requires_explicit_credential_free_work() {
        let fingerprint = json!({"base": "abc"});
        assert!(validate_request(&json!({
            "purpose": "acceptance",
            "source": "work",
            "env": "none",
            "expected_source_fingerprint": fingerprint,
        }))
        .is_ok());
        for payload in [
            json!({"source": "work"}),
            json!({"source": "work", "env": "default"}),
            json!({"source": "base", "expected_source_fingerprint": {}}),
            json!({"source": "work", "env": "none", "expected_source_fingerprint": "bad"}),
        ] {
            assert!(
                validate_request(&payload).is_err(),
                "payload should be rejected: {payload}"
            );
        }
    }

    #[test]
    #[ignore = "test_reject_before_any_clone_or_boot: requires a live machine-service daemon, because \
                the Python test drives the real HTTP surface end to end"]
    fn reject_before_any_clone_or_boot() {}

    #[test]
    #[ignore = "test_default_still_uses_legacy_path: requires a live machine-service daemon, because \
                the Python test observes the running service's legacy selection path"]
    fn default_still_uses_legacy_path() {}

    #[test]
    #[ignore = "test_restored_control_record_cannot_use_legacy_ssh: requires a live machine-service \
                daemon, because the Python test restores a control record through the running \
                service"]
    fn restored_control_record_cannot_use_legacy_ssh() {}

    // --- GuardTests ------------------------------------------------------

    #[test]
    fn helper_death_pid_reuse_attachment_loss_and_daemon_restart() {
        for field in REQUIRED {
            let (observed, observe) = observe_binding();
            let mut guard = LeaseGuard::new(binding(), observe).unwrap();
            guard.ready().unwrap();
            {
                let mut current = observed.borrow_mut();
                let mut map = current.as_object().unwrap().clone();
                map.insert(field.to_string(), json!("changed"));
                *current = Value::Object(map);
            }
            let called = Rc::new(Cell::new(false));
            let action_called = called.clone();
            let result = guard.operate(move || {
                action_called.set(true);
                "success"
            });
            assert!(result.is_err(), "field={field}");
            assert!(!called.get(), "operation must not run; field={field}");
            assert_eq!(guard.state(), "releasing", "field={field}");
            *observed.borrow_mut() = Value::Object(binding());
        }
    }

    #[test]
    fn lost_enforcement_during_operation_never_returns_success() {
        let (observed, observe) = observe_binding();
        let mut guard = LeaseGuard::new(binding(), observe).unwrap();
        guard.ready().unwrap();
        let handle = observed.clone();
        let result = guard.operate(move || {
            *handle.borrow_mut() = Value::Null;
            "success"
        });
        assert!(result.is_err());
        assert_eq!(guard.state(), "releasing");
    }

    #[derive(Debug)]
    struct DeleteFailure;

    impl std::fmt::Display for DeleteFailure {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("delete failure")
        }
    }

    impl std::error::Error for DeleteFailure {}

    #[test]
    fn delete_failure_retains_releasing_and_retries_only_owned_clone() {
        let (_observed, observe) = observe_binding();
        let mut guard = LeaseGuard::new(binding(), observe).unwrap();

        let result = guard.release(|_| true, |_| Err(Box::new(DeleteFailure)), |_| true);
        assert!(matches!(result, Err(GuardError::Callback(_))));
        assert_eq!(guard.state(), "releasing");

        let destroyed = Rc::new(Cell::new(false));
        let destroy_called = destroyed.clone();
        let result = guard.release(
            |_| false,
            move |_| {
                destroy_called.set(true);
                Ok(())
            },
            |_| true,
        );
        assert!(matches!(result, Err(GuardError::Rejected(_))));
        assert!(!destroyed.get());

        let result = guard.release(|_| true, |_| Ok(()), |_| false);
        assert!(matches!(result, Err(GuardError::Rejected(_))));
        assert_eq!(guard.state(), "releasing");

        guard.release(|_| true, |_| Ok(()), |_| true).unwrap();
        assert_eq!(guard.state(), "released");
    }

    #[test]
    fn cannot_supply_guest_verified_json() {
        let mut binding = Map::new();
        binding.insert("verified".into(), json!(true));
        binding.insert("uid".into(), json!(501));
        let result = LeaseGuard::new(binding, || json!(null));
        assert!(result.is_err());
    }
}
