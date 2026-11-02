//! The cell runner: one cell of TL-7 under one profile.
//!
//! A cell is a client platform and a target kind. For one cell and one
//! profile the runner starts the target's broker side, puts the rig in front
//! of it, plays the profile's schedule, runs each scenario that applies to the
//! profile with the client side the rig hands out, and writes the results and
//! the run record into `<evidence>/<cell>/<profile>/`.
//!
//! The real targets, a machine-service instance with the relay link and a
//! lease on it, are the `relay-acceptance` crate's; the rig's own self-tests
//! drive the [`StandIn`] echo server.

use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::Instant;

use serde::Serialize;
use serde_json::json;

use crate::profile::Profile;
use crate::rig::{ClientSide, Note, Rig, RigConfig};
use crate::schedule::Schedule;
use crate::standin::EchoServer;
use crate::RigError;

/// Where the client runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClientPlatform {
    /// A Mac.
    Macos,
    /// Ubuntu.
    Ubuntu,
}

impl ClientPlatform {
    /// The platform of this machine, when it can be a cell's client.
    pub fn this_machine() -> Option<ClientPlatform> {
        match std::env::consts::OS {
            "macos" => Some(ClientPlatform::Macos),
            "linux" => Some(ClientPlatform::Ubuntu),
            _ => None,
        }
    }

    /// The platform's name in a cell's name.
    pub fn id(self) -> &'static str {
        match self {
            ClientPlatform::Macos => "macos",
            ClientPlatform::Ubuntu => "ubuntu",
        }
    }
}

/// What the client acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TargetKind {
    /// A macOS virtual machine.
    MacosVm,
    /// A durable Mac.
    DurableMac,
    /// An Ubuntu virtual machine.
    UbuntuVm,
}

impl TargetKind {
    /// The kind's name in a cell's name.
    pub fn id(self) -> &'static str {
        match self {
            TargetKind::MacosVm => "macos-vm",
            TargetKind::DurableMac => "durable-mac",
            TargetKind::UbuntuVm => "ubuntu-vm",
        }
    }
}

/// One cell of TL-7.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Cell {
    /// Where the client runs.
    pub client: ClientPlatform,
    /// What it acts on.
    pub target: TargetKind,
}

impl Cell {
    /// The 6 cells.
    pub fn all() -> Vec<Cell> {
        [ClientPlatform::Macos, ClientPlatform::Ubuntu]
            .into_iter()
            .flat_map(|client| {
                [
                    TargetKind::MacosVm,
                    TargetKind::DurableMac,
                    TargetKind::UbuntuVm,
                ]
                .into_iter()
                .map(move |target| Cell { client, target })
            })
            .collect()
    }

    /// The cell's name, `<client>-client.<target>`.
    pub fn id(&self) -> String {
        format!("{}-client.{}", self.client.id(), self.target.id())
    }

    /// The cell named `id`, as [`Cell::id`] names it.
    pub fn parse(id: &str) -> Option<Cell> {
        Cell::all().into_iter().find(|cell| cell.id() == id)
    }
}

/// Where a target's broker side listens.
#[derive(Debug, Clone)]
pub struct Endpoint {
    /// The broker's host name, the one name the edge serves: for a real
    /// target, the host of the instance's advertised `relay_url`.
    pub broker_name: String,
    /// The broker's address, which only the rig is told.
    pub addr: std::net::SocketAddr,
    /// The authority of the broker's certificate, PEM.
    pub ca_pem: String,
}

/// A target of a cell: what the scenarios act on.
///
/// [`Target::start`] finds the broker side before the rig starts; the
/// scenarios then act on the target through [`CellContext::target`], and
/// [`Target::finish`] runs after the last scenario whatever happened, to
/// release what the target holds.
pub trait Target {
    /// Start or find the target's broker side.
    fn start(&mut self, cell: Cell, run_dir: &Path) -> Result<Endpoint, RigError>;

    /// Release what the target holds; the returned value joins the report
    /// as its `target` evidence.
    fn finish(&mut self) -> serde_json::Value {
        serde_json::Value::Null
    }
}

/// The stand-in target: an echo server in place of every target kind, for
/// the rig's own self-tests.
#[derive(Debug, Default)]
pub struct StandIn {
    server: Option<EchoServer>,
}

impl StandIn {
    /// The running echo server, after [`Target::start`].
    pub fn server(&self) -> Option<&EchoServer> {
        self.server.as_ref()
    }
}

impl Target for StandIn {
    fn start(&mut self, _cell: Cell, _run_dir: &Path) -> Result<Endpoint, RigError> {
        let server = EchoServer::start(crate::rig::DEFAULT_BROKER_NAME)?;
        let endpoint = Endpoint {
            broker_name: crate::rig::DEFAULT_BROKER_NAME.into(),
            addr: server.addr(),
            ca_pem: server.ca_pem().into(),
        };
        self.server = Some(server);
        Ok(endpoint)
    }
}

/// Why a scenario or one of its checks did not pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// It failed, and why.
    Failed(String),
    /// It could not be proved here, because something it needs is not built
    /// or not in the image yet; the reason names what and where.
    Blocked(String),
}

impl From<String> for Stop {
    fn from(why: String) -> Stop {
        Stop::Failed(why)
    }
}

impl From<&str> for Stop {
    fn from(why: &str) -> Stop {
        Stop::Failed(why.to_string())
    }
}

/// What a scenario runs with.
pub struct CellContext<'a, T> {
    /// The cell.
    pub cell: Cell,
    /// The profile's ID.
    pub profile: String,
    /// The rig, for its control and its schedule.
    pub rig: &'a mut Rig,
    /// What the client is told.
    pub client: ClientSide,
    /// The played schedule's notes.
    pub notes: &'a Receiver<Note>,
    /// The target.
    pub target: &'a mut T,
    /// The run's directory, for a scenario's own evidence files.
    pub run_dir: PathBuf,
    checks: Vec<Check>,
    evidence: serde_json::Map<String, serde_json::Value>,
}

impl<T> CellContext<'_, T> {
    /// Record a named sub-check of the running scenario. A failed or blocked
    /// check makes the scenario fail or be blocked even when it returns `Ok`.
    pub fn check(&mut self, name: &str, result: Result<(), Stop>) {
        let verdict = match result {
            Ok(()) => Verdict::Passed,
            Err(Stop::Failed(why)) => Verdict::Failed(why),
            Err(Stop::Blocked(why)) => Verdict::Blocked(why),
        };
        self.checks.push(Check {
            name: name.into(),
            verdict,
        });
    }

    /// Record a value in the running scenario's evidence.
    pub fn record(&mut self, key: &str, value: serde_json::Value) {
        self.evidence.insert(key.into(), value);
    }
}

/// A scenario's result: `Ok` passes, unless one of its checks did not.
pub type Outcome = Result<(), Stop>;

/// One scenario of the suite.
pub struct Scenario<T> {
    /// Its acceptance ID, such as `AC-55`.
    pub id: &'static str,
    /// The profiles it runs under, from its tags.
    pub profiles: &'static [&'static str],
    /// The scenario.
    pub run: fn(&mut CellContext<'_, T>) -> Outcome,
}

impl<T> Clone for Scenario<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Scenario<T> {}

impl<T> std::fmt::Debug for Scenario<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scenario")
            .field("id", &self.id)
            .field("profiles", &self.profiles)
            .finish_non_exhaustive()
    }
}

/// A scenario's or a check's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "result", content = "detail", rename_all = "snake_case")]
pub enum Verdict {
    /// It passed.
    Passed,
    /// It failed, and why.
    Failed(String),
    /// It did not run, and why.
    Skipped(String),
    /// It could not be proved yet, and what it waits for.
    Blocked(String),
}

/// One named sub-check of a scenario.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Check {
    /// What it checks.
    pub name: String,
    /// What happened.
    pub verdict: Verdict,
}

/// One scenario's entry in a cell's report.
#[derive(Debug, Clone, Serialize)]
pub struct ScenarioResult {
    /// The scenario.
    pub id: String,
    /// What happened.
    pub verdict: Verdict,
    /// How long it ran.
    pub elapsed_ms: u64,
    /// Its sub-checks.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<Check>,
    /// What it recorded.
    #[serde(skip_serializing_if = "serde_json::Map::is_empty")]
    pub evidence: serde_json::Map<String, serde_json::Value>,
}

/// One cell's run under one profile.
#[derive(Debug, Clone, Serialize)]
pub struct CellReport {
    /// The cell.
    pub cell: String,
    /// The profile.
    pub profile: String,
    /// The seed, to replay the run.
    pub seed: u64,
    /// The faults that were scheduled.
    pub schedule: Schedule,
    /// The scenarios' results.
    pub results: Vec<ScenarioResult>,
    /// What the target recorded when it finished.
    pub target: serde_json::Value,
    /// Where the run's files are.
    pub run_dir: PathBuf,
}

impl CellReport {
    /// Whether no scenario failed.
    pub fn passed(&self) -> bool {
        !self
            .results
            .iter()
            .any(|result| matches!(result.verdict, Verdict::Failed(_)))
    }
}

/// Run `scenarios` in `cell` under `profile`, and write the report.
pub fn run_cell<T: Target>(
    cell: Cell,
    profile: &Profile,
    seed: u64,
    target: &mut T,
    scenarios: &[Scenario<T>],
    evidence: &Path,
) -> Result<CellReport, RigError> {
    let run_dir = evidence.join(cell.id()).join(&profile.id);
    std::fs::create_dir_all(&run_dir)
        .map_err(RigError::io(format!("create {}", run_dir.display())))?;
    let skip_all = |why: String| {
        scenarios
            .iter()
            .map(|scenario| ScenarioResult {
                id: scenario.id.into(),
                verdict: Verdict::Skipped(why.clone()),
                elapsed_ms: 0,
                checks: Vec::new(),
                evidence: Default::default(),
            })
            .collect::<Vec<_>>()
    };

    let here = ClientPlatform::this_machine();
    let report = if here != Some(cell.client) {
        let here = here.map(ClientPlatform::id).unwrap_or(std::env::consts::OS);
        CellReport {
            cell: cell.id(),
            profile: profile.id.clone(),
            seed,
            schedule: Schedule::empty(seed, 0),
            results: skip_all(format!(
                "the cell's client is {}, this machine is {here}",
                cell.client.id()
            )),
            target: serde_json::Value::Null,
            run_dir: run_dir.clone(),
        }
    } else {
        let started = target.start(cell, &run_dir).and_then(|endpoint| {
            let mut config =
                RigConfig::new(profile.clone(), endpoint.addr, &endpoint.ca_pem, &run_dir);
            config.broker_name = endpoint.broker_name.clone();
            config.seed = seed;
            Rig::start(config)
        });
        let mut rig = match started {
            Ok(rig) => rig,
            Err(error) => {
                let finished = target.finish();
                let report = CellReport {
                    cell: cell.id(),
                    profile: profile.id.clone(),
                    seed,
                    schedule: Schedule::empty(seed, 0),
                    results: skip_all(format!("the cell did not start: {error}")),
                    target: finished,
                    run_dir: run_dir.clone(),
                };
                write_report(&report)?;
                return Err(error);
            }
        };
        let notes = rig.play();
        let client = rig.client_side().clone();
        let mut context = CellContext {
            cell,
            profile: profile.id.clone(),
            rig: &mut rig,
            client,
            notes: &notes,
            target,
            run_dir: run_dir.clone(),
            checks: Vec::new(),
            evidence: Default::default(),
        };
        let mut results = Vec::new();
        for scenario in scenarios {
            let result = run_scenario(scenario, &profile.id, &mut context);
            eprintln!(
                "{} {} {}: {:?} in {} s",
                cell.id(),
                profile.id,
                result.id,
                result.verdict,
                result.elapsed_ms / 1000
            );
            results.push(result);
            // A partial report after every scenario, so a run that is
            // stopped keeps what it proved.
            let partial = CellReport {
                cell: cell.id(),
                profile: profile.id.clone(),
                seed,
                schedule: context.rig.schedule().clone(),
                results: results.clone(),
                target: serde_json::Value::Null,
                run_dir: run_dir.clone(),
            };
            write_report(&partial)?;
        }
        let finished = context.target.finish();
        CellReport {
            cell: cell.id(),
            profile: profile.id.clone(),
            seed,
            schedule: rig.schedule().clone(),
            results,
            target: finished,
            run_dir: run_dir.clone(),
        }
    };
    write_report(&report)?;
    Ok(report)
}

fn write_report(report: &CellReport) -> Result<(), RigError> {
    let text = format!("{:#}\n", json!(report));
    std::fs::write(report.run_dir.join("report.json"), text)
        .map_err(RigError::io("write the report"))
}

fn run_scenario<T>(
    scenario: &Scenario<T>,
    profile: &str,
    context: &mut CellContext<'_, T>,
) -> ScenarioResult {
    let start = Instant::now();
    context.checks.clear();
    context.evidence.clear();
    let verdict = if !scenario
        .profiles
        .iter()
        .any(|p| p.eq_ignore_ascii_case(profile))
    {
        Verdict::Skipped(format!("{} does not run under {profile}", scenario.id))
    } else {
        let run = scenario.run;
        let returned = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(context)))
        {
            Ok(Ok(())) => Verdict::Passed,
            Ok(Err(Stop::Failed(why))) => Verdict::Failed(why),
            Ok(Err(Stop::Blocked(why))) => Verdict::Blocked(why),
            Err(panic) => Verdict::Failed(
                panic
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "the scenario panicked".into()),
            ),
        };
        fold_checks(returned, &context.checks)
    };
    ScenarioResult {
        id: scenario.id.into(),
        verdict,
        elapsed_ms: start.elapsed().as_millis() as u64,
        checks: std::mem::take(&mut context.checks),
        evidence: std::mem::take(&mut context.evidence),
    }
}

/// A scenario's verdict with its checks': a failed check fails it, and a
/// blocked check blocks a scenario that otherwise passed.
fn fold_checks(returned: Verdict, checks: &[Check]) -> Verdict {
    let named = |pick: fn(&Verdict) -> Option<&String>| {
        checks
            .iter()
            .filter_map(|check| pick(&check.verdict).map(|why| format!("{}: {why}", check.name)))
            .collect::<Vec<_>>()
    };
    let failed = named(|v| match v {
        Verdict::Failed(why) => Some(why),
        _ => None,
    });
    let blocked = named(|v| match v {
        Verdict::Blocked(why) => Some(why),
        _ => None,
    });
    match returned {
        Verdict::Passed if !failed.is_empty() => Verdict::Failed(failed.join("; ")),
        Verdict::Passed if !blocked.is_empty() => Verdict::Blocked(blocked.join("; ")),
        Verdict::Failed(why) if !failed.is_empty() => {
            Verdict::Failed(format!("{why}; {}", failed.join("; ")))
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn there_are_six_cells_with_distinct_names() {
        let cells = Cell::all();
        assert_eq!(cells.len(), 6);
        let mut ids: Vec<String> = cells.iter().map(Cell::id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 6);
        assert!(ids.contains(&"ubuntu-client.durable-mac".to_string()));
        for cell in Cell::all() {
            assert_eq!(Cell::parse(&cell.id()), Some(cell));
        }
        assert_eq!(Cell::parse("macos-client.windows-vm"), None);
    }

    struct NeverStarted;
    impl Target for NeverStarted {
        fn start(&mut self, _: Cell, _: &Path) -> Result<Endpoint, RigError> {
            panic!("a cell for another client platform must not start its target")
        }
    }

    #[test]
    fn a_cell_for_another_client_platform_is_skipped_and_reported() {
        let other = match ClientPlatform::this_machine() {
            Some(ClientPlatform::Macos) => ClientPlatform::Ubuntu,
            _ => ClientPlatform::Macos,
        };
        let cell = Cell {
            client: other,
            target: TargetKind::UbuntuVm,
        };
        let evidence = tempfile::tempdir().unwrap();
        let scenarios = [Scenario::<NeverStarted> {
            id: "AC-0",
            profiles: &["N0"],
            run: |_| Ok(()),
        }];
        let profile = Profile::builtin("N0").unwrap();
        let report = run_cell(
            cell,
            &profile,
            5,
            &mut NeverStarted,
            &scenarios,
            evidence.path(),
        )
        .unwrap();
        assert!(report.passed());
        assert!(matches!(report.results[0].verdict, Verdict::Skipped(_)));
        let written = std::fs::read_to_string(report.run_dir.join("report.json")).unwrap();
        let written: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert_eq!(written["seed"], 5);
        assert_eq!(written["results"][0]["verdict"]["result"], "skipped");
    }

    #[test]
    fn a_failed_check_fails_and_a_blocked_check_blocks_a_passing_scenario() {
        let check = |name: &str, verdict: Verdict| Check {
            name: name.into(),
            verdict,
        };
        let blocked = [check("keyring", Verdict::Blocked("the base".into()))];
        assert_eq!(
            fold_checks(Verdict::Passed, &blocked),
            Verdict::Blocked("keyring: the base".into())
        );
        let mixed = [
            check("display", Verdict::Failed("no DISPLAY".into())),
            blocked[0].clone(),
        ];
        assert_eq!(
            fold_checks(Verdict::Passed, &mixed),
            Verdict::Failed("display: no DISPLAY".into())
        );
        assert_eq!(fold_checks(Verdict::Passed, &[]), Verdict::Passed);
        assert_eq!(
            fold_checks(Verdict::Blocked("x".into()), &[]),
            Verdict::Blocked("x".into())
        );
    }
}
