//! The cell runner: one cell of TL-7 under one profile.
//!
//! A cell is a client platform and a target kind. For one cell and one
//! profile the runner starts the target's broker side, puts the rig in front
//! of it, plays the profile's schedule, runs each scenario that applies to the
//! profile with the client side the rig hands out, and writes the results and
//! the run record into `<evidence>/<cell>/<profile>/`.
//!
//! Until the broker and the relay agent exist (M3), every target kind is
//! driven through the [`StandIn`] echo server; the target trait is where the
//! real targets plug in.

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

    fn id(self) -> &'static str {
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
    fn id(self) -> &'static str {
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
}

/// Where a target's broker side listens.
#[derive(Debug, Clone)]
pub struct Endpoint {
    /// The broker's address, which only the rig is told.
    pub addr: std::net::SocketAddr,
    /// The authority of the broker's certificate, PEM.
    pub ca_pem: String,
}

/// A target of a cell.
pub trait Target {
    /// Start the target's broker side for `broker_name`.
    fn start(&mut self, cell: Cell, broker_name: &str) -> Result<Endpoint, RigError>;
}

/// The stand-in target: an echo server in place of every target kind.
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
    fn start(&mut self, _cell: Cell, broker_name: &str) -> Result<Endpoint, RigError> {
        let server = EchoServer::start(broker_name)?;
        let endpoint = Endpoint {
            addr: server.addr(),
            ca_pem: server.ca_pem().into(),
        };
        self.server = Some(server);
        Ok(endpoint)
    }
}

/// What a scenario runs with.
pub struct CellContext<'a> {
    /// The cell.
    pub cell: Cell,
    /// The rig, for its control and its schedule.
    pub rig: &'a mut Rig,
    /// What the client is told.
    pub client: ClientSide,
    /// The played schedule's notes.
    pub notes: &'a Receiver<Note>,
}

/// One scenario of the suite.
#[derive(Debug, Clone, Copy)]
pub struct Scenario {
    /// Its acceptance ID, such as `AC-55`.
    pub id: &'static str,
    /// The profiles it runs under, from its tags.
    pub profiles: &'static [&'static str],
    /// The scenario; an error is its failure.
    pub run: fn(&mut CellContext<'_>) -> Result<(), String>,
}

/// A scenario's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "result", content = "detail", rename_all = "snake_case")]
pub enum Verdict {
    /// It passed.
    Passed,
    /// It failed, and why.
    Failed(String),
    /// It did not run, and why.
    Skipped(String),
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
pub fn run_cell(
    cell: Cell,
    profile: &Profile,
    seed: u64,
    target: &mut dyn Target,
    scenarios: &[Scenario],
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
            run_dir: run_dir.clone(),
        }
    } else {
        let endpoint = target.start(cell, crate::rig::DEFAULT_BROKER_NAME)?;
        let mut config = RigConfig::new(profile.clone(), endpoint.addr, &endpoint.ca_pem, &run_dir);
        config.seed = seed;
        let mut rig = Rig::start(config)?;
        let notes = rig.play();
        let client = rig.client_side().clone();
        let mut context = CellContext {
            cell,
            rig: &mut rig,
            client,
            notes: &notes,
        };
        let results = scenarios
            .iter()
            .map(|scenario| run_scenario(scenario, &profile.id, &mut context))
            .collect();
        CellReport {
            cell: cell.id(),
            profile: profile.id.clone(),
            seed,
            schedule: rig.schedule().clone(),
            results,
            run_dir: run_dir.clone(),
        }
    };
    let text = format!("{:#}\n", json!(report));
    std::fs::write(run_dir.join("report.json"), text).map_err(RigError::io("write the report"))?;
    Ok(report)
}

fn run_scenario(
    scenario: &Scenario,
    profile: &str,
    context: &mut CellContext<'_>,
) -> ScenarioResult {
    let start = Instant::now();
    let verdict = if !scenario
        .profiles
        .iter()
        .any(|p| p.eq_ignore_ascii_case(profile))
    {
        Verdict::Skipped(format!("{} does not run under {profile}", scenario.id))
    } else {
        let run = scenario.run;
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(context))) {
            Ok(Ok(())) => Verdict::Passed,
            Ok(Err(why)) => Verdict::Failed(why),
            Err(panic) => Verdict::Failed(
                panic
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "the scenario panicked".into()),
            ),
        }
    };
    ScenarioResult {
        id: scenario.id.into(),
        verdict,
        elapsed_ms: start.elapsed().as_millis() as u64,
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
    }

    struct NeverStarted;
    impl Target for NeverStarted {
        fn start(&mut self, _: Cell, _: &str) -> Result<Endpoint, RigError> {
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
        let scenarios = [Scenario {
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
}
