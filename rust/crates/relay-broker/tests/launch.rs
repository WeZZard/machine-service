//! TL-2: a target's launcher starts every command its clients send
//! (DC-1xx-LAUNCH-1), with the broker, real remote sessions on loopback, a
//! real relay agent core and a test client of the relay protocol.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use relay_agent::protocol::{ExecutionState, Op, OpKind, Outcome};
use relay_broker::{Launcher, Upload};

const WAIT: Duration = Duration::from_secs(20);

/// A launcher that prefixes each command's output with a marker, so the
/// test sees that the agent ran the launched form, and refuses while told
/// to.
#[derive(Default)]
struct Marking {
    launched: Mutex<Vec<(String, String, OpKind)>>,
    settled: Mutex<Vec<(String, String)>>,
    refuse: Mutex<bool>,
}

impl Launcher for Marking {
    fn launch(&self, lease: &str, execution: &str, op: &Op) -> Result<Op, String> {
        if *self.refuse.lock().unwrap() {
            return Err("the lease is not current".into());
        }
        self.launched
            .lock()
            .unwrap()
            .push((lease.into(), execution.into(), op.kind()));
        Ok(match op {
            Op::Exec {
                command,
                stdin,
                timeout_s,
            } => Op::Exec {
                command: format!("printf 'launched:'; {command}"),
                stdin: *stdin,
                timeout_s: *timeout_s,
            },
            Op::Script { timeout_s } => Op::Exec {
                command: "printf 'launched-script:'; exec /bin/sh -s".into(),
                stdin: true,
                timeout_s: *timeout_s,
            },
            other => panic!("a launcher is never asked for {other:?}"),
        })
    }

    fn settled(&self, lease: &str, execution: &str) {
        self.settled
            .lock()
            .unwrap()
            .push((lease.into(), execution.into()));
    }
}

fn exit_rc(outcome: &Outcome) -> Option<i32> {
    match outcome {
        Outcome::Exit { rc, .. } => *rc,
        other => panic!("not an exit: {other:?}"),
    }
}

/// A client's exec and script run in the launched form, once per execution
/// record: a repeated ID is answered from the record without a second
/// launch, a query names the client's own operation, and the settled record
/// is reported to the launcher. Calls made in the broker's own process and
/// operations other than commands are handed on as they are.
#[test]
fn a_clients_commands_start_through_the_targets_launcher() {
    let h = Harness::start(Options::default());
    let a = lease(&h.broker, "vm-a");
    h.broker.bind_lease(&a.target, "lease-1", false);
    let launcher = Arc::new(Marking::default());
    h.broker.launch_through(&a.target, launcher.clone());
    let agent = TestAgent::start(&h.url("ws"), &a.agent, None);
    assert!(h.broker.wait_for_agent(&a.target, WAIT).is_some());
    let client = TestClient::connect(&h.url("ws"), &a.client).unwrap();

    client.call(1, "x-1", exec("echo hi"), b"");
    let answer = client.wait(1, WAIT);
    assert_eq!(exit_rc(&answer.result.as_ref().unwrap().outcome), Some(0));
    assert_eq!(answer.bytes, b"launched:hi\n");
    assert!(answer.end_matches());

    client.call(
        2,
        "x-2",
        Op::Script { timeout_s: 60.0 },
        b"echo from-script\n",
    );
    let answer = client.wait(2, WAIT);
    assert_eq!(answer.bytes, b"launched-script:from-script\n");

    // The same IDs again: replayed from the record, never launched again.
    client.call(3, "x-1", exec("echo hi"), b"");
    let again = client.wait(3, WAIT);
    assert!(again.result.unwrap().replayed);
    assert_eq!(again.bytes, b"launched:hi\n");
    client.query(4, "x-2");
    match client.wait(4, WAIT).query.unwrap() {
        ExecutionState::Completed { op, .. } => assert_eq!(op, OpKind::Script),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        *launcher.launched.lock().unwrap(),
        vec![
            ("lease-1".to_string(), "x-1".to_string(), OpKind::Exec),
            ("lease-1".to_string(), "x-2".to_string(), OpKind::Script),
        ]
    );
    assert!(wait_until(WAIT, || launcher.settled.lock().unwrap().len() == 2));
    assert_eq!(
        *launcher.settled.lock().unwrap(),
        vec![
            ("lease-1".to_string(), "x-1".to_string()),
            ("lease-1".to_string(), "x-2".to_string()),
        ]
    );

    // machine-service's own calls, and a client's file operations, are not
    // launched.
    let own = h
        .broker
        .run(&a.target, "own-1", exec("echo own"), Upload::None, WAIT)
        .unwrap();
    assert_eq!(own.bytes, b"own\n");
    client.call(
        5,
        "w-1",
        Op::Write {
            path: "f.txt".into(),
            name: String::new(),
            mode: None,
        },
        b"file",
    );
    let written = client.wait(5, WAIT);
    assert!(matches!(
        written.result.unwrap().outcome,
        Outcome::Written(_)
    ));
    assert_eq!(std::fs::read(agent.home.join("f.txt")).unwrap(), b"file");
    assert_eq!(launcher.launched.lock().unwrap().len(), 2);
    drop(agent);
}

/// A launch that fails refuses the call with `launch_failed` before
/// anything ran, keeps no record, so the same ID may be sent again, and
/// drops the upload that follows it.
#[test]
fn a_failed_launch_refuses_the_call_uncommitted() {
    let h = Harness::start(Options::default());
    let a = lease(&h.broker, "vm-a");
    let launcher = Arc::new(Marking::default());
    *launcher.refuse.lock().unwrap() = true;
    h.broker.launch_through(&a.target, launcher.clone());
    let agent = TestAgent::start(&h.url("ws"), &a.agent, None);
    assert!(h.broker.wait_for_agent(&a.target, WAIT).is_some());
    let client = TestClient::connect(&h.url("ws"), &a.client).unwrap();

    client.call(1, "x-1", Op::Script { timeout_s: 60.0 }, b"echo never\n");
    let refused = client.wait(1, WAIT);
    match refused.result.unwrap().outcome {
        Outcome::Failed {
            code,
            message,
            committed,
        } => {
            assert_eq!(code, "launch_failed");
            assert!(message.contains("not current"), "{message}");
            assert!(!committed);
        }
        other => panic!("{other:?}"),
    }
    assert!(refused.bytes.is_empty());
    client.query(2, "x-1");
    assert_eq!(client.wait(2, WAIT).query, Some(ExecutionState::Unknown));
    assert!(launcher.settled.lock().unwrap().is_empty());

    *launcher.refuse.lock().unwrap() = false;
    client.call(3, "x-1", Op::Script { timeout_s: 60.0 }, b"echo now\n");
    let ran = client.wait(3, WAIT);
    assert_eq!(ran.bytes, b"launched-script:now\n");
    assert!(!ran.result.unwrap().replayed);
    drop(agent);
}
