//! A test build never enters an installation (DC-258).
//!
//! `cargo test` builds the daemon, the CLI, the installer and the relay
//! agent with the test seams, which read `MACHINE_SERVICE_TEST_*` and accept
//! a test's own tools; each carries a marker
//! (`service_layout::test_build::carries_marker`). A release's installer
//! refuses any executable of the installation, and the relay agent of its
//! release, that carries it. The tests here prove the detection on the very
//! binaries `cargo test` built, and run a release build of the installer,
//! made for the test in a target folder of its own, in its read-only
//! `--check` mode, which writes nothing and runs no `launchctl`.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::Harness;
use service_layout::test_build::{carries_marker, harness, FEATURE};

/// The executable of the installer `cargo test` built: a test build.
const TEST_INSTALLER: &str = env!("CARGO_BIN_EXE_machine-service-install");

fn marked(path: &Path) -> bool {
    carries_marker(&fs::read(path).unwrap())
}

#[test]
fn every_binary_a_test_runs_is_marked_and_a_system_one_is_not() {
    assert!(marked(Path::new(TEST_INSTALLER)));
    assert!(!marked(Path::new("/usr/bin/true")));
    // The test process itself only links the seams; the marker is kept by
    // the binaries that start through them.
    let staged = tempfile::tempdir().unwrap();
    fs::copy(TEST_INSTALLER, staged.path().join("machine-service")).unwrap();
    fs::copy("/usr/bin/true", staged.path().join("machctl")).unwrap();
    let found = machine_service_install::installer::test_builds_in(
        staged.path(),
        &["machine-service".to_string(), "machctl".to_string()],
    );
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].contains("machine-service") && found[0].contains(FEATURE),
        "{found:?}"
    );
}

/// A release whose macOS relay agent is a test build is refused too.
#[test]
fn a_release_holding_a_test_build_of_the_agent_is_found() {
    let staged = tempfile::tempdir().unwrap();
    let folder = staged.path().join(service_layout::release::FOLDER);
    write_release_holding(&folder, Path::new(TEST_INSTALLER));
    let found = machine_service_install::installer::test_builds_in(staged.path(), &[]);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].contains("relay agent"), "{found:?}");
    write_release_holding(&folder, Path::new("/usr/bin/true"));
    assert!(machine_service_install::installer::test_builds_in(staged.path(), &[]).is_empty());
}

/// A release folder whose macOS archive holds `executable`.
fn write_release_holding(folder: &Path, executable: &Path) {
    use service_layout::release;
    let _ = fs::remove_dir_all(folder);
    fs::create_dir_all(folder).unwrap();
    let stage = tempfile::tempdir().unwrap();
    fs::copy(executable, stage.path().join(release::MEMBER)).unwrap();
    let file = format!(
        "relay-agent-9.9.9-adhoc-{}.tar.gz",
        release::platform_for("macos")
    );
    let status = Command::new("/usr/bin/tar")
        .arg("-C")
        .arg(stage.path())
        .arg("-czf")
        .arg(folder.join(&file))
        .arg(release::MEMBER)
        .status()
        .unwrap();
    assert!(status.success());
    let bytes = fs::read(folder.join(&file)).unwrap();
    fs::write(
        folder.join(release::SUMS),
        format!("{}  {file}\n", release::sha256_hex(&bytes)),
    )
    .unwrap();
}

/// The installer built as a release builds it, without the seams, into a
/// target folder of its own beside the workspace's.
fn release_installer() -> PathBuf {
    let tmp = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let target = tmp.parent().unwrap().join("release-check");
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap();
    let status = Command::new(env!("CARGO"))
        .args([
            "build",
            "-p",
            "machine-service-install",
            "--bin",
            "machine-service-install",
        ])
        .current_dir(workspace)
        .env("CARGO_TARGET_DIR", &target)
        .status()
        .expect("build the release installer");
    assert!(status.success(), "the release installer did not build");
    target.join("debug/machine-service-install")
}

/// DC-258: a release's installer refuses an installation folder that
/// holds a test build, before it writes anything or runs any `launchctl`.
#[test]
fn a_release_installer_refuses_a_test_build() {
    let release = release_installer();
    assert!(!marked(&release), "a release build carries the test marker");
    let harness = Harness::new();
    // The installation folder holds the release installer, and a daemon
    // that `cargo test` built.
    fs::copy(&release, &harness.installer).unwrap();
    fs::copy(TEST_INSTALLER, harness.bin.join("machine-service")).unwrap();
    let before = tree(&harness.home);
    let mut command = harness::command(&harness.installer);
    command
        .args(["install", "--check"])
        .env("HOME", &harness.home)
        .env("TART", &harness.tart)
        .env("PATH", format!("{}:/usr/bin:/bin", harness.bin.display()));
    let output = command.output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("must never be installed") && stderr.contains("machine-service"),
        "{stderr}"
    );
    assert_eq!(tree(&harness.home), before, "the refusal wrote something");
    assert!(!harness.log.exists(), "the refusal ran launchctl");

    // The same folder with no test build passes the check.
    fs::write(
        harness.bin.join("machine-service"),
        "#!/bin/sh\n# machine-service\nexit 0\n",
    )
    .unwrap();
    let output = command.output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("must never be installed"), "{stderr}");
}

/// Every path under `root`, with its length.
fn tree(root: &Path) -> Vec<(PathBuf, u64)> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        found.push((path.clone(), if meta.is_file() { meta.len() } else { 0 }));
        if meta.is_dir() {
            stack.extend(
                fs::read_dir(&path)
                    .unwrap()
                    .flatten()
                    .map(|entry| entry.path()),
            );
        }
    }
    found.sort();
    found
}
