//! TL-7, opt-in: one cell under one profile against a live instance, as
//! the `relay-acceptance` binary runs it. Every name comes from the
//! environment, never from the repository:
//!
//! ```sh
//! RELAY_ACCEPTANCE_CELL=macos-client.macos-vm RELAY_ACCEPTANCE_PROFILE=N0 \
//! RELAY_ACCEPTANCE_INSTANCE=http://<host name>:<port> RELAY_ACCEPTANCE_IMAGE=macos26 \
//! RELAY_ACCEPTANCE_HOST_SHELL="ssh <host>" RELAY_ACCEPTANCE_HOST_SCRIPT=<path there> \
//! RELAY_ACCEPTANCE_EVIDENCE=<dir> \
//!   cargo test -p relay-acceptance --test live -- --ignored --nocapture
//! ```

use std::path::PathBuf;

use network_rig::cell::{run_cell, Cell, Verdict};
use network_rig::Profile;
use relay_acceptance::mcp::McpSettings;
use relay_acceptance::scenarios;
use relay_acceptance::target::{HostControl, RealTarget, TargetSettings};

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

#[test]
#[ignore = "TL-7: needs a machine-service instance with the relay link and a lease; see the module documentation"]
fn one_cell_under_one_profile_passes() {
    let need = |name: &str| var(name).unwrap_or_else(|| panic!("set {name}"));
    let cell = Cell::parse(&need("RELAY_ACCEPTANCE_CELL")).expect("a known cell");
    let profile = Profile::builtin(&need("RELAY_ACCEPTANCE_PROFILE")).unwrap();
    let seed = network_rig::rig::seed_from_env();
    let settings = TargetSettings {
        instance: need("RELAY_ACCEPTANCE_INSTANCE"),
        image: var("RELAY_ACCEPTANCE_IMAGE"),
        profile: profile.id.clone(),
        seed,
        manage_agent_rig: var("RELAY_ACCEPTANCE_AGENT_RIG").as_deref() != Some("external"),
        host: var("RELAY_ACCEPTANCE_HOST_SHELL").map(|shell| HostControl {
            shell: shell.split_whitespace().map(str::to_string).collect(),
            script: var("RELAY_ACCEPTANCE_HOST_SCRIPT")
                .unwrap_or_else(|| "relay-acceptance-host.sh".into()),
        }),
        purpose: "relay-acceptance".into(),
        // The cell's client is mcp-relay when its server is named (TL-7).
        mcp: var("RELAY_ACCEPTANCE_MCP_SERVER").map(|server| {
            let run = PathBuf::from(need("RELAY_ACCEPTANCE_EVIDENCE"))
                .join(cell.id())
                .join(&profile.id);
            McpSettings {
                node: PathBuf::from(var("RELAY_ACCEPTANCE_NODE").unwrap_or_else(|| "node".into())),
                server: PathBuf::from(server),
                project: run.join("mcp-project"),
                env: vec![(
                    "MCP_RELAY_STATE_DIR".into(),
                    run.join("mcp-state").display().to_string(),
                )],
                log: run.join("mcp-relay.log"),
            }
        }),
    };
    let only: Vec<String> = var("RELAY_ACCEPTANCE_ONLY")
        .map(|s| s.split(',').map(|id| id.trim().to_string()).collect())
        .unwrap_or_default();
    let evidence = PathBuf::from(need("RELAY_ACCEPTANCE_EVIDENCE"));
    let mut target = RealTarget::new(settings);
    let report = run_cell(
        cell,
        &profile,
        seed,
        &mut target,
        &scenarios::select(&only),
        &evidence,
    )
    .expect("the cell starts");
    for result in &report.results {
        eprintln!("{} {}: {:?}", report.cell, result.id, result.verdict);
    }
    let failed: Vec<&str> = report
        .results
        .iter()
        .filter(|r| matches!(r.verdict, Verdict::Failed(_)))
        .map(|r| r.id.as_str())
        .collect();
    assert!(
        failed.is_empty(),
        "failed: {failed:?}; see {}",
        report.run_dir.display()
    );
}
