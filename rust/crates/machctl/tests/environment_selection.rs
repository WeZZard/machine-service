//! Process-level ports of the CLI selector cases in
//! `tests/unit/test_environment_config.py`.
//!
//! The Python tests shell out to the real `bin/vmctl` with a hand-built
//! `environment.json`, so these tests drive the real `machctl` binary with the
//! equivalent fixture. `machctl environment --json` is fully offline: no
//! daemon, VM, or network is touched.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

use serde_json::{json, Map, Value};
use tempfile::TempDir;

/// The `machctl` binary under test, provided by Cargo for this package.
const MACHCTL: &str = env!("CARGO_BIN_EXE_machctl");

/// Write an executable helper and return its path.
fn write_exec(dir: &Path, name: &str, content: &str) -> PathBuf {
    fs::create_dir_all(dir).expect("shim dir");
    let path = dir.join(name);
    fs::write(&path, content).expect("write shim");
    let mut permissions = fs::metadata(&path).expect("shim metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).expect("chmod shim");
    path
}

/// One isolated selector fixture: profile, images repository, fake Tart, and a
/// private `HOME`, mirroring the Python `EnvironmentFixture`.
struct Fixture {
    _tmp: TempDir,
    root: PathBuf,
    home: PathBuf,
    profile: Map<String, Value>,
    file: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canonical temp dir");
        let home = root.join("home");
        let repo = root.join("images-repo");
        fs::create_dir_all(&home).expect("home");
        fs::create_dir_all(repo.join("images")).expect("repo");
        let fake = write_exec(
            &root,
            "fake-tart",
            "#!/bin/sh\nprintf \"%s\\n\" \"$TART_HOME\"\n",
        );

        let mut profile = Map::new();
        profile.insert("schemaVersion".to_string(), json!(2));
        profile.insert("id".to_string(), json!("fixture"));
        profile.insert("vmServiceUrl".to_string(), json!("http://127.0.0.1:6249/"));
        profile.insert("imageRepository".to_string(), json!(repo.to_string_lossy()));
        profile.insert(
            "tartHome".to_string(),
            json!(root.join("store").to_string_lossy()),
        );
        profile.insert(
            "serviceStateDir".to_string(),
            json!(root.join("service").to_string_lossy()),
        );
        profile.insert(
            "imageStateDir".to_string(),
            json!(root.join("images-state").to_string_lossy()),
        );
        profile.insert(
            "relayStateDir".to_string(),
            json!(root.join("relay").to_string_lossy()),
        );
        // `machctlPath` and `tartPath` must be executable regular files; the
        // real binary satisfies both roles for `machctlPath`.
        profile.insert("machctlPath".to_string(), json!(MACHCTL));
        profile.insert("tartPath".to_string(), json!(fake.to_string_lossy()));

        let file = root.join("environment.json");
        let fixture = Fixture {
            _tmp: tmp,
            root,
            home,
            profile,
            file,
        };
        fixture.save();
        fixture
    }

    fn save(&self) {
        fs::write(
            &self.file,
            serde_json::to_vec(&Value::Object(self.profile.clone())).expect("profile json"),
        )
        .expect("write profile");
    }

    /// Run `machctl` with the fixture `HOME` and explicit environment overrides.
    ///
    /// Ambient `VM_ENVIRONMENT_FILE` / `VM_ENVIRONMENT_FINGERPRINT` are
    /// cleared first so the operator's shell cannot influence the test;
    /// callers re-add the values they want to exercise.
    fn run(&self, arguments: &[&str], env: &[(&str, &str)]) -> Output {
        let mut command = service_layout::test_build::harness::command(MACHCTL);
        command
            .args(arguments)
            .env_remove("VM_ENVIRONMENT_FILE")
            .env_remove("VM_ENVIRONMENT_FINGERPRINT")
            .env("HOME", &self.home)
            .env("MACHINE_SERVICE_PORT", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in env {
            command.env(key, value);
        }
        command.output().expect("run machctl")
    }

    fn profile_path(&self) -> &str {
        self.file.to_str().expect("utf8 profile path")
    }
}

/// `test_cli_selector_overrides_invalid_ambient_selector`
///
/// An explicit `--environment` wins over a bad ambient `VM_ENVIRONMENT_FILE`.
#[test]
fn test_cli_selector_overrides_invalid_ambient_selector() {
    let fixture = Fixture::new();
    let result = fixture.run(
        &[
            "--environment",
            fixture.profile_path(),
            "environment",
            "--json",
        ],
        &[("VM_ENVIRONMENT_FILE", "/absent")],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

/// `test_bound_cli_sequence_refuses_profile_edits_but_explicit_selection_can_change`
///
/// A bound `VM_ENVIRONMENT_FINGERPRINT` that no longer matches the ambient
/// profile is refused with a "changed during" diagnostic, while an explicit
/// `--environment` selection may switch profile.
#[test]
fn test_bound_cli_sequence_refuses_profile_edits_but_explicit_selection_can_change() {
    let fixture = Fixture::new();

    // Resolve the pristine profile to obtain its identity fingerprint, exactly
    // as the Python test's `bundle = self.resolve()` does.
    let bundle_result = fixture.run(
        &[
            "--environment",
            fixture.profile_path(),
            "environment",
            "--json",
        ],
        &[],
    );
    assert!(
        bundle_result.status.success(),
        "{}",
        String::from_utf8_lossy(&bundle_result.stderr)
    );
    let bundle: Value = serde_json::from_slice(&bundle_result.stdout).expect("bundle json");
    let fingerprint = bundle["identity"]["fingerprint"]
        .as_str()
        .expect("fingerprint")
        .to_string();

    // Edit the profile in place; the bound fingerprint no longer matches.
    let mut edited = fixture.profile.clone();
    edited.insert("id".to_string(), json!("edited-profile"));
    fs::write(
        &fixture.file,
        serde_json::to_vec(&Value::Object(edited)).expect("edited profile json"),
    )
    .expect("write edited profile");

    let bound = fixture.run(
        &["environment", "--json"],
        &[
            ("VM_ENVIRONMENT_FILE", fixture.profile_path()),
            ("VM_ENVIRONMENT_FINGERPRINT", &fingerprint),
        ],
    );
    assert_ne!(bound.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&bound.stderr).contains("changed during"),
        "{}",
        String::from_utf8_lossy(&bound.stderr)
    );

    let explicit = fixture.run(
        &[
            "--environment",
            fixture.profile_path(),
            "environment",
            "--json",
        ],
        &[
            ("VM_ENVIRONMENT_FILE", fixture.profile_path()),
            ("VM_ENVIRONMENT_FINGERPRINT", &fingerprint),
        ],
    );
    assert!(
        explicit.status.success(),
        "{}",
        String::from_utf8_lossy(&explicit.stderr)
    );
    let bundle: Value = serde_json::from_slice(&explicit.stdout).expect("bundle json");
    assert_eq!(bundle["identity"]["id"], "edited-profile");
    assert!(!fixture.root.join("store").exists());
}

/// AC-45: `machctl --environment <profile> environment --json` refuses a
/// profile at schemaVersion 1 with a `vmctlPath` field, naming `machctlPath`
/// and schemaVersion 2; the same profile at schemaVersion 2 with
/// `machctlPath` prints the resolved profile and exports `MACHCTL`, and no
/// `VMCTL`.
#[test]
fn ac45_old_profile_is_refused_and_schema_two_exports_machctl() {
    let mut fixture = Fixture::new();
    let machctl = fixture.profile.remove("machctlPath").expect("machctlPath");
    fixture
        .profile
        .insert("vmctlPath".to_string(), machctl.clone());
    fixture
        .profile
        .insert("schemaVersion".to_string(), json!(1));
    fixture.save();

    let output = fixture.run(
        &[
            "--environment",
            fixture.profile_path(),
            "environment",
            "--json",
        ],
        &[],
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("machctlPath"), "{stderr}");
    assert!(stderr.contains("schemaVersion 2"), "{stderr}");
    assert!(output.stdout.is_empty());

    fixture.profile.remove("vmctlPath");
    fixture
        .profile
        .insert("machctlPath".to_string(), machctl.clone());
    fixture
        .profile
        .insert("schemaVersion".to_string(), json!(2));
    fixture.save();

    let output = fixture.run(
        &[
            "--environment",
            fixture.profile_path(),
            "environment",
            "--json",
        ],
        &[],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let bundle: Value = serde_json::from_slice(&output.stdout).expect("bundle json");
    assert_eq!(bundle["profile"]["schemaVersion"], json!(2));
    assert_eq!(bundle["environment"]["MACHCTL"], machctl);
    let exports = bundle["environment"].as_object().expect("exports");
    assert!(!exports.contains_key("VMCTL"));
    assert!(exports.keys().all(|key| !key.starts_with("VM_SERVICE_")));
}
