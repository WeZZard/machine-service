//! The acceptance matrix (TL-7, DC-341): which runs make a full acceptance,
//! which machines this network offers for them right now, and what the runs
//! that ran add up to.
//!
//! The matrix (`matrix.json`) names each cell only by what it needs: the
//! client's system and the target, an image or a durable Mac. It names no
//! host and no address (DC-32, DC-338). [`discover`] reads what `machctl list
//! --json` reports, [`plan`] gives each run a machine or marks it missing,
//! and [`summary`] reports each run, each missing one, whether the
//! acceptance is complete, and the durable Macs' checklist of approved
//! permission prompts (DC-340).

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// The matrix in this repository.
pub const MATRIX: &str = include_str!("../matrix.json");

/// One cell of the matrix.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct CellSpec {
    /// The cell's ID, as `relay-acceptance run --cell` takes it.
    pub cell: String,
    /// The client's system: `macos` or `ubuntu`.
    pub client: String,
    /// What the cell targets.
    pub target: TargetSpec,
}

/// A cell's target: a VM of an image, or a durable Mac.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct TargetSpec {
    /// The image of a VM target.
    #[serde(default)]
    pub image: Option<String>,
    /// Whether the target is a durable Mac.
    #[serde(default)]
    pub durable: bool,
}

/// The matrix: its cells and the profiles each runs under.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Matrix {
    /// The profiles.
    pub profiles: Vec<String>,
    /// The cells.
    pub cells: Vec<CellSpec>,
}

impl Matrix {
    /// The matrix in this repository.
    pub fn builtin() -> Matrix {
        serde_json::from_str(MATRIX).expect("matrix.json parses")
    }
}

/// An instance discovery found, and what it offers now.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Found {
    /// The instance's name.
    pub instance: String,
    /// Its address.
    pub address: String,
    /// Whether it is this host's own instance.
    pub local: bool,
    /// The images it offers with a base built and room to acquire.
    pub images: Vec<String>,
    /// Its durable Mac, when it exposes one that is not leased.
    pub durable: Option<String>,
}

/// The instances `machctl list --json` reports, leaving out unreachable ones
/// and any the operator excludes by name (a run-time setting, never the
/// repository's).
pub fn discover(list: &Value, exclude: &[String]) -> Vec<Found> {
    list.as_array()
        .into_iter()
        .flatten()
        .filter(|i| i.get("reachable").and_then(Value::as_bool).unwrap_or(false))
        .filter_map(|i| {
            let instance = i.get("instance")?.as_str()?.to_string();
            if exclude.iter().any(|name| name == &instance) {
                return None;
            }
            let machines = i
                .get("machines")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let images = machines
                .iter()
                .filter(|m| m.get("kind").and_then(Value::as_str) == Some("virtual"))
                .filter(|m| {
                    m.get("base_available")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                })
                .filter(|m| {
                    m.pointer("/concurrency/acquirable")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                })
                .filter_map(|m| m.get("image").and_then(Value::as_str).map(str::to_string))
                .collect();
            let durable = machines
                .iter()
                .find(|m| {
                    m.get("kind").and_then(Value::as_str) == Some("durable")
                        && m.get("state").and_then(Value::as_str) == Some("available")
                })
                .and_then(|m| m.get("name").and_then(Value::as_str).map(str::to_string));
            Some(Found {
                instance,
                address: i
                    .get("address")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                local: i.get("local").and_then(Value::as_bool).unwrap_or(false),
                images,
                durable,
            })
        })
        .collect()
}

/// One run of the matrix and where it goes.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Assignment {
    /// The cell.
    pub cell: String,
    /// The profile.
    pub profile: String,
    /// The instance whose host serves the target, or `None` when no machine
    /// fits and the run is missing.
    pub target_instance: Option<String>,
    /// The instance whose host leases the Ubuntu client VM, for an Ubuntu
    /// client; a macOS client runs on the runner's own host.
    pub client_instance: Option<String>,
    /// Why the run is missing, when it is.
    pub missing: Option<String>,
}

/// Give each run of the matrix a machine that fits, or mark it missing.
/// Targets spread across the instances that fit, in turn; an Ubuntu client
/// comes from the runner's own instance when it offers `ubuntu2404`, else
/// from the first that does.
pub fn plan(matrix: &Matrix, found: &[Found]) -> Vec<Assignment> {
    let mut turn = 0usize;
    let mut assignments = Vec::new();
    for spec in &matrix.cells {
        for profile in &matrix.profiles {
            let fits: Vec<&Found> = found
                .iter()
                .filter(|f| match (&spec.target.image, spec.target.durable) {
                    (_, true) => f.durable.is_some(),
                    (Some(image), false) => f.images.iter().any(|i| i == image),
                    (None, false) => false,
                })
                .collect();
            let client = (spec.client == "ubuntu").then(|| {
                found
                    .iter()
                    .filter(|f| f.images.iter().any(|i| i == "ubuntu2404"))
                    .max_by_key(|f| f.local)
                    .map(|f| f.instance.clone())
            });
            let mut assignment = Assignment {
                cell: spec.cell.clone(),
                profile: profile.clone(),
                target_instance: None,
                client_instance: client.clone().flatten(),
                missing: None,
            };
            if fits.is_empty() {
                assignment.missing = Some(match (&spec.target.image, spec.target.durable) {
                    (_, true) => "no durable Mac is available".into(),
                    (Some(image), _) => format!("no instance offers {image} with room to acquire"),
                    (None, false) => "the cell names no target".into(),
                });
            } else if matches!(client, Some(None)) {
                assignment.missing =
                    Some("no instance offers ubuntu2404 for the Ubuntu client".into());
            } else {
                assignment.target_instance = Some(fits[turn % fits.len()].instance.clone());
                turn += 1;
            }
            assignments.push(assignment);
        }
    }
    assignments
}

/// What the runs under `dir` add up to: each assignment's verdicts from its
/// `report.json`, the missing runs, whether the acceptance is complete, and
/// the durable Macs' checklist of the permission prompts the suite approved
/// there (DC-340). The acceptance is complete only when every run ran and
/// every scenario passed or was skipped by its profile.
pub fn summary(dir: &Path, assignments: &[Assignment]) -> Value {
    let mut runs = Vec::new();
    let mut missing = Vec::new();
    let mut checklist: Vec<Value> = Vec::new();
    let mut complete = true;
    for a in assignments {
        if let Some(why) = &a.missing {
            complete = false;
            missing.push(json!({"cell": a.cell, "profile": a.profile, "why": why}));
            continue;
        }
        let path = dir.join(&a.cell).join(&a.profile).join("report.json");
        let report: Option<Value> = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok());
        let Some(report) = report else {
            complete = false;
            missing.push(
                json!({"cell": a.cell, "profile": a.profile, "why": "the run left no report"}),
            );
            continue;
        };
        let results = report["results"].as_array().cloned().unwrap_or_default();
        let verdicts: Vec<Value> = results
            .iter()
            .map(|r| json!({"id": r["id"], "result": r["verdict"]["result"]}))
            .collect();
        let passed = results.iter().all(|r| {
            matches!(
                r["verdict"]["result"].as_str(),
                Some("passed") | Some("skipped")
            )
        });
        complete &= passed && !results.is_empty();
        for r in &results {
            for (key, value) in r
                .get("evidence")
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
            {
                if key.ends_with("approved permission prompt")
                    && value.get("durable").and_then(Value::as_bool) == Some(true)
                {
                    checklist.push(json!({
                        "machine": value["machine"],
                        "program": value["program"],
                        "permission": value["permission"],
                        "app": value["app"],
                        "at": value["at"],
                        "cell": a.cell,
                        "profile": a.profile,
                        "scenario": r["id"],
                    }));
                }
            }
        }
        runs.push(json!({
            "cell": a.cell,
            "profile": a.profile,
            "target_instance": a.target_instance,
            "passed": passed,
            "verdicts": verdicts,
        }));
    }
    json!({
        "complete": complete,
        "runs": runs,
        "missing": missing,
        "durable_checklist": checklist,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list() -> Value {
        json!([
            {"instance": "vm-host-a", "address": "http://127.0.0.1:6240", "local": true, "reachable": true, "machines": [
                {"name": "macos26", "kind": "virtual", "image": "macos26", "state": "offer", "base_available": true, "concurrency": {"acquirable": true}},
                {"name": "ubuntu2404", "kind": "virtual", "image": "ubuntu2404", "state": "offer", "base_available": true, "concurrency": {"acquirable": true}}
            ]},
            {"instance": "vm-host-b", "address": "http://192.0.2.3:6240", "local": false, "reachable": true, "machines": [
                {"name": "macos26", "kind": "virtual", "image": "macos26", "state": "offer", "base_available": true, "concurrency": {"acquirable": false}}
            ]},
            {"instance": "durable-1", "address": "http://192.0.2.4:6240", "local": false, "reachable": true, "machines": [
                {"name": "durable-1", "kind": "durable", "state": "available"}
            ]},
            {"instance": "durable-2", "address": "http://192.0.2.5:6240", "local": false, "reachable": false, "machines": [
                {"name": "durable-2", "kind": "durable", "state": "available"}
            ]}
        ])
    }

    #[test]
    fn the_matrix_in_the_repository_is_six_cells_under_four_profiles_and_names_no_host() {
        let matrix = Matrix::builtin();
        assert_eq!(matrix.cells.len() * matrix.profiles.len(), 24);
        assert!(!MATRIX.contains("192.168.") && !MATRIX.contains("Mac-mini"));
    }

    #[test]
    fn discovery_keeps_reachable_instances_with_room_and_honours_exclusions() {
        let found = discover(&list(), &[]);
        let names: Vec<&str> = found.iter().map(|f| f.instance.as_str()).collect();
        assert_eq!(names, ["vm-host-a", "vm-host-b", "durable-1"]);
        assert!(
            found[1].images.is_empty(),
            "an image without room to acquire is not offered"
        );
        assert_eq!(found[2].durable.as_deref(), Some("durable-1"));
        let excluded = discover(&list(), &["vm-host-a".into()]);
        assert!(excluded.iter().all(|f| f.instance != "vm-host-a"));
    }

    #[test]
    fn every_run_gets_a_machine_or_is_missing_with_its_reason() {
        let matrix = Matrix::builtin();
        let all = plan(&matrix, &discover(&list(), &[]));
        assert_eq!(all.len(), 24);
        assert!(all.iter().all(|a| a.missing.is_none()), "{all:?}");
        assert!(all
            .iter()
            .filter(|a| a.cell.starts_with("ubuntu-client"))
            .all(|a| a.client_instance.as_deref() == Some("vm-host-a")));
        let without_durable = plan(&matrix, &discover(&list(), &["durable-1".into()]));
        let missing: Vec<_> = without_durable
            .iter()
            .filter(|a| a.missing.is_some())
            .collect();
        assert_eq!(missing.len(), 8);
        assert!(missing.iter().all(|a| a.cell.ends_with("durable-mac")));
    }

    #[test]
    fn a_summary_with_a_missing_run_is_incomplete_and_lists_durable_approvals() {
        let dir = tempfile::tempdir().unwrap();
        let assignments = vec![
            Assignment {
                cell: "macos-client.durable-mac".into(),
                profile: "N0".into(),
                target_instance: Some("durable-1".into()),
                client_instance: None,
                missing: None,
            },
            Assignment {
                cell: "macos-client.durable-mac".into(),
                profile: "N1".into(),
                target_instance: None,
                client_instance: None,
                missing: Some("no durable Mac is available".into()),
            },
        ];
        let run = dir.path().join("macos-client.durable-mac").join("N0");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(run.join("report.json"), json!({"results": [{"id": "AC-55", "verdict": {"result": "passed"}, "evidence": {
            "approved permission prompt": {"machine": "durable-1", "durable": true, "program": "node", "permission": "Apple Events (Automation)", "app": "Finder", "at": 1}
        }}]}).to_string()).unwrap();
        let summary = summary(dir.path(), &assignments);
        assert_eq!(summary["complete"], false);
        assert_eq!(summary["missing"].as_array().unwrap().len(), 1);
        assert_eq!(summary["durable_checklist"][0]["machine"], "durable-1");
        assert_eq!(summary["durable_checklist"][0]["program"], "node");
    }
}
