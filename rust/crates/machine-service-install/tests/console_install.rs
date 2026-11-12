//! Port of `tests/unit/test_console_install.py`.
//!
//! Every test drives the built `machine-service-install` binary against an isolated
//! `HOME`, a fake Tart executable, and a `launchctl` stub. No test needs a real
//! LaunchAgent or root.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;

use common::{write_exec, Harness};
use serde_json::json;

fn hold_lock(path: &Path) -> fs::File {
    let file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .unwrap();
    // SAFETY: `flock` operates on the open descriptor used by this test only.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    assert_eq!(result, 0, "the test could not hold {}", path.display());
    file
}

#[test]
fn default_install_and_remove() {
    let harness = Harness::new();
    let output = harness.run(&[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let data = harness.saved();
    assert_eq!(
        data["EnvironmentVariables"]["MACHINE_SERVICE_HOST"],
        json!("127.0.0.1")
    );
    // A native service needs no interpreter slot; the shell script's
    // `ProgramArguments[1]` is the single Rust executable here.
    assert_eq!(
        data["ProgramArguments"][0],
        json!(harness.bin.join("machine-service").to_str().unwrap())
    );
    assert_eq!(data["ProgramArguments"].as_array().unwrap().len(), 1);
    assert!(data["EnvironmentVariables"]
        .as_object()
        .unwrap()
        .get("MACHINE_SERVICE_CONSOLE_CONFIG")
        .is_none());
    assert_eq!(fs::read_to_string(&harness.tartlog).unwrap(), "--version\n");

    let output = harness.run(&["remove"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!harness.plist.exists());
    assert!(fs::read_to_string(&harness.log).unwrap().contains("load "));
}

/// DC-242, keeping DC-241's guarantee: a test build of the
/// installer runs only the `launchctl` its test names through the seam. Named
/// none, with the stub first on its `PATH`, it refuses before it runs any:
/// neither the one `PATH` finds nor `/bin/launchctl`.
#[test]
fn a_test_installer_never_runs_a_launchctl_it_is_not_given() {
    assert!(machine_service_core::launchd::is_test_build());
    let harness = Harness::new();
    fs::copy(&harness.launchctl, harness.bin.join("launchctl")).unwrap();
    let output = harness.run_without_launchctl(&[]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(machine_service_core::launchd::TEST_VARIABLE),
        "{stderr}"
    );
    assert!(!harness.log.exists(), "a launchctl on PATH ran");
}

#[test]
fn check_and_dry_run_do_not_write_or_launch() {
    let harness = Harness::new();
    for flag in ["--check", "--dry-run"] {
        let config = harness.config_path();
        let output = harness.run(&[flag, "--console-config", &config]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!harness.plist.exists());
        assert!(!harness.log.exists());
        assert!(!harness.home.join(".local/state").exists());
    }
    assert_eq!(
        fs::read_to_string(&harness.tartlog).unwrap(),
        "--version\n--version\n"
    );
}

#[test]
fn xml_safe_and_upgrade_preserves_environment() {
    let harness = Harness::new();
    let selected = harness.home.join("selected").to_string_lossy().into_owned();
    let previous = [
        ("HOME", harness.home_str()),
        ("MACHINE_SERVICE_HOST", "0.0.0.0"),
        ("MACHINE_SERVICE_PORT", "6543"),
        ("MACHINE_SERVICE_STATE", selected.as_str()),
        ("MACHINE_SERVICE_GRACE_HOURS", "7"),
        ("CUSTOM", "A&B <C>"),
    ];
    harness.save_old(&previous);

    let config = harness.config_path();
    let output = harness.run(&["--console-config", &config]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let data = harness.saved();
    assert_eq!(
        data["EnvironmentVariables"]["MACHINE_SERVICE_CONSOLE_CONFIG"],
        json!(config)
    );
    for (key, value) in previous {
        assert_eq!(data["EnvironmentVariables"][key], json!(value), "key {key}");
    }

    let output = harness.run(&[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        harness.saved()["EnvironmentVariables"]["MACHINE_SERVICE_CONSOLE_CONFIG"],
        json!(config)
    );
}

#[test]
fn initial_enable_requires_explicit_path() {
    let harness = Harness::new();
    let default = harness.home.join(".config/machine-service/console.json");
    fs::create_dir_all(default.parent().unwrap()).unwrap();
    fs::write(&default, fs::read(&harness.config).unwrap()).unwrap();
    fs::set_permissions(&default, fs::Permissions::from_mode(0o600)).unwrap();

    let stderr = harness.assert_refused(&["--check"]);
    assert!(stderr.contains("requires --console-config"), "{stderr}");
    assert!(!harness.log.exists());

    let default_path = default.to_string_lossy().into_owned();
    let output = harness.run(&["--check", "--console-config", &default_path]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn invalid_config_and_tart_leave_old_install_untouched() {
    let harness = Harness::new();
    harness.save_old(&[
        ("HOME", harness.home_str()),
        ("MACHINE_SERVICE_HOST", "0.0.0.0"),
    ]);
    let original = fs::read(&harness.plist).unwrap();

    fs::set_permissions(&harness.config, fs::Permissions::from_mode(0o666)).unwrap();
    let config = harness.config_path();
    let _ = harness.assert_refused(&["--console-config", &config]);
    assert_eq!(fs::read(&harness.plist).unwrap(), original);
    assert!(!harness.log.exists());

    fs::set_permissions(&harness.config, fs::Permissions::from_mode(0o600)).unwrap();
    write_exec(&harness.tart, "#!/bin/sh\necho patched-private-build\n");
    let _ = harness.assert_refused(&["--console-config", &config]);
    assert_eq!(fs::read(&harness.plist).unwrap(), original);
    assert!(!harness.log.exists());
}

#[test]
fn active_and_retained_leases_refuse_restart_and_removal() {
    let harness = Harness::new();
    harness.save_old(&[("HOME", harness.home_str())]);
    let root = harness.home.join(".local/state/machine-service");
    fs::create_dir_all(&root).unwrap();
    for state in ["pending", "provisioning", "running", "deleting", "failed"] {
        fs::write(
            root.join("state.json"),
            format!("{{\"vms\":{{\"fixture\":{{\"state\":\"{state}\"}}}}}}"),
        )
        .unwrap();
        for action in ["install", "remove"] {
            let _ = harness.assert_refused(&[action]);
            assert!(harness.plist.exists(), "state {state}, action {action}");
            assert!(!harness.log.exists(), "state {state}, action {action}");
        }
    }
}

#[test]
fn selected_old_state_is_checked_even_when_overridden() {
    let harness = Harness::new();
    let root = harness.home.join("old-state");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("state.json"), "{\"vms\":{\"old-lease\":{}}}").unwrap();
    let root_text = root.to_string_lossy().into_owned();
    harness.save_old(&[
        ("HOME", harness.home_str()),
        ("MACHINE_SERVICE_STATE", &root_text),
    ]);

    let new_state = harness
        .home
        .join("new-state")
        .to_string_lossy()
        .into_owned();
    let mut harness = harness;
    harness.set_env("MACHINE_SERVICE_STATE", new_state);
    let _ = harness.assert_refused(&[]);
    assert!(!harness.log.exists());
}

#[test]
fn selected_environment_survives_upgrade_check() {
    let harness = Harness::new();
    let repo = harness.home.join("image-repo");
    fs::create_dir_all(repo.join("images")).unwrap();
    let profile = json!({
        "schemaVersion": 2,
        "id": "fixture",
        "vmServiceUrl": "http://127.0.0.1:6549",
        "imageRepository": repo.to_str().unwrap(),
        "tartHome": harness.home.join("selected-tart").to_str().unwrap(),
        "serviceStateDir": harness.home.join("selected-service").to_str().unwrap(),
        "imageStateDir": harness.home.join("selected-images").to_str().unwrap(),
        "relayStateDir": harness.home.join("selected-relay").to_str().unwrap(),
        "machctlPath": harness.bin.join("machctl").to_str().unwrap(),
        "tartPath": harness.tart.to_str().unwrap(),
    });
    let selector = harness.home.join("environment.json");
    fs::write(&selector, profile.to_string()).unwrap();
    let selector_text = selector.to_string_lossy().into_owned();

    harness.save_old(&[
        ("HOME", harness.home_str()),
        ("VM_ENVIRONMENT_FILE", &selector_text),
        ("MACHINE_SERVICE_HOST", "127.0.0.1"),
        ("MACHINE_SERVICE_PORT", "6549"),
    ]);
    let original = fs::read(&harness.plist).unwrap();

    let config = harness.config_path();
    let output = harness.run(&["--check", "--console-config", &config]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(&harness.plist).unwrap(), original);
    assert!(!harness.log.exists());
    for key in [
        "tartHome",
        "serviceStateDir",
        "imageStateDir",
        "relayStateDir",
    ] {
        let path = Path::new(profile[key].as_str().unwrap());
        assert!(!path.exists(), "{key} was created by --check");
    }

    let output = harness.run(&["--console-config", &config]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        harness.saved()["EnvironmentVariables"]["VM_ENVIRONMENT_FILE"],
        json!(selector_text)
    );

    // A selected Tart-store owner is also a daemon owner, even when no job
    // remains and the service state's own daemon.lock is absent.
    fs::remove_file(harness.home.join("loaded-job")).unwrap();
    let store = harness.home.join("selected-tart");
    fs::create_dir_all(&store).unwrap();
    let _lock = hold_lock(&store.join(".vm-service-daemon.lock"));
    let stderr = harness.assert_refused(&["remove"]);
    assert!(stderr.contains("daemon ownership remains"), "{stderr}");
    assert!(harness.plist.exists());
}

#[test]
fn held_state_lock_refuses_restart() {
    let harness = Harness::new();
    let root = harness.home.join(".local/state/machine-service");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("state.json"), "{\"vms\":{}}").unwrap();
    let _lock = hold_lock(&root.join("state.lock"));

    let _ = harness.assert_refused(&[]);
    assert!(!harness.plist.exists());
    assert!(!harness.log.exists());
}

#[test]
fn remove_absent_install_does_not_create_state() {
    let harness = Harness::new();
    let output = harness.run(&["remove"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!harness.home.join(".local").exists());
    assert!(!harness.plist.exists());
}

#[test]
fn unload_failure_preserves_plist_and_does_not_load() {
    let harness = Harness::new();
    harness.save_old(&[("HOME", harness.home_str())]);
    let original = fs::read(&harness.plist).unwrap();
    fs::write(harness.home.join("unload-fails"), "").unwrap();

    for action in ["install", "remove"] {
        let _ = fs::remove_file(&harness.log);
        let stderr = harness.assert_refused(&[action]);
        assert!(stderr.contains("unload failed"), "{stderr}");
        assert_eq!(fs::read(&harness.plist).unwrap(), original);
        let calls = fs::read_to_string(&harness.log).unwrap();
        assert!(calls.lines().any(|line| line.starts_with("unload ")));
        assert!(!calls.lines().any(|line| line.starts_with("load ")));
    }
}

#[test]
fn user_domain_job_is_not_mistaken_for_absent() {
    let harness = Harness::new();
    harness.save_old(&[("HOME", harness.home_str())]);
    let original = fs::read(&harness.plist).unwrap();
    fs::write(harness.home.join("user-domain"), "").unwrap();
    fs::write(harness.home.join("unload-fails"), "").unwrap();

    let stderr = harness.assert_refused(&[]);
    assert!(stderr.contains("unload failed"), "{stderr}");
    assert_eq!(fs::read(&harness.plist).unwrap(), original);
}

#[test]
fn job_state_query_failure_preserves_plist() {
    let harness = Harness::new();
    harness.save_old(&[("HOME", harness.home_str())]);
    let original = fs::read(&harness.plist).unwrap();
    fs::write(harness.home.join("print-fails"), "").unwrap();

    let _ = harness.assert_refused(&[]);
    assert_eq!(fs::read(&harness.plist).unwrap(), original);
    let calls = fs::read_to_string(&harness.log).unwrap();
    assert!(calls.lines().all(|line| line.starts_with("print ")));
}

#[test]
fn successful_unload_with_lingering_job_preserves_plist() {
    let harness = Harness::new();
    harness.save_old(&[("HOME", harness.home_str())]);
    let original = fs::read(&harness.plist).unwrap();
    fs::write(harness.home.join("linger-job"), "").unwrap();

    let _ = harness.assert_refused(&[]);
    assert_eq!(fs::read(&harness.plist).unwrap(), original);
    let calls = fs::read_to_string(&harness.log).unwrap();
    assert!(!calls.lines().any(|line| line.starts_with("load ")));
}

#[test]
fn deregistered_job_with_live_pid_preserves_plist() {
    let harness = Harness::new();
    harness.save_old(&[("HOME", harness.home_str())]);
    let original = fs::read(&harness.plist).unwrap();
    // The fixture process is alive. The installer must only probe it, never
    // send a terminating signal or assume deregistration killed it.
    fs::write(
        harness.home.join("loaded-job"),
        std::process::id().to_string(),
    )
    .unwrap();

    let _ = harness.assert_refused(&["remove"]);
    assert_eq!(fs::read(&harness.plist).unwrap(), original);
    assert!(!harness.home.join("loaded-job").exists());
}

#[test]
fn daemon_owner_without_job_refuses_removal() {
    let harness = Harness::new();
    harness.save_old(&[("HOME", harness.home_str())]);
    let original = fs::read(&harness.plist).unwrap();
    fs::remove_file(harness.home.join("loaded-job")).unwrap();
    let root = harness.home.join(".local/state/machine-service");
    fs::create_dir_all(&root).unwrap();
    let _lock = hold_lock(&root.join("daemon.lock"));

    let stderr = harness.assert_refused(&["remove"]);
    assert!(stderr.contains("daemon ownership remains"), "{stderr}");
    assert_eq!(fs::read(&harness.plist).unwrap(), original);
    let calls = fs::read_to_string(&harness.log).unwrap();
    assert!(calls.lines().all(|line| line.starts_with("print ")));
}

#[test]
fn owner_remains_after_successful_unload() {
    let harness = Harness::new();
    harness.save_old(&[("HOME", harness.home_str())]);
    let original = fs::read(&harness.plist).unwrap();
    let root = harness.home.join(".local/state/machine-service");
    fs::create_dir_all(&root).unwrap();
    let _lock = hold_lock(&root.join("daemon.lock"));

    let _ = harness.assert_refused(&[]);
    assert_eq!(fs::read(&harness.plist).unwrap(), original);
    let calls = fs::read_to_string(&harness.log).unwrap();
    assert!(!calls.lines().any(|line| line.starts_with("load ")));
}

#[test]
fn missing_or_invalid_required_helpers_fail_before_unload() {
    let harness = Harness::new();
    harness.save_old(&[("HOME", harness.home_str())]);
    let original = fs::read(&harness.plist).unwrap();

    for name in [
        "console-worker",
        "guest-console-agent",
        "guest-console-agent-linux",
        "machctl",
    ] {
        let helper = harness.bin.join(name);
        let content = fs::read(&helper).unwrap();
        for invalid in [false, true] {
            fs::write(&helper, &content).unwrap();
            if invalid {
                fs::set_permissions(&helper, fs::Permissions::from_mode(0o644)).unwrap();
            } else {
                fs::remove_file(&helper).unwrap();
            }
            let _ = harness.assert_refused(&[]);
            assert_eq!(fs::read(&harness.plist).unwrap(), original, "helper {name}");
            assert!(!harness.log.exists(), "helper {name}");
        }
        fs::write(&helper, &content).unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

#[test]
fn missing_service_executable_fails_before_unload() {
    let harness = Harness::new();
    harness.save_old(&[("HOME", harness.home_str())]);
    let original = fs::read(&harness.plist).unwrap();
    fs::remove_file(harness.bin.join("machine-service")).unwrap();

    let stderr = harness.assert_refused(&[]);
    assert!(stderr.contains("missing executable service"), "{stderr}");
    assert_eq!(fs::read(&harness.plist).unwrap(), original);
    assert!(!harness.log.exists());
}

#[test]
fn corrupt_state_is_not_treated_as_empty() {
    let harness = Harness::new();
    let root = harness.home.join(".local/state/machine-service");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("state.json"), "broken").unwrap();

    let _ = harness.assert_refused(&[]);
    assert!(!harness.log.exists());
}

#[cfg(target_os = "macos")]
#[test]
fn written_plist_is_accepted_by_plutil() {
    let harness = Harness::new();
    let output = harness.run(&[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let lint = std::process::Command::new("/usr/bin/plutil")
        .arg("-lint")
        .arg(&harness.plist)
        .output()
        .unwrap();
    assert!(
        lint.status.success(),
        "plutil rejected the plist: {}{}",
        String::from_utf8_lossy(&lint.stdout),
        String::from_utf8_lossy(&lint.stderr)
    );
}
