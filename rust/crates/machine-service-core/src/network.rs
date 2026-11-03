//! The guest network a lease asks for, and whether this host can give it
//! (DC-13, `docs/acquisition-contract.md`, "Guest network").
//!
//! `nat` and `internet` are guest networks (`acquisition_options::GuestNetwork`).
//! `control-only` is a separate contract that the `control-only` crate owns
//! and refuses; this module hands such a request to that crate unchanged.

use std::ffi::OsStr;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

use acquisition_options::GuestNetwork;
use serde_json::Value;

use crate::error::{OpError, OpResult};

/// The network an acquisition request selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestedNetwork {
    /// A guest network: `nat` (also when `network` is absent) or `internet`.
    Guest(GuestNetwork),
    /// The control-only contract, which acquisition refuses while none is
    /// available (`control_only::require_available`).
    ControlOnly,
}

/// Validate an acquisition request body and return the network it selects.
///
/// A `control-only` request is validated by `control_only::validate_request`
/// exactly as before DC-13. Any other request is validated by the same
/// function as an ordinary request (its field, alias, source, `profile`,
/// `wait`, and `vnc` rules), and its `network` by
/// `acquisition_options::GuestNetwork::parse`, which accepts only `nat` and
/// `internet`.
pub fn validate_request(body: &Value) -> OpResult<RequestedNetwork> {
    let requested = body.get("network");
    if requested.and_then(Value::as_str) == Some(CONTROL_ONLY) {
        control_only::validate_request(body).map_err(|error| OpError::new(error.to_string()))?;
        return Ok(RequestedNetwork::ControlOnly);
    }
    let mut ordinary = body.clone();
    if let Some(object) = ordinary.as_object_mut() {
        // The control-only validator reads an absent network as NAT, the
        // ordinary branch every guest network shares.
        object.remove("network");
    }
    control_only::validate_request(&ordinary).map_err(|error| OpError::new(error.to_string()))?;
    let network =
        GuestNetwork::parse(requested).map_err(|error| OpError::new(error.to_string()))?;
    Ok(RequestedNetwork::Guest(network))
}

/// The one egress an acquisition may ask for: the host's egress allowlist
/// (`docs/arch/lab-runners.md`, LR-5, DC-1xx-EPH-9).
pub const EGRESS_ALLOWLIST: &str = "allowlist";

/// Validate an acquisition's `egress`: absent, or `"allowlist"` on the
/// `internet` network. Returns whether the guest is an egress guest.
pub fn validate_egress(body: &Value) -> OpResult<bool> {
    let Some(egress) = body.get("egress") else {
        return Ok(false);
    };
    if egress.as_str() != Some(EGRESS_ALLOWLIST) {
        return Err(OpError::new("egress must be allowlist"));
    }
    if body.get("network").and_then(Value::as_str) != Some("internet") {
        return Err(OpError::new("egress allowlist needs network internet"));
    }
    Ok(true)
}

/// The control-only network's request value.
const CONTROL_ONLY: &str = "control-only";

/// The arguments of `tart run` for a guest on `network`.
pub fn run_args(vm: &str, network: GuestNetwork) -> Vec<String> {
    let mut args = vec![
        "run".to_string(),
        vm.to_string(),
        "--no-graphics".to_string(),
    ];
    if network == GuestNetwork::Internet {
        args.push("--net-softnet".to_string());
    }
    args
}

/// The arguments of `tart run` for a guest on `network` that Softnet must
/// also let reach `allow`, each an address it would otherwise drop as
/// private: an egress guest's lab hosts (DC-1xx-EPH-9). Each address is
/// passed as its own `/32`, a network as written.
pub fn run_args_allowing(vm: &str, network: GuestNetwork, allow: &[String]) -> Vec<String> {
    let mut args = run_args(vm, network);
    if network == GuestNetwork::Internet && !allow.is_empty() {
        let list = allow
            .iter()
            .map(|entry| {
                if entry.contains('/') {
                    entry.clone()
                } else {
                    format!("{entry}/32")
                }
            })
            .collect::<Vec<_>>()
            .join(",");
        args.push(format!("--net-softnet-allow={list}"));
    }
    args
}

/// Find the Softnet executable Tart would run, and check that Tart can run it
/// unattended.
///
/// Tart looks `softnet` up on its own `PATH` (the first directory where the
/// name exists), resolves symlinks, and runs it directly unless it is
/// interactive. Softnet then needs to be owned by root with the setuid bit,
/// which is what Tart's own check accepts. `path` is the `PATH` Tart runs with.
///
/// Returns the resolved executable, or a reason that names Softnet.
pub fn softnet_executable(path: Option<&OsStr>) -> Result<PathBuf, String> {
    let found = path
        .into_iter()
        .flat_map(std::env::split_paths)
        .filter(|directory| !directory.as_os_str().is_empty())
        .map(|directory| directory.join("softnet"))
        .find(|candidate| candidate.exists())
        .ok_or_else(|| {
            "Softnet unavailable: no softnet executable on Tart's PATH; network internet needs \
Tart's Softnet installed setuid root (no NAT fallback)"
                .to_string()
        })?;
    let resolved = std::fs::canonicalize(&found).map_err(|error| {
        format!(
            "Softnet unavailable: {} cannot be resolved: {error} (no NAT fallback)",
            found.display()
        )
    })?;
    let metadata = std::fs::metadata(&resolved).map_err(|error| {
        format!(
            "Softnet unavailable: {} cannot be read: {error} (no NAT fallback)",
            resolved.display()
        )
    })?;
    if !metadata.is_file() || !setuid_root(&metadata) {
        return Err(format!(
            "Softnet unavailable: {} is not a setuid-root file, which Tart needs to run Softnet \
unattended (no NAT fallback)",
            resolved.display()
        ));
    }
    Ok(resolved)
}

fn setuid_root(metadata: &std::fs::Metadata) -> bool {
    metadata.uid() == 0 && metadata.mode() & 0o4000 != 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;

    #[test]
    fn absent_and_nat_select_nat() {
        assert_eq!(
            validate_request(&json!({"purpose": "p"})).unwrap(),
            RequestedNetwork::Guest(GuestNetwork::Nat)
        );
        assert_eq!(
            validate_request(&json!({"purpose": "p", "network": "nat"})).unwrap(),
            RequestedNetwork::Guest(GuestNetwork::Nat)
        );
    }

    #[test]
    fn internet_selects_internet() {
        assert_eq!(
            validate_request(&json!({"purpose": "p", "network": "internet"})).unwrap(),
            RequestedNetwork::Guest(GuestNetwork::Internet)
        );
    }

    #[test]
    fn every_other_network_is_rejected() {
        for network in [
            json!("host"),
            json!("bridged"),
            json!("softnet"),
            json!(""),
            json!(null),
            json!({}),
            json!(["internet"]),
        ] {
            let error = validate_request(&json!({"network": network})).unwrap_err();
            assert_eq!(
                error.message, "network must be nat or internet",
                "network={network}"
            );
        }
    }

    #[test]
    fn control_only_keeps_its_own_contract() {
        assert_eq!(
            validate_request(&json!({"network": "control-only", "env": "none"})).unwrap(),
            RequestedNetwork::ControlOnly
        );
        let error = validate_request(&json!({"network": "control-only"})).unwrap_err();
        assert_eq!(
            error.message,
            "control-only requires explicit env:none (not an alias)"
        );
        let error =
            validate_request(&json!({"network": "control-only", "env": "none", "profile": "typo"}))
                .unwrap_err();
        assert_eq!(error.message, "unsupported control-only profile");
    }

    #[test]
    fn internet_requests_keep_the_ordinary_rules() {
        let error =
            validate_request(&json!({"network": "internet", "profile": control_only::PROFILE}))
                .unwrap_err();
        assert_eq!(error.message, "profile is only valid for control-only");
        let error = validate_request(&json!({"network": "internet", "typo": 1})).unwrap_err();
        assert_eq!(error.message, "unknown acquisition fields: typo");
        let error = validate_request(&json!({"network": "internet", "wait": "no"})).unwrap_err();
        assert_eq!(error.message, "wait must be a boolean");
        let error = validate_request(&json!([])).unwrap_err();
        assert_eq!(error.message, "acquisition must be an object");
    }

    #[test]
    fn egress_is_the_allowlist_on_the_internet_network_only() {
        assert!(!validate_egress(&json!({"network": "internet"})).unwrap());
        assert!(validate_egress(&json!({"network": "internet", "egress": "allowlist"})).unwrap());
        for body in [
            json!({"network": "internet", "egress": "open"}),
            json!({"network": "internet", "egress": true}),
        ] {
            assert_eq!(
                validate_egress(&body).unwrap_err().message,
                "egress must be allowlist"
            );
        }
        for body in [
            json!({"egress": "allowlist"}),
            json!({"network": "nat", "egress": "allowlist"}),
        ] {
            assert_eq!(
                validate_egress(&body).unwrap_err().message,
                "egress allowlist needs network internet"
            );
        }
        // The ordinary validator accepts the field.
        assert_eq!(
            validate_request(&json!({"network": "internet", "egress": "allowlist"})).unwrap(),
            RequestedNetwork::Guest(GuestNetwork::Internet)
        );
    }

    #[test]
    fn softnet_lets_an_egress_guest_reach_its_lab_hosts() {
        assert_eq!(
            run_args_allowing(
                "si-lab-a",
                GuestNetwork::Internet,
                &[
                    "192.0.2.3".into(),
                    "192.0.2.4".into(),
                    "198.51.100.0/24".into()
                ]
            ),
            vec![
                "run",
                "si-lab-a",
                "--no-graphics",
                "--net-softnet",
                "--net-softnet-allow=192.0.2.3/32,192.0.2.4/32,198.51.100.0/24"
            ]
        );
        assert_eq!(
            run_args_allowing("si-lab-a", GuestNetwork::Internet, &[]),
            run_args("si-lab-a", GuestNetwork::Internet)
        );
        assert_eq!(
            run_args_allowing("pilot-a", GuestNetwork::Nat, &["192.0.2.3".into()]),
            run_args("pilot-a", GuestNetwork::Nat)
        );
    }

    #[test]
    fn nat_runs_today_s_command() {
        assert_eq!(
            run_args("pilot-t-abc123", GuestNetwork::Nat),
            vec!["run", "pilot-t-abc123", "--no-graphics"]
        );
    }

    #[test]
    fn internet_runs_on_softnet() {
        assert_eq!(
            run_args("pilot-t-abc123", GuestNetwork::Internet),
            vec!["run", "pilot-t-abc123", "--no-graphics", "--net-softnet"]
        );
    }

    #[test]
    fn missing_softnet_is_named() {
        let empty = tempfile::tempdir().unwrap();
        let reason = softnet_executable(Some(empty.path().as_os_str())).unwrap_err();
        assert!(
            reason.starts_with("Softnet unavailable: no softnet executable"),
            "{reason}"
        );
        assert!(reason.contains("no NAT fallback"), "{reason}");
        let reason = softnet_executable(None).unwrap_err();
        assert!(reason.starts_with("Softnet unavailable"), "{reason}");
    }

    #[test]
    fn softnet_that_is_not_setuid_root_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let softnet = directory.path().join("softnet");
        std::fs::write(&softnet, "#!/bin/sh\n").unwrap();
        let reason = softnet_executable(Some(directory.path().as_os_str())).unwrap_err();
        assert!(reason.starts_with("Softnet unavailable: "), "{reason}");
        assert!(reason.contains("is not a setuid-root file"), "{reason}");
    }

    #[test]
    fn a_symlink_to_a_setuid_root_executable_is_usable() {
        // Homebrew links `bin/softnet` into its Cellar, and Tart checks the
        // link's target. `/usr/bin/sudo` stands in for a setuid-root target.
        let directory = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/usr/bin/sudo", directory.path().join("softnet")).unwrap();
        let path = std::env::join_paths([Path::new("/nonexistent"), directory.path()]).unwrap();
        assert_eq!(
            softnet_executable(Some(path.as_os_str())).unwrap(),
            PathBuf::from("/usr/bin/sudo")
        );
    }

    #[test]
    fn the_first_softnet_on_the_path_is_the_one_checked() {
        let first = tempfile::tempdir().unwrap();
        std::fs::write(first.path().join("softnet"), "").unwrap();
        let second = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/usr/bin/sudo", second.path().join("softnet")).unwrap();
        let path = std::env::join_paths([first.path(), second.path()]).unwrap();
        assert!(softnet_executable(Some(path.as_os_str())).is_err());
    }
}
