//! TL-2: at-most-once through the broker while both legs, client and relay
//! agent, lose connections at random through a fault proxy, and the agent's
//! session is ended outright now and then (BK-3, RP-7, RP-8).

mod common;
#[path = "../../remote-session/tests/support/mod.rs"]
mod support;

use std::time::{Duration, Instant};

use common::*;
use relay_agent::protocol::Outcome;
use support::{FaultProxy, Mode};

#[test]
fn every_execution_runs_once_while_both_legs_break() {
    let h = Harness::start(Options::default());
    let a = lease(&h.broker, "vm-a");
    let agent_leg = FaultProxy::start(h.addr, Mode::Tcp);
    let client_leg = FaultProxy::start(h.addr, Mode::Tcp);
    let agent = TestAgent::start(&agent_leg.url("ws"), &a.agent, None);
    assert!(h
        .broker
        .wait_for_agent(&a.target, Duration::from_secs(20))
        .is_some());
    let client = TestClient::connect(&client_leg.url("ws"), &a.client).unwrap();

    // A seeded schedule, so a failure replays.
    let seed: u64 = std::env::var("RELAY_BROKER_FAULT_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x5eed);
    eprintln!("fault seed {seed}");
    let mut state = seed | 1;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let count = 40u64;
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let faults = {
        let stop = stop.clone();
        let broker = h.broker.clone();
        let target = a.target.clone();
        let schedule: Vec<u64> = (0..200).map(|_| next()).collect();
        std::thread::spawn(move || {
            let mut i = 0;
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                let roll = schedule[i % schedule.len()];
                i += 1;
                std::thread::sleep(Duration::from_millis(50 + roll % 150));
                match roll % 5 {
                    0 | 1 => client_leg.reset(),
                    2 | 3 => agent_leg.reset(),
                    _ => {
                        broker.end_agent_session(&target);
                    }
                }
            }
        })
    };

    for n in 1..=count {
        let command = format!("echo {n} >> ledger; sleep 0.0{}; echo {n}", n % 10);
        client.call(n, &format!("run-{n}"), exec(&command), b"");
    }
    let deadline = Instant::now() + Duration::from_secs(120);
    for n in 1..=count {
        let answer = client.wait(n, deadline.saturating_duration_since(Instant::now()));
        let result = answer.result.clone().unwrap();
        match &result.outcome {
            Outcome::Exit { rc: Some(0), .. } => {}
            other => panic!("run-{n}: {other:?}"),
        }
        assert_eq!(answer.bytes, format!("{n}\n").into_bytes(), "run-{n}");
        assert!(answer.end_matches(), "run-{n}");
    }
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    faults.join().unwrap();

    let ledger = std::fs::read_to_string(agent.home.join("ledger")).unwrap();
    let mut lines: Vec<u64> = ledger.lines().map(|l| l.parse().unwrap()).collect();
    lines.sort();
    assert_eq!(
        lines,
        (1..=count).collect::<Vec<_>>(),
        "each ran exactly once"
    );
}
