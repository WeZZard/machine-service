//! The test environment of one run (LR-6, DC-284): the run's branch build,
//! downloaded from the run's own artifact and checked against the run, and
//! started with the `second-instance` launcher on this host for the run's
//! duration only, then stopped and removed, after a crash too.
//!
//! Each run has one folder, `<state>/runs/<run>/`, which holds everything:
//! the unpacked build (`bundle/`), the launcher's root (`env/`, whose
//! manifest `stop` works from) and `run.json`. The folder is removed last,
//! and only once the launcher's `stop` succeeded, so a later round or a
//! restarted controller finishes what a failed stop or a crash left. A
//! restarted controller takes over nothing: it stops every test environment
//! it finds, as it releases every runner VM an earlier one left.

use std::io::Read as _;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::github::{Artifact, JobSource, Run};
use crate::production::Http;
use crate::runs;

/// The artifact the workflow's build job uploads for each run.
pub const ARTIFACT: &str = "lab-test-environment";
/// The artifact's record of what it holds.
pub const MANIFEST: &str = "test-environment.json";
/// The artifact's build: a gzipped tar of the installation's executables.
pub const ARCHIVE: &str = "test-environment.tar.gz";
/// The executables a build must hold.
pub const REQUIRED: [&str; 2] = ["machine-service", "second-instance"];
/// A test environment's HTTP port: the first of the lab ports the guest
/// firewall lets a runner VM reach (DC-267, DC-270).
pub const HTTP_PORT: u16 = guest_firewall::LAB_PORTS[0];
/// A test environment's broker port.
pub const BROKER_PORT: u16 = guest_firewall::LAB_PORTS[1];
/// The durable cells a durable Mac's test environment exposes its Mac for.
pub const DURABLE_CELLS: &str = "macos-client.durable-mac,ubuntu-client.durable-mac";
/// The longest a test environment lives, whatever its run does: two jobs'
/// deadlines, the durable cells' two jobs one after the other.
pub const LIFETIME: Duration = Duration::from_secs(2 * crate::decide::DEADLINE.as_secs());
/// How long a started test environment may take to answer as itself.
const HEALTH_WAIT: Duration = Duration::from_secs(60);

/// The SHA-256 of a file, in hex, read in pieces.
pub fn file_sha256(path: &Path) -> Result<String, String> {
    let mut file =
        std::fs::File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// Whether a downloaded artifact is run `run`'s build (LR-6): GitHub says it
/// is the run's and of the run's commit; its zip matches GitHub's digest
/// when GitHub reports one; its manifest names the same run and commit and
/// the archive by its SHA-256, which the archive has.
pub fn verify(
    run: &Run,
    artifact_run: u64,
    artifact_head: &str,
    digest: Option<&str>,
    zip_sha256: &str,
    manifest: &Value,
    archive_sha256: &str,
) -> Result<(), String> {
    let refuse = |why: String| Err(format!("run {}'s build is refused: {why}", run.id));
    if artifact_run != run.id {
        return refuse(format!("GitHub says the artifact is run {artifact_run}'s"));
    }
    if run.head_sha.is_empty() || artifact_head != run.head_sha {
        return refuse("GitHub says the artifact is of another commit".into());
    }
    if let Some(digest) = digest {
        if digest != format!("sha256:{zip_sha256}") {
            return refuse("the download does not match GitHub's digest".into());
        }
    }
    if manifest["run_id"].as_u64() != Some(run.id) {
        return refuse(format!(
            "its {MANIFEST} names run {}, not this one",
            manifest["run_id"]
        ));
    }
    if manifest["head_sha"].as_str() != Some(run.head_sha.as_str()) {
        return refuse(format!("its {MANIFEST} names another commit"));
    }
    if manifest["archive"].as_str() != Some(ARCHIVE) {
        return refuse(format!("its {MANIFEST} names no {ARCHIVE}"));
    }
    if manifest["sha256"].as_str() != Some(archive_sha256) {
        return refuse(format!(
            "{ARCHIVE} does not match the SHA-256 its {MANIFEST} records"
        ));
    }
    Ok(())
}

/// How a test environment is started (LR-6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// The launcher's root.
    pub root: PathBuf,
    /// Its second-instance ID ([`runs::environment_id`]).
    pub id: String,
    /// The host's LAN address, both listeners' one address.
    pub bind: Ipv4Addr,
    /// Its HTTP port; the broker's is the next.
    pub port: u16,
    /// The Tart store it reads its bases from.
    pub tart_home: PathBuf,
    /// The build's `machine-service`.
    pub executable: PathBuf,
    /// The image source.
    pub pilot_repo: Option<String>,
    /// Whether it exposes this durable Mac for the durable cells.
    pub durable: bool,
}

/// The arguments of `second-instance start` for a test environment.
pub fn start_args(settings: &Settings) -> Vec<String> {
    let bind = settings.bind.to_string();
    let broker = settings.port + 1;
    let mut args: Vec<String> = [
        "start",
        "--root",
        &settings.root.display().to_string(),
        "--id",
        &settings.id,
        "--port",
        &settings.port.to_string(),
        "--session-port",
        &broker.to_string(),
        "--bind",
        &bind,
        "--tart-home",
        &settings.tart_home.display().to_string(),
        "--executable",
        &settings.executable.display().to_string(),
        "--guest-link",
        "relay",
        "--env",
        &format!("MACHINE_SERVICE_RELAY_GUEST_URL=wss://{bind}:{broker}/relay"),
        "--env",
        "MACHINE_SERVICE_GRACE_HOURS=0",
    ]
    .iter()
    .map(|item| item.to_string())
    .collect();
    if let Some(repo) = &settings.pilot_repo {
        args.extend(["--env".to_string(), format!("PILOT_REPO={repo}")]);
    }
    if settings.durable {
        args.extend([
            "--durable".to_string(),
            "--durable-cell".to_string(),
            DURABLE_CELLS.to_string(),
        ]);
    }
    args
}

/// Where this host's test environments run.
#[derive(Debug, Clone)]
pub struct Place {
    /// The address both listeners bind: the host's LAN address.
    pub bind: Ipv4Addr,
    /// The HTTP port; the broker's is the next.
    pub port: u16,
    /// The Tart store; a host without one gets an empty store in the run's
    /// folder.
    pub tart_home: PathBuf,
    /// The image source, as production has it.
    pub pilot_repo: Option<String>,
    /// Whether this is a durable Mac.
    pub durable: bool,
    /// The controller's own launcher, for a run whose build has none left.
    pub fallback_launcher: PathBuf,
    /// A published relay agent release to use in place of the build's
    /// (DC-274): the one in the controller's bundle, when it has one.
    pub relay_agent_release: Option<PathBuf>,
}

/// The test environment that runs now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Current {
    /// Its run.
    pub run: u64,
    /// Its base URL.
    pub base: String,
    /// When it started, in seconds since the epoch.
    pub started: u64,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

/// This host's test environments: at most one at a time, since a test
/// environment holds the lab ports.
pub struct Environments {
    runs: PathBuf,
    place: Place,
    current: Mutex<Option<Current>>,
}

impl Environments {
    /// The test environments under `runs`, `<state>/runs`.
    pub fn new(runs: PathBuf, place: Place) -> Environments {
        Environments {
            runs,
            place,
            current: Mutex::new(None),
        }
    }

    /// The one that runs now.
    pub fn current(&self) -> Option<Current> {
        self.current.lock().expect("current").clone()
    }

    /// The address its listeners bind.
    pub fn address(&self) -> Ipv4Addr {
        self.place.bind
    }

    /// Its HTTP port.
    pub fn port(&self) -> u16 {
        self.place.port
    }

    fn base(&self) -> String {
        format!("http://{}:{}", self.place.bind, self.place.port)
    }

    fn dir(&self, run: u64) -> PathBuf {
        self.runs.join(run.to_string())
    }

    /// The macOS guests the current test environment runs, which production
    /// counts as another program's.
    pub fn running_macos(&self) -> u32 {
        self.current()
            .and_then(|current| {
                crate::production::parse_capacity(&crate::lab::get_json(
                    &format!("{}/images", current.base),
                    Duration::from_secs(10),
                )?)
                .ok()
            })
            .map(|capacity| capacity.running)
            .unwrap_or(0)
    }

    /// Run `run`'s test environment: the running one when it is `run`'s,
    /// else a new one from the run's verified build. Refused while another
    /// run's test environment runs here.
    pub fn ensure(&self, run: &Run, source: &dyn JobSource) -> Result<Current, String> {
        if let Some(current) = self.current() {
            if current.run == run.id {
                return Ok(current);
            }
            return Err(format!(
                "run {}'s test environment still runs on this host",
                current.run
            ));
        }
        let dir = self.dir(run.id);
        if dir.exists() {
            self.stop(run.id)?;
        }
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode_private()
            .create(&dir)
            .map_err(|error| format!("{}: {error}", dir.display()))?;
        crate::log(&format!("run {}: fetching its test environment", run.id));
        let started = self
            .prepare(run, source, &dir)
            .and_then(|bundle| self.start(run, &dir, &bundle));
        match started {
            Ok(current) => {
                *self.current.lock().expect("current") = Some(current.clone());
                crate::log(&format!("run {}: test environment started", run.id));
                Ok(current)
            }
            Err(error) => {
                if let Err(stop) = self.stop(run.id) {
                    crate::log(&format!("run {}: {stop}", run.id));
                }
                Err(error)
            }
        }
    }

    /// Download, verify and unpack the run's build; answers its folder.
    fn prepare(&self, run: &Run, source: &dyn JobSource, dir: &Path) -> Result<PathBuf, String> {
        let Artifact {
            bytes,
            digest,
            run_id,
            head_sha,
        } = source.artifact(run.id, ARTIFACT)?;
        let zip = dir.join("artifact.zip");
        std::fs::write(&zip, &bytes).map_err(|error| format!("{}: {error}", zip.display()))?;
        drop(bytes);
        let zip_sha = file_sha256(&zip)?;
        let unpacked = dir.join("artifact");
        make(&unpacked)?;
        tar(&["-xf"], &zip, &unpacked)?;
        let manifest_path = unpacked.join(MANIFEST);
        let manifest: Value = std::fs::read_to_string(&manifest_path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .ok_or_else(|| format!("run {}'s build has no readable {MANIFEST}", run.id))?;
        let archive = unpacked.join(ARCHIVE);
        if !archive.is_file() {
            return Err(format!("run {}'s build has no {ARCHIVE}", run.id));
        }
        let archive_sha = file_sha256(&archive)?;
        verify(
            run,
            run_id,
            &head_sha,
            digest.as_deref(),
            &zip_sha,
            &manifest,
            &archive_sha,
        )?;
        let bundle = dir.join("bundle");
        make(&bundle)?;
        tar(&["-xzf"], &archive, &bundle)?;
        let _ = std::fs::remove_file(&zip);
        let _ = std::fs::remove_dir_all(&unpacked);
        for name in REQUIRED {
            if !bundle.join(name).is_file() {
                return Err(format!("run {}'s build has no {name}", run.id));
            }
        }
        if let Some(release) = &self.place.relay_agent_release {
            replace_release(&bundle, release)?;
        }
        Ok(bundle)
    }

    fn start(&self, run: &Run, dir: &Path, bundle: &Path) -> Result<Current, String> {
        let id = runs::environment_id(run.id);
        let tart_home = if self.place.tart_home.is_dir() {
            self.place.tart_home.clone()
        } else {
            let empty = dir.join("tart");
            make(&empty)?;
            empty
        };
        let settings = Settings {
            root: dir.join("env"),
            id: id.clone(),
            bind: self.place.bind,
            port: self.place.port,
            tart_home,
            executable: bundle.join("machine-service"),
            pilot_repo: self.place.pilot_repo.clone(),
            durable: self.place.durable,
        };
        launch(&bundle.join("second-instance"), &start_args(&settings))?;
        let base = self.base();
        let deadline = std::time::Instant::now() + HEALTH_WAIT;
        loop {
            let health = Http::new(&base).health();
            if health.as_ref().is_some_and(|health| is_run(health, run.id)) {
                break;
            }
            if std::time::Instant::now() >= deadline {
                return Err(format!(
                    "run {}'s test environment did not answer as itself",
                    run.id
                ));
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        let current = Current {
            run: run.id,
            base,
            started: now(),
        };
        let record = json!({"run": current.run, "started": current.started});
        std::fs::write(dir.join("run.json"), record.to_string())
            .map_err(|error| format!("{}: {error}", dir.display()))?;
        Ok(current)
    }

    /// Stop run `run`'s test environment and remove its folder: the
    /// launcher's `stop` releases its leases, deletes its VMs and removes
    /// its root, working from its manifest alone. The folder stays when the
    /// stop fails, for the next try.
    pub fn stop(&self, run: u64) -> Result<(), String> {
        let dir = self.dir(run);
        let root = dir.join("env");
        // Any manifest, even one that does not read, goes to the launcher:
        // the folder is removed only after a stop that succeeded.
        if second_instance::Manifest::file(&root).exists() {
            let own = dir.join("bundle/second-instance");
            let launcher = if own.is_file() {
                own
            } else {
                self.place.fallback_launcher.clone()
            };
            launch(
                &launcher,
                &["stop".into(), "--root".into(), root.display().to_string()],
            )
            .map_err(|error| format!("run {run}'s test environment was not stopped: {error}"))?;
        }
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("{}: {error}", dir.display())),
        }
        let mut current = self.current.lock().expect("current");
        if current.as_ref().is_some_and(|current| current.run == run) {
            *current = None;
        }
        crate::log(&format!("run {run}: test environment stopped and removed"));
        Ok(())
    }

    /// The runs whose folders are here.
    fn folders(&self) -> Vec<u64> {
        let mut found: Vec<u64> = std::fs::read_dir(&self.runs)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
            .collect();
        found.sort();
        found
    }

    fn started(&self, run: u64) -> Option<u64> {
        let text = std::fs::read_to_string(self.dir(run).join("run.json")).ok()?;
        serde_json::from_str::<Value>(&text).ok()?["started"].as_u64()
    }

    /// Stop every test environment whose run `keep` no longer wants, or that
    /// outlived [`LIFETIME`], and every folder an earlier run left.
    pub fn retire(&self, keep: &dyn Fn(u64) -> bool) {
        let current = self.current().map(|current| current.run);
        for run in self.folders() {
            let old = self
                .started(run)
                .is_some_and(|started| now().saturating_sub(started) >= LIFETIME.as_secs());
            if Some(run) == current && keep(run) && !old {
                continue;
            }
            if old {
                crate::log(&format!(
                    "run {run}: its test environment outlived its lifetime"
                ));
            }
            if let Err(error) = self.stop(run) {
                crate::log(&error);
            }
        }
    }
}

/// Whether a `/health` answer is run `run`'s test environment's.
pub fn is_run(health: &Value, run: u64) -> bool {
    health["second_instance"]["id"] == runs::environment_id(run).as_str()
}

trait Private {
    fn mode_private(&mut self) -> &mut Self;
}

impl Private for std::fs::DirBuilder {
    fn mode_private(&mut self) -> &mut Self {
        std::os::unix::fs::DirBuilderExt::mode(self, 0o700)
    }
}

fn make(path: &Path) -> Result<(), String> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode_private()
        .create(path)
        .map_err(|error| format!("{}: {error}", path.display()))
}

/// Run `tar` from the tool table on `archive` into `into`. macOS's `tar`
/// reads a zip as well as a gzipped tar, and refuses a member with `..` or
/// an absolute path unless asked not to.
fn tar(flags: &[&str], archive: &Path, into: &Path) -> Result<(), String> {
    let output = service_layout::tools::command(service_layout::tools::Tool::Tar)?
        .args(flags)
        .arg(archive)
        .arg("-C")
        .arg(into)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("tar: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "tar {} {}: {}",
            flags.join(" "),
            archive.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

/// Run a launcher with `args`; its output goes to the log only on failure.
pub fn launch(launcher: &Path, args: &[String]) -> Result<(), String> {
    let output = service_layout::tools::command_for(&launcher.display().to_string())?
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("{}: {error}", launcher.display()))?;
    if !output.status.success() {
        return Err(format!(
            "second-instance {} failed: {}",
            args.first().map(String::as_str).unwrap_or(""),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

/// Put the published relay agent release `release` into a build in place of
/// its own, checked against its `SHA256SUMS` before and after the copy
/// (DC-274).
pub fn replace_release(bundle: &Path, release: &Path) -> Result<(), String> {
    use service_layout::release::{Release, FOLDER, SUMS};
    let source = Release::open(release)?;
    for archive in &source.archives {
        source.verified(&archive.platform)?;
    }
    let target = bundle.join(FOLDER);
    match std::fs::remove_dir_all(&target) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("{}: {error}", target.display())),
    }
    make(&target)?;
    let mut files = vec![SUMS.to_string()];
    files.extend(source.archives.iter().map(|archive| archive.file.clone()));
    for file in &files {
        std::fs::copy(release.join(file), target.join(file))
            .map_err(|error| format!("{file}: {error}"))?;
    }
    let copied = Release::open(&target)?;
    for archive in &copied.archives {
        copied.verified(&archive.platform)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run() -> Run {
        Run {
            id: 42,
            head_sha: "c0ffee".into(),
            ..Run::default()
        }
    }

    fn manifest() -> Value {
        json!({"run_id": 42, "head_sha": "c0ffee", "archive": ARCHIVE, "sha256": "aa"})
    }

    #[test]
    fn only_the_runs_own_build_is_accepted() {
        let ok = verify(
            &run(),
            42,
            "c0ffee",
            Some("sha256:zz"),
            "zz",
            &manifest(),
            "aa",
        );
        assert!(ok.is_ok(), "{ok:?}");
        // GitHub reports no digest: the manifest's checks remain.
        assert!(verify(&run(), 42, "c0ffee", None, "zz", &manifest(), "aa").is_ok());
        let cases: Vec<(&str, Result<(), String>)> = vec![
            (
                "another run's artifact",
                verify(&run(), 41, "c0ffee", None, "zz", &manifest(), "aa"),
            ),
            (
                "another commit's artifact",
                verify(&run(), 42, "beef", None, "zz", &manifest(), "aa"),
            ),
            (
                "a changed download",
                verify(
                    &run(),
                    42,
                    "c0ffee",
                    Some("sha256:yy"),
                    "zz",
                    &manifest(),
                    "aa",
                ),
            ),
            (
                "a manifest of another run",
                verify(
                    &run(),
                    42,
                    "c0ffee",
                    None,
                    "zz",
                    &json!({"run_id": 41, "head_sha": "c0ffee", "archive": ARCHIVE, "sha256": "aa"}),
                    "aa",
                ),
            ),
            (
                "a manifest of another commit",
                verify(
                    &run(),
                    42,
                    "c0ffee",
                    None,
                    "zz",
                    &json!({"run_id": 42, "head_sha": "beef", "archive": ARCHIVE, "sha256": "aa"}),
                    "aa",
                ),
            ),
            (
                "a changed archive",
                verify(&run(), 42, "c0ffee", None, "zz", &manifest(), "bb"),
            ),
            (
                "no manifest",
                verify(&run(), 42, "c0ffee", None, "zz", &Value::Null, "aa"),
            ),
            (
                "a run without a commit",
                verify(
                    &Run {
                        id: 42,
                        ..Run::default()
                    },
                    42,
                    "",
                    None,
                    "zz",
                    &manifest(),
                    "aa",
                ),
            ),
        ];
        for (what, result) in cases {
            let error = result.expect_err(what);
            assert!(error.contains("refused"), "{what}: {error}");
        }
    }

    fn settings(durable: bool) -> Settings {
        Settings {
            root: PathBuf::from("/h/state/runs/42/env"),
            id: "r42".into(),
            bind: "192.0.2.3".parse().unwrap(),
            port: HTTP_PORT,
            tart_home: PathBuf::from("/h/.tart"),
            executable: PathBuf::from("/h/state/runs/42/bundle/machine-service"),
            pilot_repo: Some("/p".into()),
            durable,
        }
    }

    #[test]
    fn a_test_environment_listens_on_the_lan_address_with_the_lab_ports() {
        let joined = start_args(&settings(false)).join(" ");
        assert!(joined.starts_with("start --root /h/state/runs/42/env --id r42 --port 6272 --session-port 6273 --bind 192.0.2.3 "), "{joined}");
        assert!(joined.contains("--executable /h/state/runs/42/bundle/machine-service"));
        assert!(joined.contains("--guest-link relay"));
        assert!(joined.contains("--env MACHINE_SERVICE_RELAY_GUEST_URL=wss://192.0.2.3:6273/relay"));
        assert!(joined.contains("--env PILOT_REPO=/p"));
        assert!(!joined.contains("--durable"));
        let durable = start_args(&settings(true)).join(" ");
        assert!(durable.ends_with(
            "--durable --durable-cell macos-client.durable-mac,ubuntu-client.durable-mac"
        ));
    }

    #[test]
    fn a_health_answer_is_matched_to_its_run() {
        assert!(is_run(&json!({"second_instance": {"id": "r42"}}), 42));
        assert!(!is_run(&json!({"second_instance": {"id": "r43"}}), 42));
        assert!(!is_run(&json!({"second_instance": {"id": "lab"}}), 42));
        assert!(!is_run(&json!({}), 42));
    }

    #[test]
    fn the_lifetime_covers_two_jobs() {
        assert!(LIFETIME >= 2 * crate::decide::DEADLINE);
    }
}
