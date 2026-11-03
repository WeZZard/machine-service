//! The image catalog at the component layer (`docs/arch/image-catalog.md`,
//! IC-6, TL-2): the real daemon against a fixture image repository and the
//! fake Tart store, never restarted.
//!
//! Each test cites the acceptance criterion it proves (AC-39 to AC-41 in
//! `docs/acceptance/operator.acceptance`). The live layer (TL-4) is in
//! `live_image_catalog.rs`.

mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use common::{LegacyFixture, Response};
use serde_json::{json, Value};

/// Replace a `line.conf` in one rename, as an editor's atomic save does.
fn write_line(pilot: &Path, image: &str, text: &str) -> PathBuf {
    let directory = pilot.join("images").join(image);
    std::fs::create_dir_all(&directory).expect("line folder");
    let path = directory.join("line.conf");
    let temporary = directory.join(".line.conf.tmp");
    std::fs::write(&temporary, text).expect("write line.conf");
    std::fs::rename(&temporary, &path).expect("replace line.conf");
    path
}

/// Edit the fake Tart store's VM list (`name=running` per line, as the shim
/// keeps it): drop `remove`, then add `add` stopped. Other VMs, running
/// leases included, keep their lines.
fn edit_fake_vms(fixture: &LegacyFixture, remove: Option<&str>, add: &str) {
    let path = fixture.tart_home.join("fake-vms");
    let mut text: String = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter(|line| Some(line.split('=').next().unwrap_or_default()) != remove)
        .map(|line| format!("{line}\n"))
        .collect();
    text.push_str(&format!("{add}=0\n"));
    std::fs::write(&path, text).expect("fake-vms");
}

fn images(fixture: &LegacyFixture) -> Response {
    fixture.request("GET", "/images", None)
}

fn image_names(response: &Response) -> Vec<String> {
    response.body["images"]
        .as_object()
        .map(|images| images.keys().cloned().collect())
        .unwrap_or_default()
}

fn acquire(fixture: &LegacyFixture, purpose: &str, image: &str) -> Value {
    let response = fixture.request(
        "POST",
        "/acquire",
        Some(&json!({"purpose": purpose, "image": image, "env": "none"})),
    );
    assert_eq!(response.status, 200, "acquire {image}: {}", response.body);
    response.body
}

fn error_lines(fixture: &LegacyFixture) -> Vec<String> {
    std::fs::read_to_string(fixture.state_dir.join("service.log"))
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains("ERROR"))
        .map(str::to_string)
        .collect()
}

const EXTRA: &str = "LINE_KIND=linux\nBASE_VM=pilot-extra-base\nCLONE_PREFIX=pilot-extra-\nGUEST_USER=admin\nGUEST_PASS=admin\n";

/// AC-39: adding a line's folder shows the image on the next request and an
/// acquisition of it succeeds; removing a line hides it while a running
/// lease of it can still heartbeat and release. The daemon's process never
/// changes.
#[test]
fn ac39_adding_and_removing_a_line_without_a_restart() {
    let fixture = LegacyFixture::new();
    let daemon = fixture.start_daemon();
    let pid = daemon.pid();
    assert_eq!(image_names(&images(&fixture)), ["macos26", "ubuntu2404"]);

    let ubuntu = acquire(&fixture, "kept-lease", "ubuntu2404");
    let ubuntu_vm = ubuntu["vm"].as_str().expect("vm").to_string();

    edit_fake_vms(&fixture, None, "pilot-extra-base");
    write_line(&fixture.pilot, "linux-extra", EXTRA);
    let listed = images(&fixture);
    assert_eq!(listed.status, 200, "{}", listed.body);
    assert_eq!(
        image_names(&listed),
        ["linux-extra", "macos26", "ubuntu2404"]
    );
    assert_eq!(listed.body["catalog"]["status"], "current");
    assert_eq!(listed.body["catalog"]["generation"], 2);
    let machines = fixture.request("GET", "/machines", None);
    assert_eq!(machines.body["catalog"]["generation"], 2);
    assert_eq!(machines.body["catalog"]["status"], "current");

    let extra = acquire(&fixture, "new-line", "linux-extra");
    assert_eq!(extra["image"], "linux-extra");
    assert!(extra["vm"]
        .as_str()
        .expect("vm")
        .starts_with("pilot-extra-"));

    std::fs::remove_dir_all(fixture.pilot.join("images/ubuntu2404")).expect("remove line");
    let listed = images(&fixture);
    assert_eq!(image_names(&listed), ["linux-extra", "macos26"]);

    let heartbeat = fixture.request(
        "POST",
        &format!("/machines/{ubuntu_vm}/heartbeat"),
        Some(&json!({"ttl_hours": 2})),
    );
    assert_eq!(heartbeat.status, 200, "{}", heartbeat.body);
    for vm in [ubuntu_vm.as_str(), extra["vm"].as_str().expect("vm")] {
        let released =
            fixture.request("POST", &format!("/machines/{vm}/release"), Some(&json!({})));
        assert_eq!(released.status, 200, "release {vm}: {}", released.body);
    }
    assert_eq!(daemon.pid(), pid, "the daemon was restarted");
}

/// AC-40: editing CPU shows in `machctl images-show`, and renaming the work
/// VM to the base name, as promotion does, makes the image acquirable with
/// no rebuild at all.
#[test]
fn ac40_editing_a_line_and_promoting_a_base_without_a_restart() {
    let fixture = LegacyFixture::new();
    write_line(
        &fixture.pilot,
        "macos26",
        "LINE_KIND=macos\nBASE_VM=pilot-macos26-base\nCLONE_PREFIX=pilot-mac-\nGUEST_USER=station\nGUEST_PASS=station\nCPU=6\n",
    );
    let daemon = fixture.start_daemon();
    let pid = daemon.pid();
    let cpu = |fixture: &LegacyFixture| {
        let output = common::run_machctl(&["images-show", "macos26"], &fixture.machctl_env());
        assert!(output.status.success(), "{output:?}");
        let shown: Value = serde_json::from_slice(&output.stdout).expect("image json");
        // One object per instance that lists the image (CL-4).
        shown[0]["answer"]["defaults"]["cpu"].clone()
    };
    assert_eq!(cpu(&fixture), 6);
    write_line(
        &fixture.pilot,
        "macos26",
        "LINE_KIND=macos\nBASE_VM=pilot-macos26-base\nCLONE_PREFIX=pilot-mac-\nGUEST_USER=station\nGUEST_PASS=station\nCPU=4\n",
    );
    assert_eq!(cpu(&fixture), 4);

    edit_fake_vms(&fixture, None, "pilot-promoted-work");
    write_line(
        &fixture.pilot,
        "promoted",
        "LINE_KIND=linux\nBASE_VM=pilot-promoted-base\nWORK_VM=pilot-promoted-work\nCLONE_PREFIX=pilot-promoted-\nGUEST_USER=admin\nGUEST_PASS=admin\n",
    );
    let before = images(&fixture);
    assert_eq!(before.body["images"]["promoted"]["base_available"], false);
    assert_eq!(
        before.body["images"]["promoted"]["concurrency"]["acquirable"],
        false
    );
    let generation = before.body["catalog"]["generation"].clone();

    // `tart rename pilot-promoted-work pilot-promoted-base`.
    edit_fake_vms(&fixture, Some("pilot-promoted-work"), "pilot-promoted-base");
    let after = images(&fixture);
    assert_eq!(after.body["images"]["promoted"]["base_available"], true);
    assert_eq!(
        after.body["images"]["promoted"]["concurrency"]["acquirable"],
        true
    );
    assert_eq!(
        after.body["catalog"]["generation"], generation,
        "a promotion needs no rebuild"
    );
    assert_eq!(daemon.pid(), pid);
}

/// AC-41: a syntax error keeps every image of generation 7, reports `stale`
/// with the file, the error and the time in `/images`, `/machines`,
/// `/health` and the CLI, writes exactly one ERROR line however many
/// requests follow, and fixing the file returns `current` at a new
/// generation. `POST /images/reload` reaches generation 7.
#[test]
fn ac41_a_broken_line_keeps_the_last_good_list_and_is_loud() {
    let fixture = LegacyFixture::new();
    let _daemon = fixture.start_daemon();
    for expected in 2..=7 {
        let reloaded = fixture.request("POST", "/images/reload", None);
        assert_eq!(reloaded.status, 200, "{}", reloaded.body);
        assert_eq!(reloaded.body["catalog"]["generation"], expected);
        assert_eq!(reloaded.body["catalog"]["status"], "current");
    }

    let broken = write_line(
        &fixture.pilot,
        "macos26",
        "LINE_KIND=macos\nBASE_VM=pilot-macos26-base\nif then ( fi\n",
    );
    for _ in 0..5 {
        let listed = images(&fixture);
        assert_eq!(listed.status, 200, "{}", listed.body);
        assert_eq!(image_names(&listed), ["macos26", "ubuntu2404"]);
        let catalog = &listed.body["catalog"];
        assert_eq!(catalog["status"], "stale");
        assert_eq!(catalog["generation"], 7);
        assert_eq!(catalog["file"], broken.display().to_string());
        assert!(catalog["error"]
            .as_str()
            .expect("error")
            .contains("parse failed"));
        assert!(catalog["failed_at"].as_f64().expect("failed_at") > 0.0);
    }
    let health = fixture.request("GET", "/health", None);
    assert_eq!(health.body["catalog"]["status"], "stale");
    let machines = fixture.request("GET", "/machines", None);
    assert_eq!(machines.body["catalog"]["status"], "stale");

    let listing = common::run_machctl(&["list", "--local"], &fixture.machctl_env());
    let text = String::from_utf8_lossy(&listing.stdout);
    // CL-1: the warning stands above its instance's rows, under the header,
    // and names the instance.
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[0].starts_with("INSTANCE"), "{text}");
    let warning = lines.get(1).expect("a warning line");
    let instance = lines
        .get(2)
        .and_then(|row| row.split_whitespace().next())
        .expect("the instance's row");
    assert!(
        warning.starts_with(&format!(
            "machctl: warning: {instance}: image catalog stale since "
        )),
        "{text}"
    );
    assert!(warning.contains(&broken.display().to_string()), "{text}");
    let reload = common::run_machctl(&["images", "--reload"], &fixture.machctl_env());
    assert_eq!(reload.status.code(), Some(1), "{reload:?}");

    assert_eq!(
        error_lines(&fixture).len(),
        1,
        "{:?}",
        error_lines(&fixture)
    );
    assert!(error_lines(&fixture)[0].contains(&broken.display().to_string()));

    write_line(
        &fixture.pilot,
        "macos26",
        "LINE_KIND=macos\nBASE_VM=pilot-macos26-base\nCLONE_PREFIX=pilot-mac-\nGUEST_USER=station\nGUEST_PASS=station\n",
    );
    let listed = images(&fixture);
    assert_eq!(listed.body["catalog"]["status"], "current");
    assert_eq!(listed.body["catalog"]["generation"], 8);
    assert!(listed.body["catalog"].get("error").is_none());
    let reload = common::run_machctl(&["images", "--reload"], &fixture.machctl_env());
    assert!(reload.status.success(), "{reload:?}");
}

/// AC-41: a legacy startup with broken files serves with the catalog
/// `unavailable`, answers catalog requests with HTTP 503 and the
/// diagnostic, and builds at once when the files are fixed.
#[test]
fn ac41_a_broken_start_is_unavailable_until_the_files_are_fixed() {
    let fixture = LegacyFixture::new();
    let broken = write_line(&fixture.pilot, "macos26", "false\n");
    let _daemon = fixture.start_daemon();

    let listed = images(&fixture);
    assert_eq!(listed.status, 503, "{}", listed.body);
    assert_eq!(listed.body["catalog"]["status"], "unavailable");
    assert_eq!(listed.body["catalog"]["generation"], Value::Null);
    assert_eq!(listed.body["catalog"]["file"], broken.display().to_string());
    assert!(listed.body["error"]
        .as_str()
        .expect("diagnostic")
        .contains("image catalog unavailable"));
    assert_eq!(fixture.request("GET", "/applications", None).status, 503);
    let health = fixture.request("GET", "/health", None);
    assert_eq!(health.status, 200);
    assert_eq!(health.body["catalog"]["status"], "unavailable");
    let refused = fixture.request(
        "POST",
        "/acquire",
        Some(&json!({"purpose": "nothing", "image": "ubuntu2404", "env": "none"})),
    );
    assert_eq!(refused.status, 409);
    assert!(refused.body["error"]
        .as_str()
        .expect("error")
        .contains("image catalog unavailable"));
    let shown = common::run_machctl(&["images-show", "macos26"], &fixture.machctl_env());
    assert_eq!(shown.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&shown.stderr).contains("image catalog unavailable"));

    write_line(
        &fixture.pilot,
        "macos26",
        "LINE_KIND=macos\nBASE_VM=pilot-macos26-base\nCLONE_PREFIX=pilot-mac-\n",
    );
    let listed = images(&fixture);
    assert_eq!(listed.status, 200, "{}", listed.body);
    assert_eq!(listed.body["catalog"]["status"], "current");
    assert_eq!(listed.body["catalog"]["generation"], 1);
    assert_eq!(error_lines(&fixture).len(), 1);
}

/// MM-3 (DC-67): `/machines` offers one virtual machine per image, each with
/// the image's capabilities and its `acquirable` figure; an image with a
/// validated inventory reports its software, one without reports the
/// reason; a running virtual machine carries its image's report.
#[test]
fn mm3_offers_carry_image_capabilities() {
    let fixture = LegacyFixture::new();
    let _daemon = fixture.start_daemon();
    let machines = fixture.request("GET", "/machines", None);
    assert_eq!(machines.status, 200, "{}", machines.body);
    let offers = machines.body["offers"].as_array().expect("offers").clone();
    let names: Vec<&str> = offers
        .iter()
        .map(|offer| offer["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["macos26", "ubuntu2404"]);
    for offer in &offers {
        assert_eq!(offer["kind"], "virtual");
        assert_eq!(offer["state"], "offer");
        assert_eq!(offer["capabilities"]["schema_version"], 1);
        assert_eq!(offer["capabilities"]["source"], "image");
        assert_eq!(offer["capabilities"]["gpu"]["metal3"], false);
    }
    let (macos, ubuntu) = (&offers[0], &offers[1]);
    // The offer's figures are `GET /images`'s. `acquirable` of a macOS
    // image also reads the host-wide guest gauge, which other suites move,
    // so only its type is pinned.
    let listed = images(&fixture).body;
    for offer in &offers {
        let image = offer["image"].as_str().unwrap();
        let entry = &listed["images"][image];
        for figure in ["running", "limit"] {
            assert_eq!(offer["concurrency"][figure], entry["concurrency"][figure]);
        }
        assert!(offer["concurrency"]["acquirable"].is_boolean(), "{offer}");
        assert_eq!(offer["defaults"], entry["defaults"]);
        assert_eq!(offer["base_available"], entry["base_available"]);
    }
    assert_eq!(macos["capabilities"]["os"]["name"], "macOS");
    assert_eq!(
        macos["capabilities"]["gpu"]["metal_families"],
        json!(["mac2", "common3"])
    );
    assert_eq!(macos["capabilities"]["software"], Value::Null);
    assert!(macos["capabilities"]["unknown"]["software"]
        .as_str()
        .is_some_and(|reason| reason.contains("macos26")));
    assert_eq!(ubuntu["capabilities"]["os"]["name"], "Linux");
    assert!(ubuntu["capabilities"]["software"].is_object(), "{ubuntu}");

    let leased = acquire(&fixture, "offer-check", "ubuntu2404");
    let vm = leased["vm"].as_str().expect("vm").to_string();
    let machines = fixture.request("GET", "/machines", None).body;
    assert_eq!(
        machines["machines"][&vm]["capabilities"]["os"]["name"],
        "Linux"
    );
    let released = fixture.request("POST", &format!("/machines/{vm}/release"), Some(&json!({})));
    assert_eq!(released.status, 200, "{}", released.body);
}

/// IC-5: under a selected environment a startup whose catalog cannot be
/// built still refuses to start.
#[test]
fn ic5_a_selected_startup_with_a_broken_catalog_refuses_to_start() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = common::canonical_dir(dir.path());
    let home = root.join("home");
    let repo = root.join("images-repo");
    let bin = root.join("bin");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(&bin).expect("bin");
    write_line(
        &repo,
        "ubuntu2404",
        "LINE_KIND=linux\nBASE_VM=pilot-ubuntu-base\nfalse\n",
    );
    common::write_exec(&root, "fake-tart", common::TART_SHIM);
    common::install_shims(&bin);
    let port = common::free_port();
    let state = root.join("attempt");
    let profile = json!({
        "schemaVersion": 2,
        "id": "fixture",
        "vmServiceUrl": format!("http://127.0.0.1:{port}/"),
        "imageRepository": repo.to_string_lossy(),
        "tartHome": state.join("store").to_string_lossy(),
        "serviceStateDir": state.join("service").to_string_lossy(),
        "imageStateDir": state.join("images-state").to_string_lossy(),
        "relayStateDir": state.join("relay").to_string_lossy(),
        "machctlPath": common::machctl_bin().to_string_lossy(),
        "tartPath": root.join("fake-tart").to_string_lossy(),
    });
    let profile_path = root.join("environment.json");
    std::fs::write(&profile_path, profile.to_string()).expect("profile");
    let mut environ = HashMap::new();
    environ.insert("HOME".to_string(), home.to_string_lossy().into_owned());
    environment::load_environment_with(profile_path.to_str(), &environ)
        .expect("a valid profile")
        .expect("selected");

    let err = root.join("daemon.err");
    let mut command = common::daemon_command(&root);
    command
        .args(["--environment", profile_path.to_str().expect("path")])
        .env("HOME", &home)
        .env("PATH", common::path_with(&bin))
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&err).expect("stderr file"));
    let mut child = command.spawn().expect("spawn daemon");
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait") {
            break status;
        }
        if Instant::now() >= deadline {
            panic!("a selected daemon with a broken catalog kept running");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let stderr = std::fs::read_to_string(&err).unwrap_or_default();
    assert!(!status.success());
    assert!(
        stderr.contains("selected image configuration is invalid"),
        "{stderr}"
    );
}
