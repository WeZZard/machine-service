//! The installation result (`docs/arch/installation.md` IN-1, IN-2; AC-46,
//! DC-79).
//!
//! The expected set is `service_layout::required`, the list the daemon
//! resolves its helpers from, never a list kept here: an executable the
//! daemon looks up and the installer leaves out fails these tests. Every
//! test runs the built `machine-service-install` against an isolated `HOME`
//! and the `launchctl` stub.

mod common;

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use common::{release_executable, write_exec, write_release, write_release_of, Harness};
use serde_json::json;
use service_layout::Executable;

fn files_in(directory: &Path) -> BTreeSet<String> {
    fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

fn prefix(harness: &Harness) -> PathBuf {
    harness.home.join(".local/libexec/machine-service")
}

fn install_into_prefix(harness: &Harness) -> std::process::Output {
    let prefix = prefix(harness);
    harness.run(&["install", "--prefix", prefix.to_str().unwrap()])
}

/// AC-46: an installation into an empty folder yields exactly the
/// executables the daemon resolves, plus the CLI and the installer: each one
/// byte-equal to its build and executable, and nothing else beside them.
#[test]
fn ac46_installs_exactly_the_layout_into_a_prefix() {
    let harness = Harness::new();
    // Something beside the installer that is not part of the installation.
    write_exec(&harness.bin.join("stray-tool"), "#!/bin/sh\nexit 0\n");
    let output = install_into_prefix(&harness);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let prefix = prefix(&harness);
    let expected: BTreeSet<String> = service_layout::required(true).into_iter().collect();
    let mut with_release = expected.clone();
    with_release.insert(service_layout::release::FOLDER.to_string());
    assert_eq!(files_in(&prefix), with_release);
    // DC-140: the relay agent release is the build's, verbatim.
    let built = harness.bin.join(service_layout::release::FOLDER);
    let installed = prefix.join(service_layout::release::FOLDER);
    assert_eq!(files_in(&installed), files_in(&built));
    for name in files_in(&built) {
        assert_eq!(
            fs::read(installed.join(&name)).unwrap(),
            fs::read(built.join(&name)).unwrap(),
            "{name}"
        );
    }
    for name in &expected {
        let installed = prefix.join(name);
        assert_eq!(
            fs::read(&installed).unwrap(),
            fs::read(harness.bin.join(name)).unwrap(),
            "{name} is a copy of its build"
        );
        let mode = fs::metadata(&installed).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "{name}");
    }

    // The LaunchAgent runs the installed daemon, and the CLI link names the
    // installed machctl.
    assert_eq!(
        harness.saved()["ProgramArguments"],
        json!([prefix.join("machine-service").to_str().unwrap()])
    );
    assert_eq!(
        fs::read_link(harness.home.join(".local/bin/machctl")).unwrap(),
        prefix.join("machctl")
    );
    let calls = fs::read_to_string(&harness.log).unwrap();
    assert!(
        calls.lines().any(|line| line.starts_with("load ")),
        "{calls}"
    );
}

/// AC-46: the session agent the guest link uploads is installed, for macOS
/// and Linux guests alike: the regression that left `machctl exec` and
/// `machctl push` failing with "guest session agent unavailable".
#[test]
fn ac46_installs_the_guest_session_agent_for_both_guest_kinds() {
    let harness = Harness::new();
    let output = install_into_prefix(&harness);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let directory = prefix(&harness);
    let agent = Executable::GuestSessionAgent;
    for kind in ["macos", "linux"] {
        let resolved = service_layout::guest_build_in(&directory, agent, kind);
        assert!(resolved.is_file(), "{kind}: {}", resolved.display());
    }
    assert_eq!(
        service_layout::guest_build_in(&directory, agent, "linux"),
        directory.join("guest-session-agent-linux")
    );
}

/// AC-46: any one missing executable refuses the installation, naming it,
/// before a file is copied, the LaunchAgent written or launchctl called.
#[test]
fn ac46_refuses_a_build_missing_any_executable() {
    for name in service_layout::required(true) {
        // The installer is the program running, so it is always present.
        if name == Executable::Installer.name() {
            continue;
        }
        let harness = Harness::new();
        fs::remove_file(harness.bin.join(&name)).unwrap();
        let stderr =
            harness.assert_refused(&["install", "--prefix", prefix(&harness).to_str().unwrap()]);
        let missing = harness.bin.join(&name);
        assert!(
            stderr.contains(&format!("{}", missing.display())),
            "{name}: {stderr}"
        );
        assert!(!prefix(&harness).exists(), "{name}: nothing is installed");
        assert!(!harness.plist.exists(), "{name}: no LaunchAgent is written");
        let calls = fs::read_to_string(&harness.log).unwrap_or_default();
        assert!(
            !calls.lines().any(|line| line.starts_with("load ")),
            "{name}: {calls}"
        );
    }
}

/// AC-46: on a Mac without Tart the installation is the core alone, even
/// when the build folder also holds the virtual-machine helpers.
#[test]
fn ac46_a_mac_without_tart_gets_the_core_only() {
    let mut harness = Harness::new();
    harness.remove_tart();
    let prefix = prefix(&harness);
    let output = harness.run(&[
        "install",
        "--expose-durable",
        "on",
        "--prefix",
        prefix.to_str().unwrap(),
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut expected: BTreeSet<String> = service_layout::required(false).into_iter().collect();
    // Exposed, it needs the macOS relay agent of the release (IN-3).
    expected.insert(service_layout::release::FOLDER.to_string());
    assert_eq!(files_in(&prefix), expected);
}

/// AC-46: `--check` with a prefix validates the build and writes nothing.
#[test]
fn ac46_check_with_a_prefix_writes_nothing() {
    let harness = Harness::new();
    let output = harness.run(&[
        "install",
        "--check",
        "--prefix",
        prefix(&harness).to_str().unwrap(),
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!prefix(&harness).exists());
    assert!(!harness.plist.exists());
}

/// `--prefix` names where an installation goes, so it means nothing to
/// `remove`, which is refused rather than ignored.
#[test]
fn prefix_is_refused_outside_install() {
    let harness = Harness::new();
    let stderr = harness.assert_refused(&["remove", "--prefix", "/tmp/elsewhere"]);
    assert!(
        stderr.contains("--prefix applies to install only"),
        "{stderr}"
    );
}

/// IN-1: every helper path the daemon resolves names a file of the
/// installation, so the daemon cannot look up an executable the installer
/// does not install.
#[test]
fn in1_every_path_the_daemon_resolves_is_installed() {
    let overrides = [
        "VM_GUEST_SESSION_AGENT",
        "VM_GUEST_CONSOLE_AGENT",
        "VM_CONSOLE_WORKER",
    ];
    if overrides
        .iter()
        .any(|name| std::env::var_os(name).is_some())
    {
        eprintln!("skipped: an agent override is set in the test environment");
        return;
    }
    let installed: BTreeSet<String> = service_layout::required(true).into_iter().collect();
    let name = |path: PathBuf| path.file_name().unwrap().to_string_lossy().into_owned();
    let mut resolved = vec![
        name(machine_service_core::isolation::forwarder_path(None)),
        name(PathBuf::from(
            machine_service_core::isolation::installer_path(),
        )),
        name(console::guest::agent_path().unwrap()),
    ];
    for kind in ["macos", "linux"] {
        resolved.push(name(
            machine_service_core::guest_link::agent_path_for(kind).unwrap(),
        ));
        resolved.push(name(console::guest::agent_path_for(kind).unwrap()));
    }
    for path in resolved {
        assert!(installed.contains(&path), "{path} is not installed");
    }
}

// ------------------------------------------------------------- DC-140, IN-3

fn relay_plist(harness: &Harness, label: &str) -> PathBuf {
    harness
        .home
        .join(format!("Library/LaunchAgents/{label}.plist"))
}

fn assert_ok(output: std::process::Output) -> String {
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn state_credential(state: &Path) -> String {
    fs::read_to_string(state.join("relay/durable-agent.credential"))
        .unwrap()
        .trim()
        .to_string()
}

fn agent_config(home: &Path) -> serde_json::Value {
    let path = home.join(".config/relay-agent/config.json");
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

/// DC-140: a release whose archive does not match its `SHA256SUMS`, or
/// that has no archive for a platform the installation serves, refuses the
/// installation by name before anything is written.
#[test]
fn dc140_a_broken_or_incomplete_release_refuses_the_installation() {
    let folder_of = |harness: &Harness| harness.bin.join(service_layout::release::FOLDER);
    // An archive that is not the one its sum names.
    let harness = Harness::new();
    let release = service_layout::release::Release::open(&folder_of(&harness)).unwrap();
    let archive = folder_of(&harness).join(&release.archives[1].file);
    fs::write(&archive, b"tampered").unwrap();
    let stderr =
        harness.assert_refused(&["install", "--prefix", prefix(&harness).to_str().unwrap()]);
    assert!(stderr.contains("does not match SHA256SUMS"), "{stderr}");
    assert!(!prefix(&harness).exists());
    assert!(!harness.plist.exists());
    // No release at all.
    let harness = Harness::new();
    fs::remove_dir_all(folder_of(&harness)).unwrap();
    let stderr =
        harness.assert_refused(&["install", "--prefix", prefix(&harness).to_str().unwrap()]);
    assert!(stderr.contains("no relay agent release"), "{stderr}");
    assert!(!prefix(&harness).exists());
    // A release without the Linux archive.
    let harness = Harness::new();
    let sums = folder_of(&harness).join("SHA256SUMS");
    let kept: String = fs::read_to_string(&sums)
        .unwrap()
        .lines()
        .filter(|line| !line.contains("-linux-"))
        .map(|line| format!("{line}\n"))
        .collect();
    fs::write(&sums, kept).unwrap();
    let stderr =
        harness.assert_refused(&["install", "--prefix", prefix(&harness).to_str().unwrap()]);
    assert!(stderr.contains("no archive for linux-"), "{stderr}");
    assert!(!harness.plist.exists());
}

/// DC-140: builds of the relay agent from the earlier layout are removed
/// from the install folder, because nothing reads them.
#[test]
fn dc140_the_earlier_layouts_relay_agent_builds_are_removed() {
    let harness = Harness::new();
    let prefix = prefix(&harness);
    fs::create_dir_all(&prefix).unwrap();
    for retired in service_layout::release::RETIRED {
        write_exec(&prefix.join(retired), "#!/bin/sh\n");
    }
    assert_ok(install_into_prefix(&harness));
    for retired in service_layout::release::RETIRED {
        assert!(!prefix.join(retired).exists(), "{retired}");
    }
}

/// IN-3, RA-2, RA-4: with exposure on, the installer installs the
/// release's macOS relay agent as a LaunchAgent of the desktop session,
/// with the machine's agent credential and a configuration for the Mac's
/// own broker; without exposure it installs none.
#[test]
fn in3_exposure_on_installs_the_relay_agent_from_the_release() {
    let harness = Harness::new();
    assert_ok(install_into_prefix(&harness));
    assert!(!relay_plist(&harness, "com.wezzard.relay-agent").exists());

    assert_ok(harness.run(&[
        "install",
        "--expose-durable",
        "on",
        "--prefix",
        prefix(&harness).to_str().unwrap(),
    ]));
    let plist = relay_plist(&harness, "com.wezzard.relay-agent");
    let value = machine_service_install::plist::parse(&fs::read(&plist).unwrap()).unwrap();
    assert_eq!(value["Label"], "com.wezzard.relay-agent");
    assert_eq!(value["LimitLoadToSessionType"], "Aqua");
    assert_eq!(value["RunAtLoad"], true);
    assert_eq!(value["KeepAlive"], true);
    let executable = harness.home.join(".local/libexec/relay-agent/relay-agent");
    assert_eq!(
        value["ProgramArguments"],
        json!([executable.to_str().unwrap(), "run"])
    );
    // The executable is the installed release's, byte for byte.
    assert_eq!(
        fs::read(&executable).unwrap(),
        release_executable(&prefix(&harness).join(service_layout::release::FOLDER))
    );
    let state = harness.home.join(".local/state/machine-service");
    let config = agent_config(&harness.home);
    assert_eq!(config["broker_url"], "wss://localhost:6241/relay");
    assert_eq!(config["agent_credential"], state_credential(&state));
    assert_eq!(
        config["broker_ca_pem"].as_str().unwrap(),
        fs::read_to_string(state.join("relay/tls/ca.pem")).unwrap()
    );
    let calls = fs::read_to_string(&harness.log).unwrap();
    assert!(
        calls.lines().any(|line| line.starts_with("bootstrap gui/")
            && line.ends_with("com.wezzard.relay-agent.plist")),
        "{calls}"
    );

    // A reinstall keeps the credential; a rotation replaces it.
    let first = state_credential(&state);
    assert_ok(harness.run(&["install", "--prefix", prefix(&harness).to_str().unwrap()]));
    assert_eq!(state_credential(&state), first);
    assert_ok(harness.run(&[
        "install",
        "--rotate-relay-credential",
        "--prefix",
        prefix(&harness).to_str().unwrap(),
    ]));
    let rotated = state_credential(&state);
    assert_ne!(rotated, first);
    assert_eq!(agent_config(&harness.home)["agent_credential"], rotated);

    // Exposure off removes the relay agent, and so does `remove`.
    assert_ok(harness.run(&[
        "install",
        "--expose-durable",
        "off",
        "--prefix",
        prefix(&harness).to_str().unwrap(),
    ]));
    assert!(!plist.exists());
    assert!(!executable.exists());
    assert!(!harness
        .home
        .join(".config/relay-agent/config.json")
        .exists());
    let calls = fs::read_to_string(&harness.log).unwrap();
    assert!(
        calls.lines().any(|line| line.starts_with("bootout gui/")),
        "{calls}"
    );
    assert_ok(harness.run(&[
        "install",
        "--expose-durable",
        "on",
        "--prefix",
        prefix(&harness).to_str().unwrap(),
    ]));
    assert!(plist.exists());
    assert_ok(harness.run(&["remove"]));
    assert!(!plist.exists());
}

/// DC-135: a release whose macOS executable is not signed with the release
/// identifier is refused on an exposed Mac before anything is written.
#[test]
fn dc135_an_unsigned_relay_agent_is_refused() {
    let harness = Harness::new();
    write_release(&harness.bin.join(service_layout::release::FOLDER), false);
    let stderr = harness.assert_refused(&[
        "install",
        "--expose-durable",
        "on",
        "--prefix",
        prefix(&harness).to_str().unwrap(),
    ]);
    assert!(stderr.contains("com.wezzard.relay-agent"), "{stderr}");
    assert!(!prefix(&harness).exists());
    assert!(!harness.plist.exists());
}

/// DC-224: a release, a version without a pre-release part, must be signed
/// with Developer ID; an ad hoc one is refused on an exposed Mac before
/// anything is written, and so are a `SIGNING.json` that disagrees with
/// the executable and a configured team that is no team ID.
#[test]
fn dc224_an_ad_hoc_release_is_refused() {
    let mut harness = Harness::new();
    write_release_of(
        &harness.bin.join(service_layout::release::FOLDER),
        "9.9.9",
        true,
    );
    let installed = prefix(&harness);
    let install = [
        "install",
        "--expose-durable",
        "on",
        "--prefix",
        installed.to_str().unwrap(),
    ];
    let stderr = harness.assert_refused(&install);
    assert!(
        stderr.contains("must be signed with Developer ID"),
        "{stderr}"
    );
    assert!(!prefix(&harness).exists());
    assert!(!harness.plist.exists());
    // Without exposure the Mac installs no relay agent, so nothing asks for
    // its signature.
    let plain = ["install", "--prefix", installed.to_str().unwrap()];
    assert_ok(harness.run(&plain));
    // A record that says Developer ID beside an ad hoc executable.
    let folder = harness.bin.join(service_layout::release::FOLDER);
    write_release(&folder, true);
    fs::write(
        folder.join(service_layout::signing::METADATA),
        r#"{"signature": "developer-id", "team": "EXAMPLE123"}"#,
    )
    .unwrap();
    let stderr = harness.assert_refused(&install);
    assert!(
        stderr.contains("SIGNING.json says developer-id"),
        "{stderr}"
    );
    write_release(&folder, true);
    harness.set_env(service_layout::signing::TEAM_VARIABLE, "not-a-team");
    let stderr = harness.assert_refused(&install);
    assert!(
        stderr.contains("MACHINE_SERVICE_RELAY_AGENT_TEAM_ID"),
        "{stderr}"
    );
}

/// DC-231: an ad hoc pre-release records no trusted team, and a trust
/// record that cannot be read refuses the installation before anything is
/// written, rather than trusting the next team anew.
#[test]
fn dc231_the_trust_record_is_kept_and_never_silently_replaced() {
    let harness = Harness::new();
    let installed = prefix(&harness);
    let install = [
        "install",
        "--expose-durable",
        "on",
        "--prefix",
        installed.to_str().unwrap(),
    ];
    let state = harness.home.join(".local/state/machine-service");
    let record = state.join(service_layout::signing::TRUST_RECORD);
    let stdout = assert_ok(harness.run(&install));
    assert!(
        stdout.contains("relay agent release signed ad hoc"),
        "{stdout}"
    );
    assert!(!record.exists(), "an ad hoc pre-release names no team");
    fs::write(&record, "{").unwrap();
    let removed = harness.home.join(".local/libexec/relay-agent/relay-agent");
    fs::remove_file(&removed).unwrap();
    let stderr = harness.assert_refused(&install);
    assert!(stderr.contains("fix or remove it"), "{stderr}");
    assert!(!removed.exists(), "nothing was written");
    let stderr = harness.assert_refused(&["relay-agent"]);
    assert!(stderr.contains("fix or remove it"), "{stderr}");
}

/// IN-3: a current durable lease refuses an installation that would
/// restart its relay agent, which would end the lease's commands.
#[test]
fn in3_a_current_durable_lease_refuses_the_relay_agent() {
    let harness = Harness::new();
    let state = harness.home.join(".local/state/machine-service");
    fs::create_dir_all(&state).unwrap();
    fs::write(state.join("durable.json"), r#"{"current": "lease"}"#).unwrap();
    let stderr = harness.assert_refused(&["install", "--expose-durable", "on"]);
    assert!(stderr.contains("a durable lease is current"), "{stderr}");
    let stderr = harness.assert_refused(&["relay-agent"]);
    assert!(stderr.contains("a durable lease is current"), "{stderr}");
}

/// TL-6, DC-213: `relay-agent` installs a second instance's relay
/// agent under the label and home its state folder names, with its own
/// session port, and never touches machine-service's LaunchAgent or the
/// installed instance's agent in the operator's home; `remove-relay-agent`
/// removes it whole.
#[test]
fn tl6_a_second_instance_gets_its_own_relay_agent() {
    let mut harness = Harness::new();
    let state = harness.home.join("second/state");
    harness.set_env("MACHINE_SERVICE_STATE", state.to_str().unwrap());
    harness.set_env("MACHINE_SERVICE_SESSION_PORT", "6299");
    assert_ok(harness.run(&["relay-agent"]));
    let identity = machine_service_install::relay_agent::identity(&state, &harness.home);
    assert!(identity.label.starts_with("com.wezzard.relay-agent."));
    let agent_home = state.canonicalize().unwrap().join("relay/agent");
    assert_eq!(identity.root, agent_home);
    assert_eq!(identity.home, harness.home);
    let plist = agent_home.join(format!("Library/LaunchAgents/{}.plist", identity.label));
    assert!(plist.exists());
    assert!(!relay_plist(&harness, "com.wezzard.relay-agent").exists());
    assert!(
        !harness.plist.exists(),
        "machine-service's agent is untouched"
    );
    assert!(!harness.home.join(".config/relay-agent").exists());
    assert!(!harness.home.join(".local/libexec/relay-agent").exists());
    let config = agent_config(&agent_home);
    assert_eq!(config["broker_url"], "wss://localhost:6299/relay");
    assert_eq!(config["agent_credential"], state_credential(&state));
    let calls = fs::read_to_string(&harness.log).unwrap();
    assert!(
        !calls
            .lines()
            .any(|line| line.contains("com.wezzard.machine-service")),
        "{calls}"
    );
    assert!(
        calls
            .lines()
            .any(|line| line.starts_with("bootstrap gui/") && line.ends_with(".plist")),
        "{calls}"
    );

    assert_ok(harness.run(&["remove-relay-agent"]));
    assert!(!agent_home.exists());
    assert!(state.join("relay/durable-agent.credential").exists());

    let stderr = harness.assert_refused(&["relay-agent", "--label", "x"]);
    assert!(stderr.contains("--label"), "{stderr}");
}

/// IN-3: without a desktop session the relay agent cannot run in one, and
/// the installer says so instead of loading it elsewhere.
#[test]
fn in3_no_desktop_session_refuses_the_relay_agent() {
    let harness = Harness::new();
    fs::write(harness.home.join("no-gui"), "").unwrap();
    let stderr = harness.assert_refused(&["relay-agent"]);
    assert!(stderr.contains("no desktop session"), "{stderr}");
    assert!(!relay_plist(&harness, "com.wezzard.relay-agent").exists());
}
