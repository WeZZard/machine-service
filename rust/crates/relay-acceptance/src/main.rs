//! `relay-acceptance`: run the relay acceptance suite in one cell under one
//! profile, or summarize the runs under an evidence folder.
//!
//! ```text
//! relay-acceptance run --cell macos-client.macos-vm --profile N1 \
//!     --instance http://<host name>:<port> --image macos26 \
//!     [--host-shell "ssh <host>"] [--host-script <path there>] [--agent-rig manage|external] \
//!     [--seed <n>] [--only AC-55,AC-56] [--purpose <text>] --evidence <dir>
//! relay-acceptance summary <dir>
//! ```
//!
//! Every name comes from the operator: the instance is named, never
//! addressed, and the host shell is whatever reaches the target's host. The
//! instance must be a second instance, which its `/health` says; the
//! installed instance is refused (DC-245). The
//! run's report is `<dir>/<cell>/<profile>/report.json`; the exit status is
//! 1 when a scenario failed.

use std::path::{Path, PathBuf};

use network_rig::cell::{run_cell, Cell, Verdict};
use network_rig::Profile;
use relay_acceptance::scenarios;
use relay_acceptance::target::{HostControl, RealTarget, TargetSettings};
use serde_json::Value;

fn usage() -> ! {
    eprintln!(
        "usage: relay-acceptance run --cell <cell> --profile <N0..N3> --instance <http://name:port> \
[--image <image>] [--host-shell <command>] [--host-script <path>] [--agent-rig manage|external] \
[--seed <n>] [--only <ids>] [--purpose <text>] --evidence <dir>\n       relay-acceptance summary <dir>"
    );
    std::process::exit(2)
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == &format!("--{name}"))
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn need(args: &[String], name: &str) -> String {
    flag(args, name).unwrap_or_else(|| {
        eprintln!("relay-acceptance: --{name} is required");
        usage()
    })
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("run") => std::process::exit(run(&args[1..])),
        Some("summary") => summary(Path::new(args.get(1).unwrap_or_else(|| usage()))),
        _ => usage(),
    }
}

fn run(args: &[String]) -> i32 {
    let cell = Cell::parse(&need(args, "cell")).unwrap_or_else(|| {
        eprintln!("relay-acceptance: unknown cell; the cells are:");
        for cell in Cell::all() {
            eprintln!("  {}", cell.id());
        }
        std::process::exit(2)
    });
    let profile = Profile::builtin(&need(args, "profile")).unwrap_or_else(|e| {
        eprintln!("relay-acceptance: {e}");
        std::process::exit(2)
    });
    let seed = flag(args, "seed")
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(network_rig::rig::seed_from_env);
    let host = flag(args, "host-shell").map(|shell| HostControl {
        shell: shell.split_whitespace().map(str::to_string).collect(),
        script: flag(args, "host-script").unwrap_or_else(|| "relay-acceptance-host.sh".into()),
    });
    let only: Vec<String> = flag(args, "only")
        .map(|s| s.split(',').map(|id| id.trim().to_string()).collect())
        .unwrap_or_default();
    let settings = TargetSettings {
        instance: need(args, "instance"),
        image: flag(args, "image"),
        profile: profile.id.clone(),
        seed,
        manage_agent_rig: flag(args, "agent-rig").as_deref() != Some("external"),
        host,
        // A purpose is leased once per image, so each cell and profile has
        // its own, in the characters a purpose takes.
        purpose: flag(args, "purpose").unwrap_or_else(|| {
            format!("acc-{}-{}", cell.id(), profile.id)
                .to_ascii_lowercase()
                .replace('.', "-")
        }),
    };
    let evidence = PathBuf::from(need(args, "evidence"));
    let mut target = RealTarget::new(settings);
    let suite = scenarios::select(&only);
    eprintln!(
        "relay-acceptance: {} under {} with seed {seed}: {} scenarios",
        cell.id(),
        profile.id,
        suite.len()
    );
    match run_cell(cell, &profile, seed, &mut target, &suite, &evidence) {
        Ok(report) => {
            for result in &report.results {
                println!(
                    "{} {}: {}",
                    report.cell,
                    result.id,
                    verdict_text(&result.verdict)
                );
            }
            println!("report: {}", report.run_dir.join("report.json").display());
            if report.passed() {
                0
            } else {
                1
            }
        }
        Err(error) => {
            eprintln!("relay-acceptance: the cell did not start: {error}");
            1
        }
    }
}

fn verdict_text(verdict: &Verdict) -> String {
    match verdict {
        Verdict::Passed => "passed".into(),
        Verdict::Failed(why) => format!("FAILED: {why}"),
        Verdict::Skipped(why) => format!("skipped: {why}"),
        Verdict::Blocked(why) => format!("blocked: {why}"),
    }
}

/// Print one line per cell and profile: each scenario's result.
fn summary(dir: &Path) {
    let mut rows = Vec::new();
    for cell in Cell::all() {
        for profile in ["N0", "N1", "N2", "N3"] {
            let path = dir.join(cell.id()).join(profile).join("report.json");
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(report) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let results = report["results"].as_array().cloned().unwrap_or_default();
            let cells: Vec<String> = results
                .iter()
                .map(|r| {
                    let mark = match r["verdict"]["result"].as_str().unwrap_or("?") {
                        "passed" => "pass",
                        "failed" => "FAIL",
                        "skipped" => "-",
                        "blocked" => "blocked",
                        other => other,
                    };
                    format!("{} {mark}", r["id"].as_str().unwrap_or("?"))
                })
                .collect();
            rows.push(format!("{} {profile}: {}", cell.id(), cells.join(", ")));
        }
    }
    for row in rows {
        println!("{row}");
    }
}
