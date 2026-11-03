//! The image catalog at the live layer (`docs/arch/image-catalog.md`, IC-6;
//! `docs/arch/test-layers.md`, TL-4; AC-39, AC-40, AC-18).
//!
//! A second instance built from this worktree, with real Tart and its own
//! image checkout, sees a line appear, its base get promoted and the line
//! disappear, without a restart. The test clones one stopped VM and renames
//! the clone, as promotion does; it boots nothing and deletes what it made.
//!
//! ```sh
//! cp -R ~/path/to/pilot-images /tmp/ic-live-pilot     # a checkout the test may write
//! PILOT_REPO=/tmp/ic-live-pilot LIVE_PORT=6250 LIVE_BROKER_PORT=6251 \
//!   LIVE_TART_HOME=<store> scripts/live-instance.sh start ssh
//! LIVE_ROOT=$PWD/target/live/6250 LIVE_PILOT=/tmp/ic-live-pilot \
//!   cargo test -p machine-service --test live_image_catalog -- --ignored
//! ```
//!
//! `LIVE_ROOT` names the instance by its launcher root, and
//! `LIVE_CLONE_SOURCE` (default `pilot-ubuntu-base`) the stopped VM to clone.
//! The test refuses an instance that does not report itself as a second
//! instance and one whose image checkout is not `LIVE_PILOT`. Tart cannot
//! clone from one store into another, so the clone is made in the
//! instance's store, which may be the operator's: the source is only read,
//! the clones carry the instance's prefix, so the launcher's `stop` deletes
//! them if the test cannot, and no clone prunes the store's cache
//! (DC-1xx-LIVE-4).

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use second_instance::{Manifest, INSTALLED_PORT};
use serde_json::Value;

fn listener_pid(port: u16) -> Option<String> {
    let output = Command::new("lsof")
        .args(["-t", &format!("-iTCP:{port}"), "-sTCP:LISTEN"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// `tart` in the instance's own store, without pruning its cache.
fn tart_in(store: &Path) -> Command {
    let mut command = Command::new("tart");
    command
        .env("TART_HOME", store)
        .env("TART_NO_AUTO_PRUNE", "1");
    command
}

fn tart(store: &Path, arguments: &[&str]) {
    let status = tart_in(store).args(arguments).status().expect("run tart");
    assert!(status.success(), "tart {arguments:?} failed");
}

/// Removes the test's line folder and VMs, however the test ends.
struct Cleanup {
    store: PathBuf,
    folder: PathBuf,
    vms: Vec<String>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.folder);
        for vm in &self.vms {
            let _ = tart_in(&self.store).args(["delete", vm]).status();
        }
    }
}

fn image(port: u16, name: &str) -> Option<Value> {
    let response = common::http(port, "GET", "/images", None, None);
    assert_eq!(response.status, 200, "{}", response.body);
    response.body["images"].get(name).cloned()
}

/// AC-39, AC-40: a line added to the instance's checkout appears, its base's
/// promotion makes it acquirable, and its removal hides it, while the
/// instance and the installed daemon keep their processes (AC-18).
#[test]
#[ignore = "TL-4: needs real Tart, a second instance under LIVE_ROOT started with a writable PILOT_REPO, and LIVE_PILOT naming that checkout"]
fn ac39_ac40_live_line_promotion_and_removal() {
    let root = PathBuf::from(std::env::var("LIVE_ROOT").expect("LIVE_ROOT"));
    let manifest = Manifest::read(&root)
        .expect("read the manifest")
        .expect("a second instance under LIVE_ROOT");
    let port = manifest.port;
    let health = common::http(port, "GET", "/health", None, None);
    if let Err(refused) = second_instance::guard(port, &health.body) {
        panic!("{refused}");
    }
    let store = manifest.tart_home.clone();
    let pilot = PathBuf::from(std::env::var("LIVE_PILOT").expect("LIVE_PILOT"));
    let machines = common::http(port, "GET", "/machines", None, None);
    assert_eq!(machines.status, 200, "no machine-service on port {port}");
    assert_eq!(
        Path::new(machines.body["pilot_repo"].as_str().expect("pilot_repo")),
        pilot.as_path(),
        "the instance does not read LIVE_PILOT"
    );
    let installed = listener_pid(INSTALLED_PORT);
    let instance = listener_pid(port);

    let suffix = &uuid_suffix();
    let name = format!("{}ic-live-{suffix}", manifest.vm_prefix());
    let work = format!("{name}-work");
    let base = format!("{name}-base");
    let source = std::env::var("LIVE_CLONE_SOURCE").unwrap_or_else(|_| "pilot-ubuntu-base".into());
    let folder = pilot.join("images").join(&name);
    let _cleanup = Cleanup {
        store: store.clone(),
        folder: folder.clone(),
        vms: vec![work.clone(), base.clone()],
    };

    tart(&store, &["clone", &source, &work]);
    std::fs::create_dir_all(&folder).expect("line folder");
    std::fs::write(
        folder.join("line.conf"),
        format!(
            "LINE_KIND=linux\nBASE_VM={base}\nWORK_VM={work}\nCLONE_PREFIX={name}-\nGUEST_USER=admin\nGUEST_PASS=admin\n"
        ),
    )
    .expect("line.conf");

    let added = image(port, &name).expect("the new line is listed");
    assert_eq!(added["base_available"], false);
    assert_eq!(added["concurrency"]["acquirable"], false);

    tart(&store, &["rename", &work, &base]);
    let promoted = image(port, &name).expect("the line is still listed");
    assert_eq!(promoted["base_available"], true);
    assert_eq!(promoted["concurrency"]["acquirable"], true);

    std::fs::remove_dir_all(&folder).expect("remove line");
    assert!(image(port, &name).is_none(), "the removed line is listed");

    assert_eq!(listener_pid(port), instance, "the instance was restarted");
    assert_eq!(
        listener_pid(INSTALLED_PORT),
        installed,
        "the installed daemon's process changed"
    );
}

fn uuid_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos())
        .unwrap_or_default();
    format!("{:06x}", (nanos ^ std::process::id()) & 0xff_ffff)
}
